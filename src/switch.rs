use anyhow::{Context, Result};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use tracing::{debug, warn};

struct Pfs0Entry {
    data_offset: u64,
    data_size: u64,
    name_offset: u32,
}

/// Parse the PFS0 (NSP) directory and return (entries, string_table, data_base).
/// `data_base` is the absolute file offset where file payloads begin.
fn read_pfs0_index(f: &mut std::fs::File) -> Result<(Vec<Pfs0Entry>, Vec<u8>, u64)> {
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)?;
    anyhow::ensure!(&magic == b"PFS0", "not a PFS0/NSP file (magic {:?})", magic);

    let num_files = read_u32_le(f)?;
    let string_table_size = read_u32_le(f)?;
    let _reserved = read_u32_le(f)?;

    let mut entries = Vec::with_capacity(num_files as usize);
    for _ in 0..num_files {
        let data_offset = read_u64_le(f)?;
        let data_size = read_u64_le(f)?;
        let name_offset = read_u32_le(f)?;
        let _reserved = read_u32_le(f)?;
        entries.push(Pfs0Entry { data_offset, data_size, name_offset });
    }

    let mut string_table = vec![0u8; string_table_size as usize];
    f.read_exact(&mut string_table)?;

    let data_base = f.stream_position()?;
    Ok((entries, string_table, data_base))
}

/// Emulators whose save directories hold the same raw save filesystem contents
/// for a title, so a save made in one can be restored into any other.
pub const SAVE_COMPATIBLE_EMULATORS: &[&str] =
    &["eden", "ryujinx", "yuzu", "citron", "sudachi", "suyu"];

/// Titles that keep a single save per console (Switch "device" save data)
/// instead of one per user. Only consulted when no save exists locally yet —
/// otherwise the save type found on disk decides. Same list as argosy-launcher.
pub const DEVICE_SAVE_TITLES: &[&str] = &[
    "01006F8002326000", // Animal Crossing: New Horizons
    "0100D2F00D5C0000", // Nintendo Switch Sports
    "01000320000CC000", // 1-2-Switch
    "01002FF008C24000", // Ring Fit Adventure
    "0100C4B0034B2000", // Nintendo Labo Toy-Con 01: Variety Kit
    "01009AB0034E0000", // Nintendo Labo Toy-Con 02: Robot Kit
    "01001E9003502000", // Nintendo Labo Toy-Con 03: Vehicle Kit
    "0100165003504000", // Nintendo Labo Toy-Con 04: VR Kit
    "0100C1800A9B6000", // Go Vacation
];

/// Read the application title ID (16 lowercase hex chars) from an NSP. Filenames
/// are deliberately not used — they can be wrong and would silently scope saves
/// to the wrong game.
///
/// Tier 1 is the ticket (plaintext, no keys); tier 2 decrypts the `.cnmt.nca`
/// header with `header_key` from `prod_keys`. Update NSPs (`...800`) are mapped
/// to their base title, since saves always belong to the base application.
pub fn title_id_for_rom(rom_path: &Path, prod_keys: &Path) -> Option<String> {
    let ext = rom_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if ext != "nsp" {
        warn!(
            "cannot read title ID from .{} files — only NSP is supported; \
             save scoping unavailable for {}",
            ext,
            rom_path.display()
        );
        return None;
    }

    let id = match read_title_id_from_ticket(rom_path) {
        Ok(id) => id,
        Err(e) => {
            debug!("ticket read failed for {} ({}), trying NCA header", rom_path.display(), e);
            if !prod_keys.exists() {
                warn!(
                    "prod.keys not found at {} — title ID unavailable for {}",
                    prod_keys.display(),
                    rom_path.display()
                );
                return None;
            }
            match read_title_id_from_nsp(rom_path, prod_keys) {
                Ok(id) => id,
                Err(e) => {
                    warn!("could not read title ID from {}: {:#}", rom_path.display(), e);
                    return None;
                }
            }
        }
    };
    Some(format!("{:016x}", base_title_id(id)))
}

fn base_title_id(id: u64) -> u64 {
    if id & 0xFFF == 0x800 { id & !0xFFF } else { id }
}

/// Extract the Switch title ID from an NSP by reading the ticket (`.tik`) file.
///
/// The ticket's Rights ID is in plaintext at a fixed offset; its first 8 bytes
/// are the title ID (big-endian). No cryptographic keys are required.
pub fn read_title_id_from_ticket(nsp_path: &Path) -> Result<u64> {
    let mut f = std::fs::File::open(nsp_path)
        .with_context(|| format!("opening {}", nsp_path.display()))?;

    let (entries, string_table, data_base) = read_pfs0_index(&mut f)?;

    for entry in &entries {
        if !cstr_at(&string_table, entry.name_offset as usize).ends_with(".tik") {
            continue;
        }
        // Need at least enough bytes to reach the Rights ID.
        // For RSA-2048 tickets (most common), Rights ID is at 0x1A8.
        let read_len = entry.data_size.min(0x200) as usize;
        anyhow::ensure!(read_len >= 0x1C0, "ticket too small ({} bytes)", read_len);
        let mut tik = vec![0u8; read_len];
        f.seek(SeekFrom::Start(data_base + entry.data_offset))?;
        f.read_exact(&mut tik)?;

        // Body offset depends on the signature type prepended to the ticket.
        // The Rights ID is at body_offset + 0x68 (16 bytes); title ID = first 8 bytes (BE).
        let sig_type = u32::from_le_bytes(tik[0..4].try_into().unwrap());
        let body_offset: usize = match sig_type {
            0x00010000 | 0x00010003 => 4 + 512 + 60, // RSA-4096 (SHA-1 / SHA-256)
            0x00010001 | 0x00010004 => 4 + 256 + 60, // RSA-2048 (SHA-1 / SHA-256); Switch retail uses 0x00010004
            0x00010002 | 0x00010005 => 4 + 60 + 64,  // ECDSA (SHA-1 / SHA-256)
            _ => anyhow::bail!("unsupported ticket signature type: {:#010x}", sig_type),
        };
        let rights_id_offset = body_offset + 0x68;
        anyhow::ensure!(
            read_len >= rights_id_offset + 8,
            "ticket too small for rights ID (need {} bytes, have {})",
            rights_id_offset + 8,
            read_len
        );
        let title_id =
            u64::from_be_bytes(tik[rights_id_offset..rights_id_offset + 8].try_into().unwrap());
        anyhow::ensure!(
            (title_id >> 56) == 0x01,
            "rights ID title ID doesn't look like a valid Switch title: {:#018x}",
            title_id
        );
        return Ok(title_id);
    }

    anyhow::bail!("no .tik file found in NSP")
}

/// Extract the Switch title ID from an NSP file via NCA header decryption.
///
/// Strategy:
/// 1. Parse the PFS0 container to find the `.cnmt.nca` (content meta NCA).
/// 2. Decrypt NCA header sector 1 with `header_key` from prod.keys (AES-XTS).
/// 3. Read the title ID (u64 LE) at decrypted offset 0x10.
///
/// This is a fallback for ticketless NSPs. Requires a fully derived `header_key`
/// in prod.keys (exported by tools like Lockpick_RCM with key derivation enabled).
pub fn read_title_id_from_nsp(nsp_path: &Path, prod_keys_path: &Path) -> Result<u64> {
    let header_key = parse_header_key(prod_keys_path)
        .with_context(|| format!("reading header_key from {}", prod_keys_path.display()))?;
    let nca_header = extract_cnmt_nca_header(nsp_path)
        .with_context(|| format!("locating .cnmt.nca in {}", nsp_path.display()))?;
    title_id_from_nca_header(&nca_header, &header_key)
}

/// Parse the `header_key` entry from a prod.keys file (plain `key = hex` format).
/// The value is 64 hex chars (32 bytes): first 16 = AES cipher key, last 16 = tweak key.
fn parse_header_key(path: &Path) -> Result<[u8; 32]> {
    let content = std::fs::read_to_string(path)?;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == "header_key" {
                let hex = v.trim();
                anyhow::ensure!(
                    hex.len() == 64,
                    "header_key must be 64 hex chars, got {}",
                    hex.len()
                );
                let bytes = decode_hex(hex).context("decoding header_key")?;
                return Ok(bytes.try_into().unwrap());
            }
    }
    anyhow::bail!("header_key not found in {}", path.display())
}

/// PFS0 (NSP) layout:
///   magic "PFS0" | num_files u32 | string_table_size u32 | _reserved u32
///   file entries × num_files:
///     data_offset u64 | data_size u64 | name_offset u32 | _reserved u32
///   string table (null-terminated filenames)
///   data section (file payloads; entry data_offset is relative to this)
///
/// We only read the first 0x400 bytes of the CNMT NCA (two XTS sectors).
fn extract_cnmt_nca_header(nsp_path: &Path) -> Result<Vec<u8>> {
    let mut f = std::fs::File::open(nsp_path)
        .with_context(|| format!("opening {}", nsp_path.display()))?;

    let (entries, string_table, data_base) = read_pfs0_index(&mut f)?;

    for entry in &entries {
        if cstr_at(&string_table, entry.name_offset as usize).ends_with(".cnmt.nca") {
            let read_len = entry.data_size.min(0x400) as usize;
            let mut buf = vec![0u8; read_len];
            f.seek(SeekFrom::Start(data_base + entry.data_offset))?;
            f.read_exact(&mut buf)?;
            return Ok(buf);
        }
    }

    anyhow::bail!("no .cnmt.nca found in NSP (checked {} files)", entries.len())
}

/// Extract the title ID from the first two XTS sectors of an NCA header.
///
/// NCA header layout (raw on-disk, AES-XTS-encrypted):
///   Sector 0 (0x000–0x1FF): RSA-2048 signatures
///   Sector 1 (0x200–0x3FF): fixed header
///     +0x000  magic  "NCA3" / "NCA2" / "NCA0"
///     +0x010  title_id  u64 LE
fn title_id_from_nca_header(nca: &[u8], header_key: &[u8; 32]) -> Result<u64> {
    anyhow::ensure!(nca.len() >= 0x400, "NCA data too short ({} bytes)", nca.len());

    // AES-XTS key split: first 16 bytes = AES cipher key, last 16 = tweak key.
    let key1 = &header_key[..16];
    let key2 = &header_key[16..];

    let mut sector1 = [0u8; 0x200];
    sector1.copy_from_slice(&nca[0x200..0x400]);
    aes_xts_decrypt_sector(&mut sector1, key1, key2, 1)
        .context("AES-XTS decryption of NCA header sector 1")?;

    anyhow::ensure!(
        matches!(&sector1[..4], b"NCA3" | b"NCA2" | b"NCA0"),
        "NCA magic not found after decryption (got {:?}) — wrong header_key or not an NCA",
        std::str::from_utf8(&sector1[..4]).unwrap_or("???")
    );

    Ok(u64::from_le_bytes(sector1[0x10..0x18].try_into().unwrap()))
}

/// Decrypt one 0x200-byte XTS sector in-place.
///
/// XTS per 16-byte block i:
///   T_0 = AES_key2(sector_num as 128-bit LE zero-padded)
///   T_i = T_{i-1} · α  (GF(2^128) multiply by primitive element)
///   plaintext_i = AES_key1_decrypt(ciphertext_i ⊕ T_i) ⊕ T_i
fn aes_xts_decrypt_sector(
    sector: &mut [u8; 0x200],
    key1: &[u8],
    key2: &[u8],
    sector_num: u64,
) -> Result<()> {
    use aes::cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
    use aes::Aes128;

    let enc2 = Aes128::new_from_slice(key2)
        .map_err(|e| anyhow::anyhow!("AES-128 key2 init: {e}"))?;
    let dec1 = Aes128::new_from_slice(key1)
        .map_err(|e| anyhow::anyhow!("AES-128 key1 init: {e}"))?;

    // Nintendo XTS tweak: sector_num as a 128-bit big-endian integer (non-standard —
    // IEEE P1619 uses little-endian; Nintendo puts the u64 in the high 8 bytes).
    let mut tweak = [0u8; 16];
    tweak[8..].copy_from_slice(&sector_num.to_be_bytes());
    enc2.encrypt_block(GenericArray::from_mut_slice(&mut tweak));

    for chunk in sector.as_chunks_mut::<16>().0 {
        for (b, t) in chunk.iter_mut().zip(&tweak) {
            *b ^= t;
        }
        dec1.decrypt_block(GenericArray::from_mut_slice(chunk));
        for (b, t) in chunk.iter_mut().zip(&tweak) {
            *b ^= t;
        }
        gf128_mul_alpha(&mut tweak);
    }

    Ok(())
}

/// Multiply a 128-bit little-endian value by the primitive element α in GF(2^128).
/// XTS polynomial: x^128 + x^7 + x^2 + x + 1 → feedback byte 0x87 on carry.
fn gf128_mul_alpha(t: &mut [u8; 16]) {
    let carry = t[15] >> 7;
    for i in (1..16usize).rev() {
        t[i] = (t[i] << 1) | (t[i - 1] >> 7);
    }
    t[0] = (t[0] << 1) ^ if carry != 0 { 0x87 } else { 0 };
}

fn cstr_at(table: &[u8], offset: usize) -> &str {
    let slice = &table[offset..];
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    std::str::from_utf8(&slice[..end]).unwrap_or("")
}

fn decode_hex(s: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(s.len().is_multiple_of(2), "odd-length hex string");
    s.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = hex_nibble(pair[0])?;
            let lo = hex_nibble(pair[1])?;
            Ok((hi << 4) | lo)
        })
        .collect()
}

fn hex_nibble(b: u8) -> Result<u8> {
    Ok(match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => anyhow::bail!("invalid hex character: {:?}", b as char),
    })
}

fn read_u32_le(r: &mut impl Read) -> Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64_le(r: &mut impl Read) -> Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_title_maps_to_base() {
        assert_eq!(base_title_id(0x0100000000010800), 0x0100000000010000);
        assert_eq!(base_title_id(0x0100000000010000), 0x0100000000010000);
        assert_eq!(base_title_id(0x0100000000011001), 0x0100000000011001);
    }
}
