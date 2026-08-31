use anyhow::{Context, Result};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Extract the Switch title ID from an NSP file.
///
/// Strategy:
/// 1. Parse the PFS0 container to find the `.cnmt.nca` (content meta NCA).
/// 2. Decrypt NCA header sector 1 with `header_key` from prod.keys (AES-XTS).
/// 3. Read the title ID (u64 LE) at decrypted offset 0x10.
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
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "header_key" {
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

    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)?;
    anyhow::ensure!(&magic == b"PFS0", "not a PFS0/NSP file (magic {:?})", magic);

    let num_files = read_u32_le(&mut f)?;
    let string_table_size = read_u32_le(&mut f)?;
    let _reserved = read_u32_le(&mut f)?;

    struct Entry {
        data_offset: u64,
        data_size: u64,
        name_offset: u32,
    }
    let mut entries = Vec::with_capacity(num_files as usize);
    for _ in 0..num_files {
        let data_offset = read_u64_le(&mut f)?;
        let data_size = read_u64_le(&mut f)?;
        let name_offset = read_u32_le(&mut f)?;
        let _reserved = read_u32_le(&mut f)?;
        entries.push(Entry { data_offset, data_size, name_offset });
    }

    let mut string_table = vec![0u8; string_table_size as usize];
    f.read_exact(&mut string_table)?;

    let data_base = f.stream_position()?;

    for entry in &entries {
        if cstr_at(&string_table, entry.name_offset as usize).ends_with(".cnmt.nca") {
            let read_len = entry.data_size.min(0x400) as usize;
            let mut buf = vec![0u8; read_len];
            f.seek(SeekFrom::Start(data_base + entry.data_offset))?;
            f.read_exact(&mut buf)?;
            return Ok(buf);
        }
    }

    anyhow::bail!("no .cnmt.nca found in NSP (checked {} files)", num_files)
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

    // Initial tweak = AES_key2(sector_num as 128-bit little-endian, zero-padded).
    let mut tweak = [0u8; 16];
    tweak[..8].copy_from_slice(&sector_num.to_le_bytes());
    enc2.encrypt_block(GenericArray::from_mut_slice(&mut tweak));

    for chunk in sector.chunks_exact_mut(16) {
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
    anyhow::ensure!(s.len() % 2 == 0, "odd-length hex string");
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
