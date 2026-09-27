//! Test support: a fake RomM server, a stand-in emulator, and scratch dirs.

pub mod fake_romm;

use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};
use tokio::process::Child;

use crate::config::EmulatorConfig;
use crate::emulator::{Emulator, LaunchContext, SaveLayout};
use crate::profile::ProfileBlob;

/// An emulator that only has an identity; tests hand the sync engine a
/// `SaveLayout` directly.
pub struct TestEmulator {
    pub id: &'static str,
    pub family: &'static [&'static str],
}

#[async_trait]
impl Emulator for TestEmulator {
    fn id(&self) -> &'static str {
        self.id
    }

    fn display_name(&self) -> &'static str {
        self.id
    }

    async fn launch(
        &self,
        _config: &EmulatorConfig,
        _rom_path: &Path,
        _game_id: Option<&str>,
        _profile: Option<&ProfileBlob>,
    ) -> Result<(Child, LaunchContext)> {
        unimplemented!("test emulators are never launched")
    }

    async fn snapshot(&self, _config: &EmulatorConfig, _game_id: &str) -> Result<ProfileBlob> {
        anyhow::bail!("not supported")
    }

    async fn save_layout(&self, _config: &EmulatorConfig, _rom_path: &Path, _game_id: Option<&str>) -> SaveLayout {
        SaveLayout::default()
    }

    fn save_family(&self) -> &'static [&'static str] {
        self.family
    }
}

/// A fresh, empty directory under the system temp dir, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rommd-test-{}-{}-{}", label, std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn write(path: &Path, data: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, data).unwrap();
}

/// Set a file's mtime `age` in the past.
pub fn age(path: &Path, age: Duration) {
    let t = SystemTime::now() - age;
    std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
}

/// Build a zip in memory from (entry name, contents).
pub fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, data) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap().into_inner()
}
