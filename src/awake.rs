//! Keeping the computer from idle sleep while a ROM downloads. Only idle
//! sleep: a deliberate one (power button, closing the lid) still happens, and
//! the download resumes afterwards.
//!
//! Linux holds a logind idle inhibitor through `systemd-inhibit`, macOS an
//! idle-sleep assertion through `caffeinate`; both are child processes that
//! also end when rommd does, so a crash can't leave the computer awake.
//! Windows sets the execution state on a thread of its own, since the state
//! belongs to the thread that set it. Where none of this is available the
//! download simply runs without it.

use tracing::{debug, info};

/// Held while the computer must not idle-sleep; released on drop.
pub struct KeepAwake {
    #[cfg(unix)]
    child: std::process::Child,
    #[cfg(windows)]
    release: std::sync::mpsc::Sender<()>,
}

impl KeepAwake {
    /// `None` when this system offers no way to do it.
    pub fn start(why: &str) -> Option<Self> {
        let awake = Self::platform_start(why);
        match &awake {
            Some(_) => info!("keeping the computer from idle sleep: {why}"),
            None => debug!("can't keep the computer from idle sleep here ({why})"),
        }
        awake
    }

    #[cfg(target_os = "linux")]
    fn platform_start(why: &str) -> Option<Self> {
        let pid = std::process::id().to_string();
        let child = std::process::Command::new("systemd-inhibit")
            .args(["--what=idle", "--who=rommd", "--mode=block"])
            .arg(format!("--why={why}"))
            .args(["tail", "--pid", &pid, "-f", "/dev/null"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        Self::running(child)
    }

    #[cfg(target_os = "macos")]
    fn platform_start(_why: &str) -> Option<Self> {
        let pid = std::process::id().to_string();
        let child = std::process::Command::new("caffeinate")
            .args(["-i", "-w", &pid])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        Self::running(child)
    }

    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    fn platform_start(_why: &str) -> Option<Self> {
        None
    }

    /// A helper that fails (no logind session, say) exits at once.
    #[cfg(unix)]
    fn running(mut child: std::process::Child) -> Option<Self> {
        std::thread::sleep(std::time::Duration::from_millis(50));
        match child.try_wait() {
            Ok(None) => Some(Self { child }),
            _ => None,
        }
    }

    #[cfg(windows)]
    fn platform_start(_why: &str) -> Option<Self> {
        const ES_CONTINUOUS: u32 = 0x8000_0000;
        const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn SetThreadExecutionState(flags: u32) -> u32;
        }
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (started, set) = std::sync::mpsc::channel::<bool>();
        std::thread::spawn(move || {
            // SAFETY: plain Win32 call taking flags by value.
            let ok = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) } != 0;
            let _ = started.send(ok);
            if ok {
                let _ = released.recv();
                // SAFETY: as above; clears this thread's request.
                unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
            }
        });
        set.recv().ok().filter(|ok| *ok).map(|_| Self { release })
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        #[cfg(windows)]
        {
            let _ = self.release.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn holds_an_idle_sleep_assertion_until_dropped() {
        let awake = KeepAwake::start("a test").expect("caffeinate is part of macOS");
        let pid = awake.child.id();
        let alive = |pid: u32| {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .is_ok_and(|s| s.success())
        };
        assert!(alive(pid));
        drop(awake);
        assert!(!alive(pid), "released on drop");
    }

    #[test]
    fn starting_and_dropping_never_fails() {
        // CI runners may have no session to inhibit; that's `None`, not an error.
        drop(KeepAwake::start("a test"));
    }
}
