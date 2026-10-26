use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::process::Child;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::EmulatorConfig;
use crate::emulator::{Emulator, LaunchContext, SaveLayout};
use crate::romm::RommClient;
use crate::sync::{self, SyncCtx, SyncStore};

/// How long a game gets to close after a stop request before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(5);
const TREE_POLL: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub struct GameProcess {
    pub rom_id: i64,
    pub emulator: String,
    /// Emulator-specific game identifier used for profile snapshots and status display.
    /// For DuckStation this is the disc serial; other emulators may leave it None.
    pub game_id: Option<String>,
    pub rom_path: PathBuf,
    pub started_at: SystemTime,
    pub launch_ctx: LaunchContext,
    /// Where the game's saves were found at launch.
    pub save_layout: SaveLayout,
    /// Sync-state keys of the saves that existed locally at launch; anything
    /// else found after exit was created during the session.
    pub present_at_launch: HashSet<String>,
    pub tree: Arc<ProcessTree>,
}

#[derive(Debug, Clone)]
pub struct RunningGame {
    pub pid: u32,
    pub rom_id: i64,
    pub emulator: String,
    pub game_id: Option<String>,
    pub rom_path: PathBuf,
    pub started_at: SystemTime,
}

#[derive(Clone)]
pub struct ProcessManager {
    inner: Arc<Mutex<HashMap<u32, GameProcess>>>,
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn add(&self, pid: u32, process: GameProcess) {
        self.inner.lock().await.insert(pid, process);
    }

    pub async fn remove(&self, pid: u32) -> Option<GameProcess> {
        self.inner.lock().await.remove(&pid)
    }

    pub async fn list(&self) -> Vec<RunningGame> {
        self.inner
            .lock()
            .await
            .iter()
            .map(|(pid, p)| RunningGame {
                pid: *pid,
                rom_id: p.rom_id,
                emulator: p.emulator.clone(),
                game_id: p.game_id.clone(),
                rom_path: p.rom_path.clone(),
                started_at: p.started_at,
            })
            .collect()
    }

    /// Return the game_id of the first running game on the given emulator.
    pub async fn game_id_for_emulator(&self, emulator: &str) -> Option<String> {
        self.inner
            .lock()
            .await
            .values()
            .find(|p| p.emulator == emulator)
            .and_then(|p| p.game_id.clone())
    }

    /// Stop a game rommd launched. Returns false when no game has this PID.
    pub async fn stop(&self, pid: u32) -> bool {
        let tree = self.inner.lock().await.get(&pid).map(|g| g.tree.clone());
        match tree {
            Some(tree) => {
                tree.stop().await;
                true
            }
            None => false,
        }
    }
}

/// Spawn a background task that waits for the game to exit, then syncs
/// saves/states to RomM and cleans up any launch artifacts. `child` is `None`
/// for an attached launch, whose emulator is another process's child; its
/// process tree is all there is to wait for.
pub fn monitor(
    pid: u32,
    child: Option<Child>,
    manager: ProcessManager,
    romm: Option<Arc<RommClient>>,
    emulator: Arc<dyn Emulator>,
    emu_config: EmulatorConfig,
    store: Arc<SyncStore>,
) {
    tokio::spawn(async move {
        let mut child = child;
        let tree = manager.inner.lock().await.get(&pid).map(|g| g.tree.clone());
        match &mut child {
            Some(child) => {
                match child.wait().await {
                    Ok(status) => info!("game pid={} exited: {}", pid, status),
                    Err(e) => warn!("error waiting for pid={}: {}", pid, e),
                }
                if let Some(tree) = tree {
                    tree.wait().await;
                }
            }
            None => {
                if let Some(tree) = tree {
                    tree.wait_until_gone().await;
                }
                info!("game pid={} exited", pid);
            }
        }

        if let Some(game) = manager.remove(pid).await {
            game.launch_ctx.clean_up().await;

            if let Some(romm) = romm {
                let ctx = SyncCtx {
                    romm: &romm,
                    store: &store,
                    rom_id: game.rom_id,
                    emulator: emulator.as_ref(),
                };
                // Resolve the layout again: the session may have created what
                // was missing at launch (Ryujinx's save slot, DuckStation's
                // title cache entry that names memory cards).
                let layout = emulator
                    .save_layout(&emu_config, &game.rom_path, game.game_id.as_deref())
                    .await
                    .or(game.save_layout.clone());
                sync::sync_layout(&ctx, &layout, &game.rom_path, Some(&game.present_at_launch))
                    .await;
            }
        }
        // `child` lives until here, so its PID cannot be reused while the game
        // is listed and a stop request could still reach it.
        drop(child);
    });
}

/// Every process of one game: the emulator and whatever it starts. Launchers
/// can exit before the emulator does (a uruntime AppImage parent that was
/// killed), and syncing then would read saves the emulator may still write.
///
/// On Unix this is the process group `LaunchCommand::spawn` makes the
/// emulator lead. On Windows it is a job object the emulator joins right after it is
/// spawned; the job does not kill its processes when rommd exits, so games
/// keep running across a rommd restart.
#[derive(Debug)]
pub struct ProcessTree {
    pid: u32,
    #[cfg(windows)]
    job: Option<job::Job>,
}

impl ProcessTree {
    pub fn new(child: &Child) -> Option<Self> {
        let pid = child.id()?;
        #[cfg(windows)]
        let job = match child.raw_handle().map(job::Job::with_process) {
            Some(Ok(job)) => Some(job),
            Some(Err(e)) => {
                warn!(
                    "cannot track the processes of pid={} ({}); only it is waited for and stopped",
                    pid, e
                );
                None
            }
            None => None,
        };
        Some(Self {
            pid,
            #[cfg(windows)]
            job,
        })
    }

    /// The tree of a game another process started (`rommd play --attach`).
    /// On Unix `pid` must lead its process group, as `LaunchCommand::spawn`
    /// makes it, or the group could be the starter's own. On Windows the
    /// process joins a job here, so whatever it started before is not
    /// tracked; its starter's job (if any) still is.
    pub fn attach(pid: u32) -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            let p = nix::unistd::Pid::from_raw(pid as i32);
            let group = nix::unistd::getpgid(Some(p))
                .map_err(|e| anyhow::anyhow!("no process {pid} to track: {e}"))?;
            anyhow::ensure!(
                group == p,
                "process {pid} does not lead its own process group"
            );
            Ok(Self { pid })
        }
        #[cfg(windows)]
        {
            let job = job::Job::with_pid(pid)
                .map_err(|e| anyhow::anyhow!("cannot track process {pid}: {e}"))?;
            Ok(Self {
                pid,
                job: Some(job),
            })
        }
    }

    /// Wait until no process of the tree is left, once its first process
    /// has exited.
    pub async fn wait(&self) {
        let mut logged = false;
        while self.alive() {
            if !logged {
                info!(
                    "game pid={} exited; waiting for the processes it started",
                    self.pid
                );
                logged = true;
            }
            tokio::time::sleep(TREE_POLL).await;
        }
    }

    /// Wait until no process of the tree is left.
    pub async fn wait_until_gone(&self) {
        while self.alive() {
            tokio::time::sleep(TREE_POLL).await;
        }
    }

    #[cfg(unix)]
    fn alive(&self) -> bool {
        nix::sys::signal::killpg(self.pgid(), None).is_ok()
    }

    #[cfg(windows)]
    fn alive(&self) -> bool {
        self.job.as_ref().is_some_and(|j| j.active_processes() > 0)
    }

    #[cfg(unix)]
    fn pgid(&self) -> nix::unistd::Pid {
        nix::unistd::Pid::from_raw(self.pid as i32)
    }

    /// Ask the game to close, like closing its window, and kill it if it is
    /// still running after `STOP_GRACE`: some emulators show a confirmation
    /// popup (DuckStation) or ignore the request while a game runs (Eden).
    pub async fn stop(self: Arc<Self>) {
        self.request_close().await;
        tokio::spawn(async move {
            tokio::time::sleep(STOP_GRACE).await;
            if self.alive() {
                self.kill().await;
            }
        });
    }

    #[cfg(unix)]
    async fn request_close(&self) {
        use nix::sys::signal::{Signal, killpg};
        if let Err(e) = killpg(self.pgid(), Signal::SIGTERM) {
            warn!("could not signal pid={}: {}", self.pid, e);
        }
    }

    #[cfg(unix)]
    async fn kill(&self) {
        use nix::sys::signal::{Signal, killpg};
        let _ = killpg(self.pgid(), Signal::SIGKILL);
    }

    /// taskkill without /F posts WM_CLOSE to the processes' windows.
    #[cfg(windows)]
    async fn request_close(&self) {
        let pids = match &self.job {
            Some(job) => job.process_ids(),
            None => vec![self.pid],
        };
        let mut cmd = tokio::process::Command::new("taskkill");
        for pid in pids {
            cmd.arg("/PID").arg(pid.to_string());
        }
        let _ = cmd.output().await;
    }

    #[cfg(windows)]
    async fn kill(&self) {
        match &self.job {
            Some(job) => job.terminate(),
            None => {
                let _ = tokio::process::Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &self.pid.to_string()])
                    .output()
                    .await;
            }
        }
    }
}

#[cfg(windows)]
mod job {
    use std::os::windows::io::RawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_BASIC_PROCESS_ID_LIST, JobObjectBasicAccountingInformation,
        JobObjectBasicProcessIdList, QueryInformationJobObject, TerminateJobObject,
    };

    /// An anonymous job object. Closing it leaves its processes running.
    #[derive(Debug)]
    pub struct Job(HANDLE);

    // SAFETY: a job handle may be used from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn with_process(process: RawHandle) -> std::io::Result<Self> {
            // SAFETY: null attributes and name create an unnamed job with
            // default security; the handle is owned by the returned Job.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let job = Self(handle);
            // SAFETY: both handles are valid; the process handle is borrowed.
            if unsafe { AssignProcessToJobObject(job.0, process as HANDLE) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(job)
        }

        /// The job of a process that is already running, found by its ID.
        pub fn with_pid(pid: u32) -> std::io::Result<Self> {
            use windows_sys::Win32::System::Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
                PROCESS_TERMINATE,
            };
            // SAFETY: plain Win32 call; a null result is an error.
            let process = unsafe {
                OpenProcess(
                    PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                    0,
                    pid,
                )
            };
            if process.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let job = Self::with_process(process as RawHandle);
            // SAFETY: the handle was opened above and is closed once; the job
            // keeps its own reference to the process.
            unsafe { CloseHandle(process) };
            job
        }

        pub fn active_processes(&self) -> u32 {
            // SAFETY: plain-old-data struct, zero is a valid value.
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: the buffer is the struct this information class fills.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    (&raw mut info).cast(),
                    size_of_val(&info) as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 { 0 } else { info.ActiveProcesses }
        }

        pub fn process_ids(&self) -> Vec<u32> {
            const MAX: usize = 64;
            #[repr(C)]
            struct List {
                header: JOBOBJECT_BASIC_PROCESS_ID_LIST,
                more: [usize; MAX - 1],
            }
            // SAFETY: plain-old-data struct, zero is a valid value.
            let mut list: List = unsafe { std::mem::zeroed() };
            // SAFETY: the buffer is a process ID list header followed by room
            // for MAX IDs in total, and its size is passed along.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.0,
                    JobObjectBasicProcessIdList,
                    (&raw mut list).cast(),
                    size_of_val(&list) as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Vec::new();
            }
            let count = (list.header.NumberOfProcessIdsInList as usize).min(MAX);
            std::iter::once(list.header.ProcessIdList[0])
                .chain(list.more)
                .take(count)
                .map(|id| id as u32)
                .collect()
        }

        pub fn terminate(&self) {
            // SAFETY: the handle is valid for the lifetime of self.
            unsafe { TerminateJobObject(self.0, 1) };
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle is owned and closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::windows::io::AsRawHandle;

        #[test]
        fn tracks_processes_started_by_the_first() {
            // cmd waits a second, so it is in the job before it starts a
            // second process that outlives it.
            let mut child = std::process::Command::new("cmd")
                .args([
                    "/C",
                    "ping -n 2 127.0.0.1 >NUL & start /B ping -n 30 127.0.0.1 >NUL",
                ])
                .spawn()
                .unwrap();
            let job = Job::with_process(child.as_raw_handle()).unwrap();
            child.wait().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert!(
                job.active_processes() >= 1,
                "ping outlives cmd inside the job"
            );
            assert!(!job.process_ids().is_empty());
            job.terminate();
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert_eq!(job.active_processes(), 0);
        }
    }
}
