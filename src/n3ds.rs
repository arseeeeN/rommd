//! 3DS title ID extraction. Every supported container carries the title ID in
//! plaintext, so no keys are needed.

use anyhow::{Context, Result};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use tracing::warn;

const APPLICATION: u32 = 0x0004_0000;
const UPDATE: u32 = 0x0004_000E;
const DLC: u32 = 0x0004_008C;

/// The application title ID (16 lowercase hex) of a 3DS ROM. Filenames are
/// never used. Update and DLC titles map to their base application, since
/// saves belong to it.
pub fn title_id_for_rom(rom_path: &Path) -> Option<String> {
    let ext = rom_path.extension()?.to_str()?.to_ascii_lowercase();
    let result = match ext.as_str() {
        "3ds" | "cci" => read_at(rom_path, 0x100, b"NCSD", 0x108),
        "cxi" | "app" => read_at(rom_path, 0x100, b"NCCH", 0x118),
        "cia" => read_cia(rom_path),
        other => {
            warn!(
                "cannot read a 3DS title ID from .{} files (supported: 3ds, cci, cxi, cia); \
                 save sync unavailable for {}",
                other,
                rom_path.display()
            );
            return None;
        }
    };
    match result {
        Ok(id) => Some(format!("{:016x}", base_title(id))),
        Err(e) => {
            warn!(
                "could not read the title ID of {}: {:#}",
                rom_path.display(),
                e
            );
            None
        }
    }
}

fn base_title(id: u64) -> u64 {
    match (id >> 32) as u32 {
        UPDATE | DLC => (u64::from(APPLICATION) << 32) | (id & 0xFFFF_FFFF),
        _ => id,
    }
}

/// NCSD (cartridge image) and NCCH (single program) headers: magic at 0x100
/// and the title / program ID as u64 LE at a fixed offset.
fn read_at(path: &Path, magic_at: u64, magic: &[u8; 4], id_at: u64) -> Result<u64> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = [0u8; 4];
    f.seek(SeekFrom::Start(magic_at))?;
    f.read_exact(&mut buf)?;
    anyhow::ensure!(
        &buf == magic,
        "no {} header (encrypted or compressed image?)",
        String::from_utf8_lossy(magic)
    );
    let mut id = [0u8; 8];
    f.seek(SeekFrom::Start(id_at))?;
    f.read_exact(&mut id)?;
    Ok(u64::from_le_bytes(id))
}

/// CIA: header, then certificate chain, ticket and TMD, each aligned to 64
/// bytes. The TMD's title ID (u64 BE) sits at 0x4C into its header, which
/// follows a signature whose size depends on the signature type.
fn read_cia(path: &Path) -> Result<u64> {
    let mut f = std::fs::File::open(path)?;
    let mut header = [0u8; 0x20];
    f.read_exact(&mut header)?;
    let le = |at: usize| u64::from(u32::from_le_bytes(header[at..at + 4].try_into().unwrap()));
    let align = |n: u64| n.div_ceil(64) * 64;
    let (header_size, cert, ticket) = (le(0x00), le(0x08), le(0x0C));
    anyhow::ensure!(
        header_size == 0x2020,
        "not a CIA (header size {:#x})",
        header_size
    );
    let tmd = align(align(align(header_size) + cert) + ticket);

    let mut sig_type = [0u8; 4];
    f.seek(SeekFrom::Start(tmd))?;
    f.read_exact(&mut sig_type)?;
    let body = 4 + match u32::from_be_bytes(sig_type) {
        0x0001_0000 | 0x0001_0003 => 0x200 + 0x3C,
        0x0001_0001 | 0x0001_0004 => 0x100 + 0x3C,
        0x0001_0002 | 0x0001_0005 => 0x3C + 0x40,
        other => anyhow::bail!("unknown TMD signature type {:#x}", other),
    };
    let mut id = [0u8; 8];
    f.seek(SeekFrom::Start(tmd + body + 0x4C))
        .context("seeking to the TMD title ID")?;
    f.read_exact(&mut id)?;
    Ok(u64::from_be_bytes(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TempDir, write};

    #[test]
    fn reads_cartridge_and_program_headers() {
        let dir = TempDir::new("n3ds");
        let mut cci = vec![0u8; 0x200];
        cci[0x100..0x104].copy_from_slice(b"NCSD");
        cci[0x108..0x110].copy_from_slice(&0x0004_0000_0017_4400u64.to_le_bytes());
        write(&dir.path().join("psmd.cci"), &cci);
        assert_eq!(
            title_id_for_rom(&dir.path().join("psmd.cci")).as_deref(),
            Some("0004000000174400")
        );

        let mut cxi = vec![0u8; 0x200];
        cxi[0x100..0x104].copy_from_slice(b"NCCH");
        cxi[0x118..0x120].copy_from_slice(&0x0004_000E_000B_A900u64.to_le_bytes());
        write(&dir.path().join("update.cxi"), &cxi);
        assert_eq!(
            title_id_for_rom(&dir.path().join("update.cxi")).as_deref(),
            Some("00040000000ba900")
        );

        write(&dir.path().join("encrypted.3ds"), &vec![0u8; 0x200]);
        assert_eq!(title_id_for_rom(&dir.path().join("encrypted.3ds")), None);
    }

    #[test]
    fn reads_the_cia_tmd() {
        let dir = TempDir::new("cia");
        let (cert, ticket) = (0xA00u32, 0x350u32);
        let mut cia = vec![0u8; 0x2020];
        cia[0x00..0x04].copy_from_slice(&0x2020u32.to_le_bytes());
        cia[0x08..0x0C].copy_from_slice(&cert.to_le_bytes());
        cia[0x0C..0x10].copy_from_slice(&ticket.to_le_bytes());
        let align = |n: usize| n.div_ceil(64) * 64;
        let tmd = align(align(align(0x2020) + cert as usize) + ticket as usize);
        cia.resize(tmd + 0x400, 0);
        cia[tmd..tmd + 4].copy_from_slice(&0x0001_0004u32.to_be_bytes());
        let id_at = tmd + 4 + 0x100 + 0x3C + 0x4C;
        cia[id_at..id_at + 8].copy_from_slice(&0x0004_0000_0005_5D00u64.to_be_bytes());
        write(&dir.path().join("x.cia"), &cia);
        assert_eq!(
            title_id_for_rom(&dir.path().join("x.cia")).as_deref(),
            Some("0004000000055d00")
        );
    }
}
