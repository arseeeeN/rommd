//! Nintendo 64 cartridges: the ROM's identity and the game's saves.
//!
//! Saves sync as argosy-launcher's unit (Sigil `src/n64_save.c` and
//! `src/sync_n64.c`, `docs/platforms/n64.md`): a zip of the save types the
//! game uses, `eeprom` (0x200 or 0x800 bytes), `pak1` to `pak4` (0x8000
//! each), `sram` (0x8000) and `flash` (0x20000), each in the order the N64
//! reads it, so the same save gives the same unit from any emulator. On disk
//! the standalone emulators keep a file per type, `.eep`, `.sra`, `.fla`, and
//! one `.mpk` holding four paks; mupen64plus (RMG) reverses each 32-bit word
//! of SRAM and flash, gopher64 keeps them in bus order.

use anyhow::{Context, Result, bail, ensure};
use md5::Digest;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::emulator::N64Files;
use crate::save_archive;

/// Emulators whose N64 saves any N64 emulator can restore: the unit is the
/// same from all of them, and restore also reads the shapes clients uploaded
/// before it. argosy-launcher's IDs (`EmulatorRegistry.kt`), RomM's web
/// player's libretro cores, and the standalone emulators.
pub const SAVE_COMPATIBLE_EMULATORS: &[&str] = &[
    "gopher64",
    "rmg",
    "mupen64plus",
    "simple64",
    "project64",
    "mupen64plus_next",
    "mupen64plus_next_gles2",
    "mupen64plus_next_gles3",
    "parallel_n64",
    "mupen64plus_fz",
    "mupen64plus_fz_pro",
    "m64pro_fzx_plus",
];

const EEPROM_4K: usize = 0x200;
const EEPROM_MAX: usize = 0x800;
const PAK_SIZE: usize = 0x8000;
const PAKS: usize = 4;
const SRAM_SIZE: usize = 0x8000;
const FLASH_SIZE: usize = 0x20000;

const REGIONS: usize = 7;
const EEPROM: usize = 0;
const PAK1: usize = 1;
const SRAM: usize = 5;
const FLASH: usize = 6;
const NAMES: [&str; REGIONS] = ["eeprom", "pak1", "pak2", "pak3", "pak4", "sram", "flash"];

/// The libretro cores' `.srm`: every region at a fixed offset.
const BLOB_SIZE: usize = 0x48800;
const BLOB_OFFSETS: [usize; REGIONS] = [0, 0x800, 0x8800, 0x10800, 0x18800, 0x20800, 0x28800];
/// A `.srm` with a 64DD disk appended (parallel_n64), the largest save file.
const MAX_FILE: u64 = BLOB_SIZE as u64 + 0x440_0000;

/// The standalone emulators' files: extension and the region it holds
/// (`.mpk`: all four paks).
const FILES: [(&str, usize); 4] = [
    ("eep", EEPROM),
    ("sra", SRAM),
    ("fla", FLASH),
    ("mpk", PAK1),
];

fn full_size(region: usize) -> usize {
    match region {
        EEPROM => EEPROM_MAX,
        SRAM => SRAM_SIZE,
        FLASH => FLASH_SIZE,
        _ => PAK_SIZE,
    }
}

fn is_pak(region: usize) -> bool {
    (PAK1..PAK1 + PAKS).contains(&region)
}

/// A game's save, region by region in bus order; `None` for a type the game
/// doesn't use.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Save([Option<Vec<u8>>; REGIONS]);

impl Save {
    /// Take one region from an emulator's file as stored. A short file reads
    /// as 0xFF past its end. The region counts as used when the game wrote to
    /// it: a byte other than 0x00 and 0xFF, or for a pak a note in its index.
    fn take(&mut self, region: usize, data: &[u8], word_swapped: bool) {
        let full = full_size(region);
        let mut buf = vec![0xFF; full];
        let n = data.len().min(full);
        buf[..n].copy_from_slice(&data[..n]);
        if word_swapped && matches!(region, SRAM | FLASH) {
            swap32(&mut buf);
        }
        let used = match region {
            EEPROM => {
                if !written(&buf[EEPROM_4K..]) {
                    buf.truncate(EEPROM_4K);
                }
                written(&buf)
            }
            r if is_pak(r) => pak_used(&buf),
            _ => written(&buf),
        };
        self.0[region] = used.then_some(buf);
    }

    fn take_paks(&mut self, data: &[u8]) {
        for p in 0..PAKS {
            self.take(
                PAK1 + p,
                data.get(p * PAK_SIZE..).unwrap_or_default(),
                false,
            );
        }
    }

    fn take_blob(&mut self, data: &[u8]) {
        for (region, at) in BLOB_OFFSETS.into_iter().enumerate() {
            self.take(region, data.get(at..).unwrap_or_default(), true);
        }
    }

    /// One member of a zip: the unit's own, or an emulator's file as clients
    /// zipped them before Sigil (mupen64plus's, Project64's `_Cont_<n>.mpk`).
    fn take_member(&mut self, name: &str, data: &[u8]) -> Result<()> {
        if let Some(region) = NAMES.iter().position(|n| *n == name) {
            let size_ok = match region {
                EEPROM => data.len() == EEPROM_4K || data.len() == EEPROM_MAX,
                r => data.len() == full_size(r),
            };
            ensure!(size_ok, "{name} has {} bytes", data.len());
            self.0[region] = Some(data.to_vec());
            return Ok(());
        }
        let lower = name.to_ascii_lowercase();
        if let Some(pak) = controller_of(&lower) {
            self.take(PAK1 + pak, data, false);
            return Ok(());
        }
        match lower.rsplit_once('.').map(|(_, ext)| ext) {
            Some("srm") => self.take_blob(data),
            Some("mpk") => self.take_paks(data),
            Some("eep") => self.take(EEPROM, data, true),
            Some("sra") => self.take(SRAM, data, true),
            Some("fla") => self.take(FLASH, data, true),
            _ => bail!("{name} is not an N64 save file"),
        }
        Ok(())
    }

    /// The region as an emulator stores it, at full size.
    fn give(&self, region: usize, word_swapped: bool) -> Vec<u8> {
        let mut buf = vec![0xFF; full_size(region)];
        if let Some(data) = &self.0[region] {
            buf[..data.len()].copy_from_slice(data);
        }
        if word_swapped && matches!(region, SRAM | FLASH) {
            swap32(&mut buf);
        }
        buf
    }

    fn is_empty(&self) -> bool {
        self.0.iter().all(Option::is_none)
    }

    /// The unit's members, in region order.
    pub fn members(&self) -> Vec<(String, Vec<u8>)> {
        self.0
            .iter()
            .zip(NAMES)
            .filter_map(|(data, name)| Some((name.to_owned(), data.clone()?)))
            .collect()
    }

    /// RomM's `content_hash` of the unit; `None` when the game has no save.
    pub fn hash(&self) -> Option<String> {
        save_archive::members_hash(&self.members())
    }
}

/// The pak (0 to 3) in Project64's `<name>_cont_<n>.mpk`, lowercased.
fn controller_of(lower: &str) -> Option<usize> {
    let stem = lower.strip_suffix(".mpk")?;
    let n = stem
        .chars()
        .last()?
        .to_digit(10)
        .filter(|n| (1..=4).contains(n))?;
    stem[..stem.len() - 1]
        .ends_with("_cont_")
        .then_some(n as usize - 1)
}

/// Reverse each 4-byte group; its own inverse.
fn swap32(data: &mut [u8]) {
    for word in data.as_chunks_mut::<4>().0 {
        word.reverse();
    }
}

fn written(data: &[u8]) -> bool {
    data.iter().any(|&b| b != 0x00 && b != 0xFF)
}

/// A pak's index table: inodes 5 to 127 of page 1, big-endian, 0x0003 when free.
const PAK_INDEX: std::ops::Range<usize> = 0x10A..0x200;

fn pak_used(pak: &[u8]) -> bool {
    PAK_INDEX
        .step_by(2)
        .any(|at| u16::from_be_bytes([pak[at], pak[at + 1]]) != 0x0003)
        && written(pak)
}

/// A controller pak formatted as mupen64plus formats a new one, with an
/// all-zero serial (Sigil's `n64_pak_formatted`).
fn pak_formatted() -> Vec<u8> {
    let mut out = vec![0u8; PAK_SIZE];
    for block in [0x20, 0x60, 0x80, 0xC0] {
        out[block + 0x18..block + 0x20]
            .copy_from_slice(&[0x00, 0x01, 0x01, 0x00, 0x01, 0x01, 0xFE, 0xF1]);
    }
    for at in PAK_INDEX.step_by(2) {
        out[at + 1] = 0x03;
    }
    out[0x101] = 0x71;
    out.copy_within(0x100..0x200, 0x200);
    out
}

/// The save a RomM entry holds: the unit, or what clients uploaded before
/// Sigil: a zip of an emulator's files, a libretro `.srm`, or a lone `.eep`.
pub fn read_unit(path: &Path) -> Result<Save> {
    let mut data = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_FILE)
        .read_to_end(&mut data)?;
    let mut save = Save::default();
    if data.starts_with(b"PK\x03\x04") {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&data))?;
        for i in 0..zip.len() {
            let entry = zip.by_index(i)?;
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().to_owned();
            let mut bytes = Vec::new();
            entry.take(MAX_FILE).read_to_end(&mut bytes)?;
            save.take_member(&name, &bytes)?;
        }
    } else if data.len() >= BLOB_SIZE {
        save.take_blob(&data);
    } else if data.len() == EEPROM_4K || data.len() == EEPROM_MAX {
        save.take(EEPROM, &data, false);
    } else {
        bail!("not an N64 save ({} bytes)", data.len());
    }
    ensure!(!save.is_empty(), "the save holds no data");
    Ok(save)
}

// ── An emulator's files ───────────────────────────────────────────────────────

/// The game's files in the emulator's save folder, in `FILES` order.
type LocalFiles = [Option<PathBuf>; FILES.len()];

fn local_files(files: &N64Files) -> Result<LocalFiles> {
    let mut found = LocalFiles::default();
    for path in files.set.files() {
        let ext = path
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        let Some(i) = FILES.iter().position(|(e, _)| *e == ext) else {
            continue;
        };
        if let Some(other) = &found[i] {
            bail!(
                "two {ext} files for one game: {} and {}",
                other.display(),
                path.display()
            );
        }
        found[i] = Some(path);
    }
    Ok(found)
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    std::fs::File::open(path)
        .with_context(|| format!("reading {}", path.display()))?
        .take(MAX_FILE)
        .read_to_end(&mut data)?;
    Ok(data)
}

fn read_local(files: &N64Files, paths: &LocalFiles) -> Result<Save> {
    let mut save = Save::default();
    for ((_, region), path) in FILES.iter().zip(paths) {
        let Some(path) = path else { continue };
        let data = read_file(path)?;
        match *region {
            PAK1 => save.take_paks(&data),
            r => save.take(r, &data, files.word_swapped),
        }
    }
    Ok(save)
}

/// The game's save as its files hold it; `None` when it has none.
pub fn local_hash(files: &N64Files) -> Result<Option<String>> {
    Ok(read_local(files, &local_files(files)?)?.hash())
}

/// Newest modification time of the game's files.
pub fn local_mtime(files: &N64Files) -> Option<SystemTime> {
    files
        .set
        .files()
        .iter()
        .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .max()
}

/// Write the game's save as the unit to `dest`.
pub fn write_unit(files: &N64Files, dest: &Path) -> Result<()> {
    let save = read_local(files, &local_files(files)?)?;
    save_archive::zip_members(&save.members(), dest)
}

/// Replace the game's files with the save in `unit`, backing up the current
/// ones to `backup` first. A type the unit lacks has its file removed; for
/// the `.mpk`, a pak the unit lacks keeps what is there when the game never
/// used it and is formatted otherwise. Files the emulator hasn't created
/// take the name of the game's other files, else `new_stem`; with neither,
/// nothing is written.
pub fn restore(files: &N64Files, unit: &Path, backup: &Path) -> Result<()> {
    let unit = read_unit(unit)?;
    let paths = local_files(files)?;
    let local = read_local(files, &paths)?;
    let base = paths
        .iter()
        .flatten()
        .next()
        .map(|p| p.with_extension(""))
        .or_else(|| files.new_stem.as_ref().map(|s| files.set.dir.join(s)));
    let target = |i: usize| -> Result<PathBuf> {
        match (&paths[i], &base) {
            (Some(path), _) => Ok(path.clone()),
            (None, Some(base)) => Ok(PathBuf::from(format!("{}.{}", base.display(), FILES[i].0))),
            (None, None) => bail!(
                "the emulator has not created this game's save files yet, and rommd can't \
                 name them; the save is restored once the game has saved"
            ),
        }
    };

    let mut writes: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    for (i, (_, region)) in FILES.iter().enumerate() {
        let data = if *region == PAK1 {
            (PAK1..PAK1 + PAKS).any(|r| unit.0[r].is_some()).then(|| {
                let old = paths[i].as_deref().and_then(|p| read_file(p).ok());
                let mut out = Vec::with_capacity(PAKS * PAK_SIZE);
                for p in 0..PAKS {
                    let r = PAK1 + p;
                    let kept = old
                        .as_deref()
                        .and_then(|o| o.get(p * PAK_SIZE..(p + 1) * PAK_SIZE))
                        .filter(|_| local.0[r].is_none());
                    out.extend(match (&unit.0[r], kept) {
                        (Some(_), _) => unit.give(r, false),
                        (None, Some(kept)) => kept.to_vec(),
                        (None, None) => pak_formatted(),
                    });
                }
                out
            })
        } else {
            unit.0[*region]
                .is_some()
                .then(|| unit.give(*region, files.word_swapped))
        };
        match data {
            Some(data) => writes.push((target(i)?, Some(data))),
            None => writes.extend(paths[i].clone().map(|p| (p, None))),
        }
    }

    let present: Vec<&PathBuf> = paths.iter().flatten().collect();
    if !present.is_empty() {
        std::fs::create_dir_all(backup)?;
        for path in present {
            std::fs::copy(path, backup.join(path.file_name().unwrap_or_default()))?;
        }
    }
    std::fs::create_dir_all(&files.set.dir)?;
    for (path, data) in writes {
        match data {
            None => std::fs::remove_file(&path)?,
            Some(data) if std::fs::read(&path).is_ok_and(|old| old == data) => {}
            Some(data) => {
                // Write then rename so the emulator never sees a half-written save.
                let staged = path.with_file_name(format!(
                    ".{}.rommd-tmp",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
                std::fs::write(&staged, &data)?;
                std::fs::rename(&staged, &path)?;
            }
        }
    }
    Ok(())
}

// ── The ROM ───────────────────────────────────────────────────────────────────

const HEADER_LEN: usize = 0x40;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Order {
    /// Big-endian, the cart's own order (`.z64`).
    Z64,
    /// 16-bit words swapped (`.v64`).
    V64,
    /// 32-bit words reversed (`.n64`).
    N64,
}

/// A dump's byte order, from the cart's first word (the PI domain setting).
fn order_of(first: &[u8]) -> Option<Order> {
    match first.get(..4)? {
        [0x80, 0x37, 0x12, 0x40] => Some(Order::Z64),
        [0x37, 0x80, 0x40, 0x12] => Some(Order::V64),
        [0x40, 0x12, 0x37, 0x80] => Some(Order::N64),
        _ => None,
    }
}

fn to_z64(data: &mut [u8], order: Order) {
    match order {
        Order::Z64 => {}
        Order::V64 => data
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .for_each(|w| w.swap(0, 1)),
        Order::N64 => swap32(data),
    }
}

/// The cart header in big-endian order: name at 0x20 (20 bytes), game code
/// at 0x3B, CRCs at 0x10 and 0x14. `None` for a file that isn't an N64 ROM.
pub fn read_header(rom: &Path) -> Option<[u8; HEADER_LEN]> {
    let mut header = [0u8; HEADER_LEN];
    std::fs::File::open(rom)
        .ok()?
        .read_exact(&mut header)
        .ok()?;
    let order = order_of(&header)?;
    to_z64(&mut header, order);
    Some(header)
}

/// The header's name field: 20 bytes, space or NUL padded.
pub fn header_name(header: &[u8; HEADER_LEN]) -> &[u8] {
    &header[0x20..0x34]
}

/// A digest of the whole ROM in big-endian order, as the emulators name
/// saves after it (mupen64plus: MD5, gopher64: SHA-256); uppercase hex.
pub fn rom_digest<D: Digest>(rom: &Path) -> Option<String> {
    let mut file = std::fs::File::open(rom).ok()?;
    let mut buf = vec![0u8; 1 << 20];
    let mut hasher = D::new();
    let mut order = None;
    loop {
        // Fill whole chunks so a word never straddles two reads.
        let mut got = 0;
        while got < buf.len() {
            match file.read(&mut buf[got..]).ok()? {
                0 => break,
                n => got += n,
            }
        }
        if got == 0 {
            break;
        }
        let order = *order.get_or_insert(order_of(&buf[..got])?);
        to_z64(&mut buf[..got], order);
        hasher.update(&buf[..got]);
        if got < buf.len() {
            break;
        }
    }
    order?;
    Some(
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::FileSet;
    use crate::testing::{TempDir, write};

    fn files(dir: &Path, word_swapped: bool) -> N64Files {
        N64Files {
            set: FileSet::tagged(dir.to_owned(), "-12345678", &["eep", "sra", "fla", "mpk"])
                .unwrap(),
            new_stem: Some("GAME-12345678".into()),
            word_swapped,
        }
    }

    fn eeprom_4k() -> Vec<u8> {
        let mut e = vec![0xFF; EEPROM_MAX];
        e[..4].copy_from_slice(b"SAVE");
        e
    }

    fn sram_bus() -> Vec<u8> {
        let mut s = vec![0xFF; SRAM_SIZE];
        s[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        s
    }

    fn unit_zip(dir: &Path, members: &[(String, Vec<u8>)]) -> PathBuf {
        let path = dir.join("unit.zip");
        save_archive::zip_members(members, &path).unwrap();
        path
    }

    #[test]
    fn eeprom_is_4k_unless_written_past_it() {
        let mut save = Save::default();
        save.take(EEPROM, &eeprom_4k(), false);
        assert_eq!(save.0[EEPROM].as_ref().unwrap().len(), EEPROM_4K);
        let mut e = eeprom_4k();
        e[0x300] = 7;
        save.take(EEPROM, &e, false);
        assert_eq!(save.0[EEPROM].as_ref().unwrap().len(), EEPROM_MAX);
        save.take(EEPROM, &[0xFF; EEPROM_MAX], false);
        assert!(save.is_empty());
    }

    #[test]
    fn a_formatted_pak_is_unused_until_it_lists_a_note() {
        let mut pak = pak_formatted();
        assert!(!pak_used(&pak));
        pak[0x10A..0x10C].copy_from_slice(&[0x00, 0x05]);
        assert!(pak_used(&pak));
    }

    #[test]
    fn mupen_and_gopher64_files_give_the_same_unit() {
        let root = TempDir::new("n64-same-unit");
        let (mupen, gopher) = (root.path().join("m"), root.path().join("g"));
        let mut swapped = sram_bus();
        swap32(&mut swapped);
        write(&mupen.join("GAME-12345678.sra"), &swapped);
        write(&gopher.join("GAME-12345678.sra"), &sram_bus());
        let (m, g) = (files(&mupen, true), files(&gopher, false));
        let hash = local_hash(&m).unwrap();
        assert!(hash.is_some());
        assert_eq!(hash, local_hash(&g).unwrap());

        let unit = mupen.join("unit.zip");
        write_unit(&m, &unit).unwrap();
        let save = read_unit(&unit).unwrap();
        assert_eq!(save.members(), vec![("sram".to_owned(), sram_bus())]);
        assert_eq!(save.hash(), hash);
    }

    #[test]
    fn no_save_until_the_game_writes() {
        let root = TempDir::new("n64-empty");
        write(&root.path().join("GAME-12345678.sra"), &[0xFF; SRAM_SIZE]);
        write(
            &root.path().join("GAME-12345678.mpk"),
            &pak_formatted().repeat(4),
        );
        assert_eq!(local_hash(&files(root.path(), true)).unwrap(), None);
    }

    #[test]
    fn restores_into_new_files_and_removes_unused_types() {
        let root = TempDir::new("n64-restore");
        let dir = root.path().join("saves");
        write(&dir.join("GAME-12345678.eep"), &eeprom_4k());
        write(&dir.join("Other-87654321.sra"), &sram_bus());
        let unit = unit_zip(root.path(), &[("sram".into(), sram_bus())]);

        let set = files(&dir, true);
        restore(&set, &unit, &root.path().join("backup")).unwrap();
        let mut swapped = sram_bus();
        swap32(&mut swapped);
        assert_eq!(
            std::fs::read(dir.join("GAME-12345678.sra")).unwrap(),
            swapped
        );
        assert!(!dir.join("GAME-12345678.eep").exists());
        assert!(root.path().join("backup/GAME-12345678.eep").is_file());
        assert_eq!(
            std::fs::read(dir.join("Other-87654321.sra")).unwrap(),
            sram_bus()
        );
        assert_eq!(
            local_hash(&set).unwrap(),
            save_archive::members_hash(&[("sram".into(), sram_bus())])
        );
    }

    #[test]
    fn new_files_take_the_name_of_the_games_files() {
        let root = TempDir::new("n64-restore-name");
        let dir = root.path().join("saves");
        let mut pak = pak_formatted();
        pak[0x10A..0x10C].copy_from_slice(&[0x00, 0x05]);
        pak[0x300] = 0x42;
        write(&dir.join("Database Name-12345678.mpk"), &pak.repeat(4));
        let unit = unit_zip(
            root.path(),
            &[("eeprom".into(), eeprom_4k()[..EEPROM_4K].to_vec())],
        );

        let set = files(&dir, true);
        restore(&set, &unit, &root.path().join("backup")).unwrap();
        let eep = std::fs::read(dir.join("Database Name-12345678.eep")).unwrap();
        assert_eq!(eep.len(), EEPROM_MAX);
        assert!(!dir.join("Database Name-12345678.mpk").exists());
        assert!(!dir.join("GAME-12345678.eep").exists());
    }

    #[test]
    fn a_restored_pak_keeps_the_unused_paks_there() {
        let root = TempDir::new("n64-paks");
        let dir = root.path().join("saves");
        let mut old = pak_formatted();
        old[0] = 0x81; // gopher64's label area
        write(&dir.join("GAME-12345678.mpk"), &old.repeat(4));
        let mut pak = pak_formatted();
        pak[0x10A..0x10C].copy_from_slice(&[0x00, 0x05]);
        pak[0x300] = 0x42;
        let unit = unit_zip(root.path(), &[("pak2".into(), pak.clone())]);

        restore(&files(&dir, false), &unit, &root.path().join("backup")).unwrap();
        let mpk = std::fs::read(dir.join("GAME-12345678.mpk")).unwrap();
        assert_eq!(mpk.len(), PAKS * PAK_SIZE);
        assert_eq!(&mpk[..PAK_SIZE], &old[..]);
        assert_eq!(&mpk[PAK_SIZE..2 * PAK_SIZE], &pak[..]);
    }

    #[test]
    fn refuses_to_guess_a_name() {
        let root = TempDir::new("n64-no-name");
        let unit = unit_zip(root.path(), &[("sram".into(), sram_bus())]);
        let mut set = files(&root.path().join("saves"), true);
        set.new_stem = None;
        assert!(restore(&set, &unit, &root.path().join("backup")).is_err());
        assert!(set.set.files().is_empty());
    }

    #[test]
    fn reads_saves_from_before_sigil() {
        let root = TempDir::new("n64-legacy");
        let mut swapped = sram_bus();
        swap32(&mut swapped);

        let mut srm = vec![0xFF; BLOB_SIZE];
        for p in 0..PAKS {
            let at = BLOB_OFFSETS[PAK1 + p];
            srm[at..at + PAK_SIZE].copy_from_slice(&pak_formatted());
        }
        srm[BLOB_OFFSETS[SRAM]..BLOB_OFFSETS[SRAM] + SRAM_SIZE].copy_from_slice(&swapped);
        write(&root.path().join("game.srm"), &srm);
        let save = read_unit(&root.path().join("game.srm")).unwrap();
        assert_eq!(save.members(), vec![("sram".to_owned(), sram_bus())]);

        let zip = unit_zip(
            root.path(),
            &[
                ("GAME-12345678.sra".into(), swapped),
                ("GAME_Cont_2.mpk".into(), {
                    let mut pak = pak_formatted();
                    pak[0x10A..0x10C].copy_from_slice(&[0x00, 0x05]);
                    pak
                }),
            ],
        );
        let names: Vec<_> = read_unit(&zip)
            .unwrap()
            .members()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, ["pak2", "sram"]);

        write(&root.path().join("lone.eep"), &eeprom_4k()[..EEPROM_4K]);
        assert!(read_unit(&root.path().join("lone.eep")).is_ok());
        write(&root.path().join("odd.bin"), &[1; 0x8000]);
        assert!(read_unit(&root.path().join("odd.bin")).is_err());
    }

    #[test]
    fn rom_digest_is_over_the_big_endian_order() {
        let root = TempDir::new("n64-rom");
        let mut z64 = vec![0u8; 0x1000];
        z64[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
        z64[0x20..0x25].copy_from_slice(b"HELLO");
        for (i, b) in z64.iter_mut().enumerate().skip(0x40) {
            *b = i as u8;
        }
        let mut v64 = z64.clone();
        to_z64(&mut v64, Order::V64); // the swaps are their own inverses
        let mut n64 = z64.clone();
        swap32(&mut n64);
        for (name, data) in [("a.z64", &z64), ("a.v64", &v64), ("a.n64", &n64)] {
            write(&root.path().join(name), data);
        }
        let md5 = rom_digest::<md5::Md5>(&root.path().join("a.z64")).unwrap();
        assert_eq!(md5, format!("{:X}", md5::Md5::digest(&z64)));
        for name in ["a.v64", "a.n64"] {
            let path = root.path().join(name);
            assert_eq!(rom_digest::<md5::Md5>(&path).unwrap(), md5);
            assert_eq!(
                header_name(&read_header(&path).unwrap()),
                b"HELLO\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0"
            );
        }
        write(&root.path().join("not.z64"), b"nope nope nope");
        assert_eq!(rom_digest::<md5::Md5>(&root.path().join("not.z64")), None);
    }
}
