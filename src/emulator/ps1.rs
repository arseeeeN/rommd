use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const RAW_SECTOR: u64 = 2352;
const COOKED_SECTOR: u64 = 2048;
// MODE2/2352: 12 sync + 3 address + 1 mode + 8 subheader = 24 bytes before user data.
const RAW_HEADER: u64 = 24;

/// Extract the disc serial (e.g. `"SLPS-00017"`) from a PS1/PS2 disc image.
/// Follows `.m3u` → `.cue` → `.bin` chains automatically.
/// Returns `None` if the image format is unrecognised or `SYSTEM.CNF` is absent.
pub fn read_serial(path: &Path) -> Option<String> {
    let bin = resolve_to_bin(path)?;
    let raw = is_raw_bin(&bin)?;
    let cnf = read_system_cnf(&bin, raw)?;
    parse_serial(&cnf)
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
    Some(hdr == [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00])
}

// ── ISO 9660 reader ───────────────────────────────────────────────────────────

fn read_system_cnf(path: &Path, raw: bool) -> Option<String> {
    let mut f = File::open(path).ok()?;

    // Primary Volume Descriptor lives at logical sector 16.
    let pvd = read_sectors(&mut f, 16, COOKED_SECTOR, raw)?;
    if pvd[0] != 1 || &pvd[1..6] != b"CD001" {
        return None;
    }

    // Root directory record is embedded in the PVD at byte offset 156.
    let root_lba = u32::from_le_bytes(pvd[158..162].try_into().ok()?) as u64;
    let root_len = u32::from_le_bytes(pvd[166..170].try_into().ok()?) as u64;

    let root = read_sectors(&mut f, root_lba, root_len, raw)?;
    let cnf_lba = find_in_dir(&root, "SYSTEM.CNF")?;

    // SYSTEM.CNF is a handful of bytes — one sector is always enough.
    let data = read_sectors(&mut f, cnf_lba, COOKED_SECTOR, raw)?;
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

/// Walk ISO 9660 directory records and return the LBA of `target` (case-insensitive).
fn find_in_dir(dir: &[u8], target: &str) -> Option<u64> {
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
        if name_len > 0 && pos + 33 + name_len <= dir.len() {
            if let Ok(name) = std::str::from_utf8(&rec[33..33 + name_len]) {
                // Strip ISO 9660 version suffix (";1").
                let name = name.split(';').next().unwrap_or(name);
                if name.eq_ignore_ascii_case(target) {
                    let lba = u32::from_le_bytes(rec[2..6].try_into().ok()?) as u64;
                    return Some(lba);
                }
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
        let value = rest.splitn(2, '=').nth(1)?.trim();
        // Take the last path component, strip the version suffix.
        let file = value.split(['\\', '/']).last()?.split(';').next()?;
        return normalize_serial(file);
    }
    None
}

/// `"SLPS_000.17"` → `"SLPS-00017"`
fn normalize_serial(raw: &str) -> Option<String> {
    let (prefix, rest) = raw.split_once('_')?;
    Some(format!("{}-{}", prefix.trim(), rest.replace('.', "").trim()))
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
    }
}

