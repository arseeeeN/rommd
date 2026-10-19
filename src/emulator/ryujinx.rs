use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Child;
use tracing::{debug, info};

use super::{CreateDir, Emulator, GameSaveDir, LaunchContext, SaveLayout, SaveSet};
use crate::config::EmulatorConfig;
use crate::profile::ProfileBlob;

pub struct Ryujinx;

#[async_trait]
impl Emulator for Ryujinx {
    fn id(&self) -> &'static str {
        "ryujinx"
    }

    fn display_name(&self) -> &'static str {
        "Ryujinx"
    }

    fn platforms(&self) -> &'static [&'static str] {
        &["switch"]
    }

    fn config_dir_markers(&self) -> &'static [&'static str] {
        &["Config.json", "bis", "system"]
    }

    fn config_dir_hint(&self) -> &'static str {
        if cfg!(windows) {
            r"%APPDATA%\Ryujinx (the portable folder next to Ryujinx.exe for portable installs)"
        } else if cfg!(target_os = "macos") {
            "~/Library/Application Support/Ryujinx"
        } else {
            "~/.config/Ryujinx"
        }
    }

    fn game_id_for_rom(&self, config: &EmulatorConfig, rom_path: &Path) -> Option<String> {
        crate::switch::title_id_for_rom(
            rom_path,
            &config.config_dir.join("system").join("prod.keys"),
        )
    }

    fn save_family(&self) -> &'static [&'static str] {
        crate::switch::SAVE_COMPATIBLE_EMULATORS
    }

    async fn save_layout(
        &self,
        config: &EmulatorConfig,
        _rom_path: &Path,
        game_id: Option<&str>,
    ) -> SaveLayout {
        let saves = match game_id {
            Some(id) => game_save_dir(config, id).await.map(SaveSet::Dir),
            None => None,
        };
        SaveLayout {
            saves,
            states: None,
        }
    }

    async fn launch(
        &self,
        config: &EmulatorConfig,
        rom_path: &Path,
        _game_id: Option<&str>,
        _profile: Option<&ProfileBlob>,
    ) -> Result<(Child, LaunchContext)> {
        let child = super::emulator_command(&config.binary)
            .arg(rom_path)
            .spawn()
            .with_context(|| format!("spawning Ryujinx from {}", config.binary.display()))?;
        Ok((child, LaunchContext::default()))
    }

    async fn snapshot(&self, _config: &EmulatorConfig, _game_id: &str) -> Result<ProfileBlob> {
        anyhow::bail!("Ryujinx does not support per-game profiles")
    }
}

/// Scan `bis/user/save/*/ExtraData0` for the slot whose title ID matches
/// `game_id`, preferring the Account save and falling back to the Device
/// save for games that keep one save per console.
///
/// Without a slot, the layout points at the one rommd would create (see
/// `create_slot`), so a RomM save can be restored before Ryujinx has ever
/// booted the game. The slot is only created when there is a save to restore.
///
/// Inside a slot, `0/` is the committed save and `1/` the working copy of the
/// save filesystem's two-phase commit; they are not user profiles. `0/` is the
/// source of truth, and restores write both so neither holds a stale copy.
async fn game_save_dir(config: &EmulatorConfig, game_id: &str) -> Option<GameSaveDir> {
    let title_id = u64::from_str_radix(game_id, 16).ok()?;
    let saves_root = config.config_dir.join("bis").join("user").join("save");
    if let Some(slot) = find_save_slot(&saves_root, title_id).await {
        return Some(slot_save_dir(game_id, &slot, None));
    }
    let root = config.config_dir.clone();
    let pending = tokio::task::spawn_blocking(move || pending_slot(&root, title_id))
        .await
        .ok()?;
    let pending = match pending {
        Ok(p) => p,
        Err(e) => {
            info!(
                "no Ryujinx save slot for {} yet and rommd can't create one ({:#}); saves sync once Ryujinx has booted the game",
                game_id, e
            );
            return None;
        }
    };
    debug!(
        "no Ryujinx save slot for {} yet; slot {:016x} is created if a save is restored",
        game_id, pending.id
    );
    let slot = pending.dir(&config.config_dir);
    let (root, binary) = (config.config_dir.clone(), config.binary.clone());
    let create = CreateDir(Arc::new(move || {
        anyhow::ensure!(
            !ryujinx_running(&binary),
            "Ryujinx is running; it keeps its save index in memory and would overwrite the new slot"
        );
        create_slot(
            &root,
            &pending,
            &crate::sync::data_dir().join("ryujinx-index-backups"),
        )
    }));
    Some(slot_save_dir(game_id, &slot, Some(create)))
}

fn slot_save_dir(game_id: &str, slot: &Path, create: Option<CreateDir>) -> GameSaveDir {
    GameSaveDir {
        dir: slot.join("0"),
        archive_root: game_id.to_uppercase(),
        extra_roots: Vec::new(),
        mirrors: vec![slot.join("1")],
        required_subdir: None,
        create,
    }
}

const SAVE_TYPE_ACCOUNT: u8 = 1;
const SAVE_TYPE_DEVICE: u8 = 3;

/// Walk `saves_root/*/ExtraData0` (a SaveDataAttribute: program ID u64 at 0x00,
/// user ID at 0x08, save type u8 at 0x20) and return the slot for `title_id`,
/// Account before Device. Other types (BCAT server data, cache, temporary) are
/// not player saves and are never synced.
async fn find_save_slot(saves_root: &Path, title_id: u64) -> Option<PathBuf> {
    let mut device = None;
    let mut rd = tokio::fs::read_dir(saves_root).await.ok()?;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let slot_dir = entry.path();
        if !slot_dir.is_dir() {
            continue;
        }
        let data = match tokio::fs::read(slot_dir.join("ExtraData0")).await {
            Ok(d) => d,
            Err(_) => continue,
        };
        if data.len() < 0x21 {
            continue;
        }
        let found_id = u64::from_le_bytes(data[0x00..0x08].try_into().unwrap());
        if found_id != title_id {
            continue;
        }
        match data[0x20] {
            SAVE_TYPE_ACCOUNT => {
                debug!("found account save slot at {}", slot_dir.display());
                return Some(slot_dir);
            }
            SAVE_TYPE_DEVICE => device = Some(slot_dir),
            _ => {}
        }
    }
    if let Some(ref d) = device {
        debug!("found device save slot at {}", d.display());
    }
    device
}

// ── Creating save slots ──────────────────────────────────────────────────────
//
// Ryujinx registers every save in an index, itself a two-phase system save
// (`0/` committed, `1/` working copy, identical once Ryujinx has closed):
// `imkvdb.arc` (LibHac key-value archive: `IMKV`, u32 reserved, u32 count, then
// per entry `IMEN`, u32 key size, u32 value size, key, value) maps each save's
// 0x40-byte SaveDataAttribute to a 0x40-byte value holding its slot ID, and
// `lastPublishedId` (u64) is the last slot ID handed out. Ryujinx loads the
// index when it starts and writes it back whole, so it may only be changed
// while Ryujinx is closed. The layout was read from Ryujinx 1.3.3; anything
// else is refused rather than guessed.
//
// A slot's `ExtraData0`/`1` hold the attribute again, then the owner (the
// title ID), sizes and a commit ID. The sizes come from the game's NACP, but
// Ryujinx doesn't check them: Super Mario Odyssey loaded and saved in a slot
// with size 0, and Ryujinx kept 0 when it rewrote the ExtraData. So 0 is
// written rather than decrypting the control NCA (title key, AES-CTR, RomFS);
// that is only needed if a Ryujinx version starts checking.

const INDEX_SAVE: &str = "bis/system/save/8000000000000000";
const ATTRIBUTE_LEN: usize = 0x40;
const ENTRY_LEN: usize = 12 + 2 * ATTRIBUTE_LEN;
const EXTRA_DATA_LEN: usize = 0x200;
const USER_SPACE: u8 = 1;

type Attribute = [u8; ATTRIBUTE_LEN];

/// A slot rommd may create: its ID and the attribute it is registered under.
#[derive(Debug, Clone)]
struct PendingSlot {
    id: u64,
    attribute: Attribute,
}

impl PendingSlot {
    fn dir(&self, root: &Path) -> PathBuf {
        root.join("bis/user/save").join(format!("{:016x}", self.id))
    }
}

struct SaveIndex {
    entries: Vec<(Attribute, [u8; ATTRIBUTE_LEN])>,
    last_published_id: u64,
}

impl SaveIndex {
    /// Read the committed copy, refusing unless the working copy matches it
    /// and the format is exactly the one above.
    fn read(root: &Path) -> Result<(Self, [Vec<u8>; 2])> {
        let dir = root.join(INDEX_SAVE);
        let read = |rel: &str| {
            std::fs::read(dir.join(rel)).with_context(|| format!("reading {}/{}", INDEX_SAVE, rel))
        };
        let arc = read("0/imkvdb.arc")?;
        let last = read("0/lastPublishedId")?;
        anyhow::ensure!(
            read("1/imkvdb.arc")? == arc && read("1/lastPublishedId")? == last,
            "the save index's committed and working copies differ"
        );
        Ok((Self::parse(&arc, &last)?, [arc, last]))
    }

    fn parse(arc: &[u8], last: &[u8]) -> Result<Self> {
        anyhow::ensure!(
            arc.len() >= 12 && &arc[..4] == b"IMKV",
            "imkvdb.arc is not an IMKV archive"
        );
        let count = u32::from_le_bytes(arc[8..12].try_into().unwrap()) as usize;
        anyhow::ensure!(
            arc.len() == 12 + count * ENTRY_LEN,
            "imkvdb.arc has an unexpected size"
        );
        let entries = arc[12..]
            .as_chunks::<ENTRY_LEN>()
            .0
            .iter()
            .map(|e| {
                anyhow::ensure!(
                    &e[..4] == b"IMEN"
                        && e[4..8] == (ATTRIBUTE_LEN as u32).to_le_bytes()
                        && e[8..12] == (ATTRIBUTE_LEN as u32).to_le_bytes(),
                    "imkvdb.arc has an entry of an unknown format"
                );
                Ok((
                    e[12..12 + ATTRIBUTE_LEN].try_into().unwrap(),
                    e[12 + ATTRIBUTE_LEN..].try_into().unwrap(),
                ))
            })
            .collect::<Result<_>>()?;
        let last_published_id = u64::from_le_bytes(
            last.try_into()
                .map_err(|_| anyhow::anyhow!("lastPublishedId has an unexpected size"))?,
        );
        Ok(Self {
            entries,
            last_published_id,
        })
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + self.entries.len() * ENTRY_LEN);
        out.extend_from_slice(b"IMKV");
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for (key, value) in &self.entries {
            out.extend_from_slice(b"IMEN");
            out.extend_from_slice(&(ATTRIBUTE_LEN as u32).to_le_bytes());
            out.extend_from_slice(&(ATTRIBUTE_LEN as u32).to_le_bytes());
            out.extend_from_slice(key);
            out.extend_from_slice(value);
        }
        out
    }
}

/// LibHac's attribute order: program, type, user, system save ID, rank, index.
/// The index read from Ryujinx was in this order (and in slot order), so new
/// entries go where both would put them.
fn attribute_order(a: &Attribute) -> (u64, u8, u64, u64, u64, u8, u16) {
    let u64_at = |o: usize| u64::from_le_bytes(a[o..o + 8].try_into().unwrap());
    (
        u64_at(0x00),
        a[0x20],
        u64_at(0x08),
        u64_at(0x10),
        u64_at(0x18),
        a[0x21],
        u16::from_le_bytes([a[0x22], a[0x23]]),
    )
}

fn attribute(title_id: u64, user: [u64; 2], save_type: u8) -> Attribute {
    let mut a = [0u8; ATTRIBUTE_LEN];
    a[0x00..0x08].copy_from_slice(&title_id.to_le_bytes());
    a[0x08..0x10].copy_from_slice(&user[0].to_le_bytes());
    a[0x10..0x18].copy_from_slice(&user[1].to_le_bytes());
    a[0x20] = save_type;
    a
}

/// The active profile from `system/Profiles.json`, as the two halves of its
/// 32-digit ID in the order a save attribute stores them.
fn active_user(root: &Path) -> Result<[u64; 2]> {
    let text = std::fs::read_to_string(root.join("system/Profiles.json"))
        .context("reading system/Profiles.json")?;
    let profiles: serde_json::Value = serde_json::from_str(&text)?;
    let id = profiles["last_opened"]
        .as_str()
        .filter(|id| id.len() == 32)
        .ok_or_else(|| anyhow::anyhow!("Profiles.json has no active profile"))?;
    Ok([
        u64::from_str_radix(&id[..16], 16)?,
        u64::from_str_radix(&id[16..], 16)?,
    ])
}

/// The slot rommd would create for `title_id`: Account for the active user,
/// or Device for games that keep one save per console.
fn pending_slot(root: &Path, title_id: u64) -> Result<PendingSlot> {
    let (index, _) = SaveIndex::read(root)?;
    let attribute =
        if crate::switch::DEVICE_SAVE_TITLES.contains(&format!("{:016X}", title_id).as_str()) {
            attribute(title_id, [0, 0], SAVE_TYPE_DEVICE)
        } else {
            attribute(title_id, active_user(root)?, SAVE_TYPE_ACCOUNT)
        };
    let pending = PendingSlot {
        id: index.last_published_id + 1,
        attribute,
    };
    anyhow::ensure!(
        !pending.dir(root).exists(),
        "slot {:016x} exists but is not registered",
        pending.id
    );
    Ok(pending)
}

/// Create `pending`'s slot and register it, as Ryujinx would. Refuses if the
/// index changed since `pending` was worked out. The index files are backed up
/// to `backups` first, and restored if registering fails halfway.
fn create_slot(root: &Path, pending: &PendingSlot, backups: &Path) -> Result<()> {
    let (mut index, original) = SaveIndex::read(root)?;
    anyhow::ensure!(
        index.last_published_id + 1 == pending.id
            && !index.entries.iter().any(|(k, _)| *k == pending.attribute),
        "Ryujinx's save index changed since the save was looked up"
    );
    let slot = pending.dir(root);
    anyhow::ensure!(!slot.exists(), "{} already exists", slot.display());

    let backup = backups.join(
        chrono::Local::now()
            .format("%Y-%m-%d_%H-%M-%S%.3f")
            .to_string(),
    );
    std::fs::create_dir_all(&backup)?;
    std::fs::write(backup.join("imkvdb.arc"), &original[0])?;
    std::fs::write(backup.join("lastPublishedId"), &original[1])?;
    crate::sync::prune_backups(backups);

    let staging = slot.with_file_name(format!(".rommd-{:016x}", pending.id));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(staging.join("0"))?;
    std::fs::create_dir_all(staging.join("1"))?;
    std::fs::write(staging.join(".lock"), b"")?;
    let extra = extra_data(&pending.attribute);
    std::fs::write(staging.join("ExtraData0"), extra)?;
    std::fs::write(staging.join("ExtraData1"), extra)?;
    std::fs::rename(&staging, &slot)?;

    let mut value = [0u8; ATTRIBUTE_LEN];
    value[..8].copy_from_slice(&pending.id.to_le_bytes());
    value[0x18] = USER_SPACE;
    index.entries.push((pending.attribute, value));
    index.entries.sort_by_key(|(k, _)| attribute_order(k));
    index.last_published_id = pending.id;
    let updated = [index.encode(), pending.id.to_le_bytes().to_vec()];

    let dir = root.join(INDEX_SAVE);
    let write_all = |files: &[Vec<u8>; 2]| -> Result<()> {
        for half in ["1", "0"] {
            write_replacing(&dir.join(half).join("imkvdb.arc"), &files[0])?;
            write_replacing(&dir.join(half).join("lastPublishedId"), &files[1])?;
        }
        Ok(())
    };
    if let Err(e) = write_all(&updated) {
        let _ = write_all(&original);
        let _ = std::fs::remove_dir_all(&slot);
        return Err(e.context("registering the new slot in Ryujinx's save index"));
    }
    info!(
        "created Ryujinx save slot {:016x} for {:016X} (index backed up to {})",
        pending.id,
        u64::from_le_bytes(pending.attribute[..8].try_into().unwrap()),
        backup.display()
    );
    Ok(())
}

fn extra_data(attribute: &Attribute) -> [u8; EXTRA_DATA_LEN] {
    use std::hash::{BuildHasher, Hasher};
    let mut d = [0u8; EXTRA_DATA_LEN];
    d[..ATTRIBUTE_LEN].copy_from_slice(attribute);
    d[0x40..0x48].copy_from_slice(&attribute[..8]);
    let commit_id = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    d[0x68..0x70].copy_from_slice(&commit_id.to_le_bytes());
    d
}

fn write_replacing(path: &Path, data: &[u8]) -> Result<()> {
    let staged = path.with_extension("rommd-tmp");
    std::fs::write(&staged, data)?;
    std::fs::rename(&staged, path).with_context(|| format!("writing {}", path.display()))
}

/// Whether Ryujinx is running, by process name: "Ryujinx" or the configured
/// binary's name. When the check itself fails, Ryujinx counts as running.
fn ryujinx_running(binary: &Path) -> bool {
    let mut names = vec!["Ryujinx".to_owned()];
    if let Some(stem) = binary.file_stem() {
        names.push(stem.to_string_lossy().into_owned());
    }
    names.iter().any(|name| process_running(name))
}

#[cfg(unix)]
fn process_running(name: &str) -> bool {
    // Linux matches against the first 15 bytes of the process name.
    let name: String = if cfg!(target_os = "linux") {
        name.chars().take(15).collect()
    } else {
        name.to_owned()
    };
    match std::process::Command::new("pgrep")
        .args(["-i", "-x", &name])
        .output()
    {
        Ok(out) => out.status.code() != Some(1),
        Err(_) => true,
    }
}

#[cfg(windows)]
fn process_running(name: &str) -> bool {
    let image = format!("{name}.exe");
    match std::process::Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {image}"), "/FO", "CSV", "/NH"])
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .to_lowercase()
            .contains(&image.to_lowercase()),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TempDir, write};

    fn extra_data(title: u64, save_type: u8) -> Vec<u8> {
        let mut d = vec![0u8; 0x40];
        d[..8].copy_from_slice(&title.to_le_bytes());
        d[0x20] = save_type;
        d
    }

    #[tokio::test]
    async fn finds_the_account_save_before_the_device_save() {
        let root = TempDir::new("ryujinx");
        let config = EmulatorConfig {
            binary: "ryujinx".into(),
            config_dir: root.path().to_owned(),
        };
        let saves = root.path().join("bis/user/save");
        write(
            &saves.join("0000000000000001/ExtraData0"),
            &extra_data(0x0100000000010000, 2),
        ); // BCAT
        write(
            &saves.join("0000000000000002/ExtraData0"),
            &extra_data(0x0100000000010000, 3),
        ); // device
        write(
            &saves.join("0000000000000003/ExtraData0"),
            &extra_data(0x0100000000020000, 1),
        ); // other game
        let device = game_save_dir(&config, "0100000000010000").await.unwrap();
        assert_eq!(device.dir, saves.join("0000000000000002/0"));

        write(
            &saves.join("0000000000000004/ExtraData0"),
            &extra_data(0x0100000000010000, 1),
        ); // account
        let account = game_save_dir(&config, "0100000000010000").await.unwrap();
        assert_eq!(account.dir, saves.join("0000000000000004/0"));
        assert_eq!(account.mirrors, vec![saves.join("0000000000000004/1")]);
        assert_eq!(account.archive_root, "0100000000010000");
        assert!(
            game_save_dir(&config, "0100000000030000").await.is_none(),
            "no slot until Ryujinx creates one"
        );
    }

    const SMO: u64 = 0x0100000000010000;
    const POKEMON: u64 = 0x0100F43008C44000;
    const RYU_PLAYER: &str = "00000000000000010000000000000000";

    /// Bytes from a buffer of `len` zeros with these (offset, byte) set.
    fn bytes(len: usize, set: &[(usize, u8)]) -> Vec<u8> {
        let mut b = vec![0u8; len];
        for &(o, v) in set {
            b[o] = v;
        }
        b
    }

    fn entry(key: &[u8], value: &[u8]) -> Vec<u8> {
        [
            b"IMEN".as_slice(),
            &[0x40, 0, 0, 0, 0x40, 0, 0, 0],
            key,
            value,
        ]
        .concat()
    }

    fn archive(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut a = b"IMKV".to_vec();
        a.extend_from_slice(&0u32.to_le_bytes());
        a.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        a.extend(entries.concat());
        a
    }

    /// The system save entry every Ryujinx index starts with (0x8000000000000030).
    fn system_entry() -> Vec<u8> {
        entry(
            &bytes(0x40, &[(0x18, 0x30), (0x1f, 0x80)]),
            &bytes(0x40, &[(0x00, 0x30), (0x07, 0x80)]),
        )
    }

    fn ryujinx_root(label: &str, entries: &[Vec<u8>], last_id: u64) -> TempDir {
        let root = TempDir::new(label);
        for half in ["0", "1"] {
            let dir = root.path().join(INDEX_SAVE).join(half);
            write(&dir.join("imkvdb.arc"), &archive(entries));
            write(&dir.join("lastPublishedId"), &last_id.to_le_bytes());
        }
        write(
            &root.path().join("system/Profiles.json"),
            format!(r#"{{"profiles":[{{"user_id":"{RYU_PLAYER}","name":"RyuPlayer"}}],"last_opened":"{RYU_PLAYER}"}}"#).as_bytes(),
        );
        root
    }

    fn read(path: PathBuf) -> Vec<u8> {
        std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {}", path.display(), e))
    }

    #[test]
    fn creates_the_slot_ryujinx_created_for_super_mario_odyssey() {
        // Ryujinx 1.3.3 gave Super Mario Odyssey's Account save slot 1 in an
        // index holding only the system save; these are the bytes it wrote.
        let root = ryujinx_root("ryujinx-smo", &[system_entry()], 0);
        let backups = root.path().join("backups");
        let pending = pending_slot(root.path(), SMO).unwrap();
        assert_eq!(pending.id, 1);
        create_slot(root.path(), &pending, &backups).unwrap();

        let smo_key = bytes(0x40, &[(0x02, 1), (0x07, 1), (0x08, 1), (0x20, 1)]);
        let smo_entry = entry(&smo_key, &bytes(0x40, &[(0x00, 1), (0x18, 1)]));
        for half in ["0", "1"] {
            let dir = root.path().join(INDEX_SAVE).join(half);
            assert_eq!(
                read(dir.join("imkvdb.arc")),
                archive(&[system_entry(), smo_entry.clone()])
            );
            assert_eq!(read(dir.join("lastPublishedId")), 1u64.to_le_bytes());
        }

        let slot = root.path().join("bis/user/save/0000000000000001");
        assert!(slot.join("0").is_dir() && slot.join("1").is_dir() && slot.join(".lock").is_file());
        let extra = read(slot.join("ExtraData0"));
        assert_eq!(extra, read(slot.join("ExtraData1")));
        // Ryujinx wrote the game's sizes (16 MiB, 3 MiB) at 0x58/0x60; rommd
        // writes 0, which Ryujinx accepts. The commit ID at 0x68 is random.
        let mut expected = bytes(0x200, &[(0x42, 1), (0x47, 1)]);
        expected[..0x40].copy_from_slice(&smo_key);
        assert_eq!(extra[..0x68], expected[..0x68]);
        assert_eq!(extra[0x70..], expected[0x70..]);

        let backed_up: Vec<_> = std::fs::read_dir(&backups).unwrap().flatten().collect();
        assert_eq!(backed_up.len(), 1);
        assert_eq!(
            read(backed_up[0].path().join("imkvdb.arc")),
            archive(&[system_entry()])
        );
        assert_eq!(
            find_slot_blocking(root.path(), SMO),
            Some(slot),
            "the slot is found like one Ryujinx created"
        );
    }

    fn find_slot_blocking(root: &Path, title: u64) -> Option<PathBuf> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(find_save_slot(&root.join("bis/user/save"), title))
    }

    #[test]
    fn new_entries_are_inserted_in_order() {
        let pokemon = entry(
            &attribute(POKEMON, [1, 0], SAVE_TYPE_ACCOUNT),
            &bytes(0x40, &[(0x00, 3), (0x18, 1)]),
        );
        let root = ryujinx_root("ryujinx-order", &[system_entry(), pokemon.clone()], 3);
        let pending = pending_slot(root.path(), SMO).unwrap();
        assert_eq!(pending.id, 4);
        create_slot(root.path(), &pending, &root.path().join("backups")).unwrap();
        let arc = read(root.path().join(INDEX_SAVE).join("0/imkvdb.arc"));
        let index = SaveIndex::parse(&arc, &4u64.to_le_bytes()).unwrap();
        let titles: Vec<u64> = index
            .entries
            .iter()
            .map(|(k, _)| u64::from_le_bytes(k[..8].try_into().unwrap()))
            .collect();
        assert_eq!(titles, vec![0, SMO, POKEMON]);
    }

    #[test]
    fn device_save_titles_get_a_device_slot() {
        let root = ryujinx_root("ryujinx-device", &[system_entry()], 0);
        let acnh = 0x01006F8002326000;
        let pending = pending_slot(root.path(), acnh).unwrap();
        assert_eq!(pending.attribute, attribute(acnh, [0, 0], SAVE_TYPE_DEVICE));
    }

    #[test]
    fn refuses_an_index_it_does_not_recognise_or_that_changed() {
        let index_files = |root: &TempDir| -> Vec<Vec<u8>> {
            [
                "0/imkvdb.arc",
                "1/imkvdb.arc",
                "0/lastPublishedId",
                "1/lastPublishedId",
            ]
            .iter()
            .map(|f| read(root.path().join(INDEX_SAVE).join(f)))
            .collect()
        };
        let untouched = |root: &TempDir, before: Vec<Vec<u8>>| {
            assert_eq!(index_files(root), before);
            assert!(!root.path().join("bis/user/save/0000000000000001").exists());
        };

        // Ryujinx created a save after the layout was worked out.
        let root = ryujinx_root("ryujinx-raced", &[system_entry()], 0);
        let pending = pending_slot(root.path(), SMO).unwrap();
        for half in ["0", "1"] {
            write(
                &root
                    .path()
                    .join(INDEX_SAVE)
                    .join(half)
                    .join("lastPublishedId"),
                &1u64.to_le_bytes(),
            );
        }
        let before = index_files(&root);
        assert!(create_slot(root.path(), &pending, &root.path().join("b")).is_err());
        untouched(&root, before);

        // The working copy differs from the committed one.
        let root = ryujinx_root("ryujinx-halfway", &[system_entry()], 0);
        let pending = pending_slot(root.path(), SMO).unwrap();
        write(
            &root.path().join(INDEX_SAVE).join("1/imkvdb.arc"),
            &archive(&[]),
        );
        let before = index_files(&root);
        assert!(create_slot(root.path(), &pending, &root.path().join("b")).is_err());
        untouched(&root, before);

        // Not the format rommd knows.
        let root = ryujinx_root("ryujinx-unknown", &[system_entry()], 0);
        let mut odd = archive(&[system_entry()]);
        odd[4] = 1;
        odd.push(0);
        for half in ["0", "1"] {
            write(
                &root.path().join(INDEX_SAVE).join(half).join("imkvdb.arc"),
                &odd,
            );
        }
        assert!(pending_slot(root.path(), SMO).is_err());
    }

    #[tokio::test]
    async fn without_a_slot_the_layout_points_at_the_one_rommd_would_create() {
        let root = ryujinx_root("ryujinx-pending", &[system_entry()], 5);
        let config = EmulatorConfig {
            binary: "ryujinx".into(),
            config_dir: root.path().to_owned(),
        };
        let save = game_save_dir(&config, "0100000000010000").await.unwrap();
        let slot = root.path().join("bis/user/save/0000000000000006");
        assert_eq!(save.dir, slot.join("0"));
        assert_eq!(save.mirrors, vec![slot.join("1")]);
        assert!(save.create.is_some());
        assert!(
            !slot.exists(),
            "nothing is created until a save is restored"
        );
    }
}
