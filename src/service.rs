//! `rommd service install|uninstall`: start rommd at login with the platform's
//! own mechanism, so it runs without a terminal and restarts after updates.
//! Every definition sets `ROMMD_SUPERVISED`, which tells the updater it may
//! simply exit and be started again.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
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
        Ok(dirs::config_dir().context("no config dir")?.join("systemd/user").join(UNIT))
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
        run(Command::new("systemctl").args(["--user", "enable", "--now", UNIT]))?;
        println!("rommd now starts with your desktop session ({}).", path.display());
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
        Ok(dirs::home_dir().context("no home dir")?.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
    }

    fn domain() -> String {
        format!("gui/{}", nix::unistd::getuid())
    }

    /// A launch agent restarted whenever it exits. `AbandonProcessGroup` keeps
    /// running emulators alive when rommd restarts.
    pub fn plist(exe: &Path) -> Result<String> {
        let log = dirs::home_dir().context("no home dir")?.join("Library/Logs/rommd.log");
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

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    pub fn install(exe: &Path) -> Result<()> {
        let path = plist_path()?;
        let _ = run(Command::new("launchctl").args(["bootout", &format!("{}/{}", domain(), LABEL)]));
        write(&path, &plist(exe)?)?;
        run(Command::new("launchctl").arg("bootstrap").arg(domain()).arg(&path))?;
        println!("rommd now starts when you log in ({}).", path.display());
        println!("Logs: ~/Library/Logs/rommd.log");
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let _ = run(Command::new("launchctl").args(["bootout", &format!("{}/{}", domain(), LABEL)]));
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

    /// A logon task. Windows has no restart-on-exit here, so the updater starts
    /// the new version itself (`ROMMD_SUPERVISED` is not set).
    pub fn install(exe: &Path) -> Result<()> {
        let action = format!("\"{}\"", exe.display());
        run(Command::new("schtasks").args(["/Create", "/F", "/SC", "ONLOGON", "/RL", "LIMITED", "/TN", TASK, "/TR", &action]))?;
        run(Command::new("schtasks").args(["/Run", "/TN", TASK]))?;
        println!("rommd now starts when you log in (scheduled task \"{TASK}\").");
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
