//! `rommd service install|uninstall`: start rommd at login with the platform's
//! own mechanism, so it runs without a terminal and restarts after updates.
//! The systemd and launchd definitions set `ROMMD_SUPERVISED`, which tells the
//! updater it may simply exit and be started again.

use anyhow::{Context, Result};
use std::path::Path;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::path::PathBuf;
use std::process::Command;

pub fn install() -> Result<()> {
    let exe = std::env::current_exe().context("locating the rommd binary")?;
    platform::install(&exe)
}

pub fn uninstall() -> Result<()> {
    platform::uninstall()
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("running {:?}", cmd))?;
    anyhow::ensure!(status.success(), "{:?} failed ({})", cmd, status);
    Ok(())
}

#[cfg(any(target_os = "macos", windows))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    const UNIT: &str = "rommd.service";

    fn unit_path() -> Result<PathBuf> {
        Ok(dirs::config_dir()
            .context("no config dir")?
            .join("systemd/user")
            .join(UNIT))
    }

    /// A user unit tied to the graphical session, so emulators it launches can
    /// open windows. `KillMode=process` keeps running emulators alive when
    /// rommd restarts (after an update, or `systemctl --user restart`).
    pub fn unit(exe: &Path) -> String {
        format!(
            "[Unit]\n\
             Description=rommd: RomM emulator bridge\n\
             PartOf=graphical-session.target\n\
             After=graphical-session.target\n\
             \n\
             [Service]\n\
             ExecStart=\"{}\"\n\
             Environment={}=1\n\
             Restart=always\n\
             RestartSec=2\n\
             KillMode=process\n\
             \n\
             [Install]\n\
             WantedBy=graphical-session.target\n",
            exe.display(),
            crate::update::SUPERVISED_ENV
        )
    }

    pub fn install(exe: &Path) -> Result<()> {
        let path = unit_path()?;
        write(&path, &unit(exe))?;
        run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        run(Command::new("systemctl").args(["--user", "enable", UNIT]))?;
        // restart rather than start, so a reinstall runs the new binary.
        run(Command::new("systemctl").args(["--user", "restart", UNIT]))?;
        println!(
            "rommd now starts with your desktop session ({}).",
            path.display()
        );
        println!("Logs: journalctl --user -u rommd -f");
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let _ = run(Command::new("systemctl").args(["--user", "disable", "--now", UNIT]));
        let path = unit_path()?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        println!("rommd no longer starts automatically.");
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    const LABEL: &str = "io.github.rommd";

    fn plist_path() -> Result<PathBuf> {
        Ok(dirs::home_dir()
            .context("no home dir")?
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    fn domain() -> String {
        format!("gui/{}", nix::unistd::getuid())
    }

    /// A launch agent restarted whenever it exits. `AbandonProcessGroup` keeps
    /// running emulators alive when rommd restarts.
    pub fn plist(exe: &Path) -> Result<String> {
        let log = dirs::home_dir()
            .context("no home dir")?
            .join("Library/Logs/rommd.log");
        Ok(format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{exe}</string></array>
  <key>EnvironmentVariables</key><dict><key>{env}</key><string>1</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>AbandonProcessGroup</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
            exe = xml_escape(&exe.display().to_string()),
            env = crate::update::SUPERVISED_ENV,
            log = xml_escape(&log.display().to_string()),
        ))
    }

    pub fn install(exe: &Path) -> Result<()> {
        let path = plist_path()?;
        let _ =
            run(Command::new("launchctl").args(["bootout", &format!("{}/{}", domain(), LABEL)]));
        write(&path, &plist(exe)?)?;
        run(Command::new("launchctl")
            .arg("bootstrap")
            .arg(domain())
            .arg(&path))?;
        println!("rommd now starts when you log in ({}).", path.display());
        println!("Logs: ~/Library/Logs/rommd.log");
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let _ =
            run(Command::new("launchctl").args(["bootout", &format!("{}/{}", domain(), LABEL)]));
        let path = plist_path()?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        println!("rommd no longer starts automatically.");
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use super::*;

    const TASK: &str = "rommd";

    /// A task started at this user's logon, with `--background` so it runs
    /// without a console window and logs to a file. A logon trigger for one
    /// user needs no elevation (`schtasks /SC ONLOGON` does). The settings
    /// override Task Scheduler's defaults, which would not start rommd on
    /// battery, stop it when unplugged or after 3 days, and run it below
    /// normal priority, which the emulators it starts would inherit. Ending
    /// the task leaves those emulators running. Windows has no restart-on-exit
    /// here, so the updater starts the new version itself (`ROMMD_SUPERVISED`
    /// is not set).
    pub fn task_xml(exe: &Path, user: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>rommd: RomM emulator bridge</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals>
    <Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command><Arguments>--background</Arguments></Exec></Actions>
</Task>
"#,
            user = xml_escape(user),
            exe = xml_escape(&exe.display().to_string()),
        )
    }

    fn current_user() -> Result<String> {
        let name = std::env::var("USERNAME").context("USERNAME is not set")?;
        Ok(match std::env::var("USERDOMAIN") {
            Ok(domain) if !domain.is_empty() => format!("{domain}\\{name}"),
            _ => name,
        })
    }

    pub fn install(exe: &Path) -> Result<()> {
        // schtasks reads the definition from a file, as UTF-16 like it declares.
        let xml: Vec<u8> = std::iter::once(0xFEFF)
            .chain(task_xml(exe, &current_user()?).encode_utf16())
            .flat_map(u16::to_le_bytes)
            .collect();
        let path = std::env::temp_dir().join(format!("rommd-task-{}.xml", std::process::id()));
        std::fs::write(&path, xml).with_context(|| format!("writing {}", path.display()))?;
        let created = run(Command::new("schtasks")
            .args(["/Create", "/F", "/TN", TASK, "/XML"])
            .arg(&path));
        let _ = std::fs::remove_file(&path);
        created?;
        let _ = run(Command::new("schtasks").args(["/End", "/TN", TASK]));
        run(Command::new("schtasks").args(["/Run", "/TN", TASK]))?;
        println!("rommd now starts when you log in (scheduled task \"{TASK}\").");
        println!("Logs: {}", crate::log_path().display());
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let _ = run(Command::new("schtasks").args(["/End", "/TN", TASK]));
        run(Command::new("schtasks").args(["/Delete", "/F", "/TN", TASK]))?;
        println!("rommd no longer starts automatically.");
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use super::*;

    pub fn install(_exe: &Path) -> Result<()> {
        anyhow::bail!("starting rommd at login is not supported on this platform")
    }

    pub fn uninstall() -> Result<()> {
        anyhow::bail!("starting rommd at login is not supported on this platform")
    }
}

/// rommd is a console program, so the logon task starts it with a console
/// window; `--background` detaches from it, which closes the window.
#[cfg(windows)]
pub mod console {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleWindow};

    pub fn detach() {
        // SAFETY: no arguments; detaching only invalidates the std handles,
        // which logging no longer uses in background mode.
        unsafe { FreeConsole() };
    }

    pub fn has_window() -> bool {
        // SAFETY: no arguments; returns a window handle or null.
        !unsafe { GetConsoleWindow() }.is_null()
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    #[test]
    fn task_runs_in_the_background_without_default_limits() {
        let xml = super::platform::task_xml(
            std::path::Path::new(r"C:\Users\A&B\AppData\Local\Programs\rommd\rommd.exe"),
            r"PC\A&B",
        );
        assert!(xml.contains(
            r"<Command>C:\Users\A&amp;B\AppData\Local\Programs\rommd\rommd.exe</Command>"
        ));
        assert!(xml.contains("<Arguments>--background</Arguments>"));
        assert!(xml.contains(r"<LogonTrigger><Enabled>true</Enabled><UserId>PC\A&amp;B</UserId>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<Priority>5</Priority>"));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn unit_restarts_without_killing_emulators() {
        let unit = super::platform::unit(std::path::Path::new("/home/u/.local/bin/rommd"));
        assert!(unit.contains("ExecStart=\"/home/u/.local/bin/rommd\""));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("KillMode=process"));
        assert!(unit.contains("Environment=ROMMD_SUPERVISED=1"));
        assert!(unit.contains("WantedBy=graphical-session.target"));
    }
}
