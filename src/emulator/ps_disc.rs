//! PlayStation and PlayStation 2 disc images (ISO 9660, cooked or raw
//! sectors): the serial from `SYSTEM.CNF`, and for PCSX2 the CRC of the boot
//! program. Compressed images (CHD, CSO) are not read.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const RAW_SECTOR: u64 = 2352;
const COOKED_SECTOR: u64 = 2048;
// MODE2/2352: 12 sync + 3 address + 1 mode + 8 subheader = 24 bytes before user data.
const RAW_HEADER: u64 = 24;

/// Upper bound for a boot program read whole; real ones are a few MB.
const MAX_BOOT_FILE: u64 = 64 << 20;

/// Extract the disc serial (e.g. `"SLPS-00017"`) from a PS1/PS2 disc image.
/// Follows `.m3u` → `.cue` → `.bin` chains automatically.
/// Returns `None` if the image format is unrecognised or `SYSTEM.CNF` is absent.
pub fn read_serial(path: &Path) -> Option<String> {
    let bin = resolve_to_bin(path)?;
    let raw = is_raw_bin(&bin)?;
    let mut f = File::open(&bin).ok()?;
    let cnf = read_system_cnf(&mut f, raw)?;
    parse_serial(&cnf)
}

/// A PS2 disc's serial and the CRC of the program its `BOOT2` line starts,
/// which PCSX2 names save states and per-game settings after
/// (`SLUS-20811 (CB99CD12).01.p2s`). The CRC is PCSX2's
/// `ElfObject::GetCRC`: the XOR of the file's little-endian 32-bit words.
pub fn read_ps2_boot(path: &Path) -> Option<(String, u32)> {
    let bin = resolve_to_bin(path)?;
    let raw = is_raw_bin(&bin)?;
    let mut f = File::open(&bin).ok()?;
    let cnf = read_system_cnf(&mut f, raw)?;
    let boot = boot2_path(&cnf)?;
    let serial = normalize_serial(boot.rsplit(['\\', '/']).next()?.split(';').next()?)?;

    let mut dir = read_root(&mut f, raw)?;
    let mut parts = boot.split(['\\', '/']).filter(|p| !p.is_empty()).peekable();
    while let Some(part) = parts.next() {
        let (lba, size) = find_in_dir(&dir, part)?;
        if parts.peek().is_none() {
            if size > MAX_BOOT_FILE {
                return None;
            }
            let data = read_sectors(&mut f, lba, size, raw)?;
            return Some((serial, elf_crc(&data)));
        }
        dir = read_sectors(&mut f, lba, size, raw)?;
    }
    None
}

fn elf_crc(data: &[u8]) -> u32 {
    data.as_chunks::<4>()
        .0
        .iter()
        .fold(0, |crc, w| crc ^ u32::from_le_bytes(*w))
}

// ── Resolution ────────────────────────────────────────────────────────────────

/// Walk `.m3u` → `.cue` → `.bin`/`.iso` until we reach a disc image.
fn resolve_to_bin(path: &Path) -> Option<PathBuf> {
    match path.extension()?.to_str()? {
        "m3u" => {
            let text = std::fs::read_to_string(path).ok()?;
            let first = text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('#'))?;
            resolve_to_bin(&path.parent()?.join(first))
        }
        "cue" => {
            let text = std::fs::read_to_string(path).ok()?;
            for line in text.lines() {
                if !line.trim_start().to_ascii_uppercase().starts_with("FILE") {
                    continue;
                }
                let after = line.trim_start()[4..].trim();
                let name = if after.starts_with('"') {
                    after.trim_start_matches('"').split('"').next()?
                } else {
                    after.split_ascii_whitespace().next()?
                };
                let candidate = path.parent()?.join(name);
                if candidate.exists() {
                    return Some(candidate);
                }
                // Fallback: CUE references a differently-named BIN — take the
                // first .bin in the same directory.
                return first_bin_in_dir(path.parent()?);
            }
            None
        }
        "bin" | "iso" => Some(path.to_owned()),
        _ => None,
    }
}

fn first_bin_in_dir(dir: &Path) -> Option<PathBuf> {
    let mut rd = std::fs::read_dir(dir).ok()?;
    while let Some(Ok(entry)) = rd.next() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("bin") {
            return Some(p);
        }
    }
    None
}

// ── Format detection ──────────────────────────────────────────────────────────

fn is_raw_bin(path: &Path) -> Option<bool> {
    let mut f = File::open(path).ok()?;
    let mut hdr = [0u8; 12];
    f.read_exact(&mut hdr).ok()?;
    // Raw BIN starts with the CD-ROM sync pattern.
    Some(
        hdr == [
            0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00,
        ],
    )
}

// ── ISO 9660 reader ───────────────────────────────────────────────────────────

/// The root directory's records.
fn read_root(f: &mut File, raw: bool) -> Option<Vec<u8>> {
    // Primary Volume Descriptor lives at logical sector 16.
    let pvd = read_sectors(f, 16, COOKED_SECTOR, raw)?;
    if pvd[0] != 1 || &pvd[1..6] != b"CD001" {
        return None;
    }

    // Root directory record is embedded in the PVD at byte offset 156.
    let root_lba = u32::from_le_bytes(pvd[158..162].try_into().ok()?) as u64;
    let root_len = u32::from_le_bytes(pvd[166..170].try_into().ok()?) as u64;
    read_sectors(f, root_lba, root_len, raw)
}

fn read_system_cnf(f: &mut File, raw: bool) -> Option<String> {
    let root = read_root(f, raw)?;
    let (cnf_lba, _) = find_in_dir(&root, "SYSTEM.CNF")?;

    // SYSTEM.CNF is a handful of bytes — one sector is always enough.
    let data = read_sectors(f, cnf_lba, COOKED_SECTOR, raw)?;
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8(data[..end].to_vec()).ok()
}

/// Read `count` bytes of logical data starting at `lba`, skipping raw sector
/// headers when the image is in raw (2352-byte sector) format.
fn read_sectors(f: &mut File, lba: u64, count: u64, raw: bool) -> Option<Vec<u8>> {
    let ss = if raw { RAW_SECTOR } else { COOKED_SECTOR };
    let ho = if raw { RAW_HEADER } else { 0 };
    let sectors = count.div_ceil(COOKED_SECTOR);
    let mut out = Vec::with_capacity(count as usize);

    for i in 0..sectors {
        f.seek(SeekFrom::Start((lba + i) * ss + ho)).ok()?;
        let to_read = (count - i * COOKED_SECTOR).min(COOKED_SECTOR) as usize;
        let mut buf = vec![0u8; to_read];
        f.read_exact(&mut buf).ok()?;
        out.extend_from_slice(&buf);
    }
    Some(out)
}

/// Walk ISO 9660 directory records and return the LBA and size of `target`
/// (case-insensitive, version suffix ignored on both sides).
fn find_in_dir(dir: &[u8], target: &str) -> Option<(u64, u64)> {
    let target = target.split(';').next().unwrap_or(target);
    let mut pos = 0usize;
    while pos < dir.len() {
        let len = dir[pos] as usize;
        if len == 0 {
            // Zero-length record: padding to the next 2048-byte sector boundary.
            let next = (pos / COOKED_SECTOR as usize + 1) * COOKED_SECTOR as usize;
            if next >= dir.len() {
                break;
            }
            pos = next;
            continue;
        }
        if pos + len > dir.len() {
            break;
        }
        let rec = &dir[pos..pos + len];
        let name_len = rec[32] as usize;
        if name_len > 0
            && pos + 33 + name_len <= dir.len()
            && let Ok(name) = std::str::from_utf8(&rec[33..33 + name_len])
        {
            // Strip ISO 9660 version suffix (";1").
            let name = name.split(';').next().unwrap_or(name);
            if name.eq_ignore_ascii_case(target) {
                let lba = u32::from_le_bytes(rec.get(2..6)?.try_into().ok()?) as u64;
                let size = u32::from_le_bytes(rec.get(10..14)?.try_into().ok()?) as u64;
                return Some((lba, size));
            }
        }
        pos += len;
    }
    None
}

// ── SYSTEM.CNF parser ─────────────────────────────────────────────────────────

fn parse_serial(cnf: &str) -> Option<String> {
    for line in cnf.lines() {
        let line = line.trim();
        // Match "BOOT = cdrom:\SLPS_000.17;1" and "BOOT2 = cdrom0:\SLES_012.34;1"
        let rest = if let Some(r) = line.strip_prefix("BOOT2") {
            r
        } else if let Some(r) = line.strip_prefix("BOOT") {
            r
        } else {
            continue;
        };
        let value = rest.split_once('=')?.1.trim();
        // Take the last path component, strip the version suffix.
        let file = value.split(['\\', '/']).next_back()?.split(';').next()?;
        return normalize_serial(file);
    }
    None
}

/// The path after the device in the `BOOT2` line (`cdrom0:\\SLUS_208.11;1`
/// gives `\\SLUS_208.11;1`).
fn boot2_path(cnf: &str) -> Option<&str> {
    cnf.lines().find_map(|line| {
        let value = line
            .trim()
            .strip_prefix("BOOT2")?
            .trim_start()
            .strip_prefix('=')?;
        let value = value.trim();
        Some(value.split_once(':').map_or(value, |(_, p)| p))
    })
}

/// `"SLPS_000.17"` → `"SLPS-00017"`
fn normalize_serial(raw: &str) -> Option<String> {
    let (prefix, rest) = raw.split_once('_')?;
    Some(format!(
        "{}-{}",
        prefix.trim(),
        rest.replace('.', "").trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_normalization() {
        assert_eq!(normalize_serial("SLPS_000.17"), Some("SLPS-00017".into()));
        assert_eq!(normalize_serial("SLUS_002.54"), Some("SLUS-00254".into()));
        assert_eq!(normalize_serial("SLES_012.34"), Some("SLES-01234".into()));
        assert_eq!(normalize_serial("SCUS_944.25"), Some("SCUS-94425".into()));
    }

    #[test]
    fn cnf_parsing() {
        let cnf = "BOOT = cdrom:\\SLPS_000.17;1\nTCBP = 4\n";
        assert_eq!(parse_serial(cnf), Some("SLPS-00017".into()));

        let cnf2 = "BOOT2 = cdrom0:\\SLUS_213.56;1\n";
        assert_eq!(parse_serial(cnf2), Some("SLUS-21356".into()));
        assert_eq!(boot2_path(cnf2), Some("\\SLUS_213.56;1"));
        assert_eq!(boot2_path(cnf), None);
    }

    /// A cooked ISO with the boot program in a subfolder, as some discs have.
    #[test]
    fn ps2_boot_program_crc() {
        let elf: Vec<u8> = [0x464c457fu32, 0x00010101, 0x12345678]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .chain([0xAA, 0xBB]) // a trailing partial word is ignored
            .collect();
        let cnf = b"BOOT2 = cdrom0:\\GAME\\SLES_525.85;1\r\nVER = 1.01\r\n";

        let record = |name: &[u8], lba: u32, size: u32| {
            let mut r = vec![0u8; 33 + name.len() + (name.len() + 1) % 2];
            r[0] = r.len() as u8;
            r[2..6].copy_from_slice(&lba.to_le_bytes());
            r[10..14].copy_from_slice(&size.to_le_bytes());
            r[32] = name.len() as u8;
            r[33..33 + name.len()].copy_from_slice(name);
            r
        };
        let mut iso = vec![0u8; 21 * 2048];
        let pvd = &mut iso[16 * 2048..17 * 2048];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[158..162].copy_from_slice(&17u32.to_le_bytes());
        pvd[166..170].copy_from_slice(&2048u32.to_le_bytes());
        let root = [
            record(b"SYSTEM.CNF;1", 18, cnf.len() as u32),
            record(b"GAME", 19, 2048),
        ]
        .concat();
        iso[17 * 2048..17 * 2048 + root.len()].copy_from_slice(&root);
        iso[18 * 2048..18 * 2048 + cnf.len()].copy_from_slice(cnf);
        let game = record(b"SLES_525.85;1", 20, elf.len() as u32);
        iso[19 * 2048..19 * 2048 + game.len()].copy_from_slice(&game);
        iso[20 * 2048..20 * 2048 + elf.len()].copy_from_slice(&elf);

        let dir = crate::testing::TempDir::new("ps2-boot");
        let path = dir.path().join("game.iso");
        std::fs::write(&path, &iso).unwrap();
        assert_eq!(
            read_ps2_boot(&path),
            Some(("SLES-52585".into(), 0x464c457f ^ 0x00010101 ^ 0x12345678))
        );
        assert_eq!(read_serial(&path).as_deref(), Some("SLES-52585"));
    }
}
