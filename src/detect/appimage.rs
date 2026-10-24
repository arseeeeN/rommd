//! What an AppImage says about itself, read from the file without running
//! it: the names at the root of its embedded filesystem, which by the AppImage
//! spec include the app's desktop file and icon (named after its app ID, e.g.
//! `org.duckstation.DuckStation.desktop`), and the update information the
//! runtime carries in its `.upd_info` ELF section.
//!
//! An AppImage is an ELF runtime followed by a filesystem image. Only the
//! squashfs root directory is read (gzip or zstd, which is what AppImage
//! tooling produces); DwarFS images (uruntime) pack their names and are known
//! by update information alone.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Default, PartialEq)]
pub struct AppImage {
    /// Entry names in the root of the image; empty when it can't be read.
    pub root: Vec<String>,
    /// The `.upd_info` section, e.g. `zsync|https://…/Eden-Linux-amd64.AppImage.zsync`.
    pub update_info: String,
}

impl AppImage {
    /// Whether the image declares this app ID with a root desktop file or
    /// icon.
    pub fn declares(&self, app_id: &str) -> bool {
        self.root.iter().any(|name| {
            name.rsplit_once('.').is_some_and(|(stem, ext)| {
                stem == app_id && matches!(ext, "desktop" | "png" | "svg" | "xpm")
            })
        })
    }
}

/// Whether `path` starts like a type 2 AppImage (`AI\x02` in the ELF
/// header's padding).
pub fn is_appimage(path: &Path) -> bool {
    let mut magic = [0u8; 11];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok_and(|_| magic[..4] == *b"\x7fELF" && magic[8..] == *b"AI\x02")
}

pub fn read(path: &Path) -> Option<AppImage> {
    if !is_appimage(path) {
        return None;
    }
    let file = File::open(path).ok()?;
    let elf = Elf::read(&file)?;
    let update_info = elf.update_info(&file).unwrap_or_default();
    let root = squashfs_root(&file, elf.end).unwrap_or_default();
    Some(AppImage { root, update_info })
}

/// The parts of a 64-bit little-endian ELF header used here.
struct Elf {
    /// Where the section header table ends: the start of the filesystem.
    end: u64,
    sections: u64,
    section_count: u16,
    names_index: u16,
}

impl Elf {
    fn read(mut file: &File) -> Option<Self> {
        let mut header = [0u8; 64];
        file.read_exact(&mut header).ok()?;
        // ELFCLASS64, little endian.
        if header[4] != 2 || header[5] != 1 {
            return None;
        }
        let sections = u64_at(&header, 0x28);
        let entry_size = u16_at(&header, 0x3A);
        let section_count = u16_at(&header, 0x3C);
        if entry_size != 64 {
            return None;
        }
        Some(Elf {
            end: sections.checked_add(64 * section_count as u64)?,
            sections,
            section_count,
            names_index: u16_at(&header, 0x3E),
        })
    }

    fn update_info(&self, mut file: &File) -> Option<String> {
        if self.section_count > 256 || self.names_index >= self.section_count {
            return None;
        }
        let mut table = vec![0u8; 64 * self.section_count as usize];
        file.seek(SeekFrom::Start(self.sections)).ok()?;
        file.read_exact(&mut table).ok()?;
        let section = |i: usize| &table[i * 64..(i + 1) * 64];
        let names = section(self.names_index as usize);
        let names = read_at(
            file,
            u64_at(names, 0x18),
            u64_at(names, 0x20).min(64 * 1024),
        )?;
        let upd_info = (0..self.section_count as usize).map(section).find(|s| {
            names
                .get(u32_at(s, 0) as usize..)
                .is_some_and(|n| n.starts_with(b".upd_info\0"))
        })?;
        let data = read_at(
            file,
            u64_at(upd_info, 0x18),
            u64_at(upd_info, 0x20).min(4096),
        )?;
        let text = data.split(|&b| b == 0).next()?;
        Some(String::from_utf8_lossy(text).trim().to_owned())
    }
}

const GZIP: u16 = 1;
const ZSTD: u16 = 6;
const METADATA_BLOCK: usize = 8192;

/// Entry names in the root directory of the squashfs image at `base`.
fn squashfs_root(file: &File, base: u64) -> Option<Vec<String>> {
    let sb = read_at(file, base, 96)?;
    if sb.len() < 96 || sb[..4] != *b"hsqs" || u16_at(&sb, 28) != 4 {
        return None;
    }
    let image = Squashfs {
        file,
        base,
        compression: u16_at(&sb, 20),
    };
    let root = u64_at(&sb, 32);
    let (inodes, directories) = (u64_at(&sb, 64), u64_at(&sb, 72));

    let inode = image.metadata(inodes, root >> 16, (root & 0xFFFF) as usize, 40)?;
    let (block, offset, size) = match u16_at(&inode, 0) {
        // Basic directory: block u32, links u32, size u16, offset u16.
        1 => (
            u32_at(&inode, 16),
            u16_at(&inode, 26),
            u16_at(&inode, 24) as u32,
        ),
        // Extended directory: links u32, size u32, block u32, parent u32,
        // index count u16, offset u16.
        8 => (u32_at(&inode, 24), u16_at(&inode, 34), u32_at(&inode, 20)),
        _ => return None,
    };
    // The listing size counts 3 bytes for `.` and `..`, which aren't stored.
    let size = (size as usize).checked_sub(3)?;
    if size > 256 * 1024 {
        return None;
    }
    let listing = image.metadata(directories, block as u64, offset as usize, size)?;

    let mut names = Vec::new();
    let mut at = 0;
    while at + 12 <= listing.len() {
        let count = u32_at(&listing, at) as usize + 1;
        at += 12;
        for _ in 0..count.min(256) {
            // offset u16, inode delta i16, type u16, name length - 1 u16.
            let len = u16_at(listing.get(at..at + 8)?, 6) as usize + 1;
            let name = listing.get(at + 8..at + 8 + len)?;
            names.push(String::from_utf8_lossy(name).into_owned());
            at += 8 + len;
        }
    }
    Some(names)
}

struct Squashfs<'a> {
    file: &'a File,
    base: u64,
    compression: u16,
}

impl Squashfs<'_> {
    /// `len` bytes of a metadata table starting `skip` bytes into the block at
    /// `table + block`, following on into the next blocks.
    fn metadata(&self, table: u64, block: u64, skip: usize, len: usize) -> Option<Vec<u8>> {
        let mut file = self.file;
        let mut position = self.base.checked_add(table)?.checked_add(block)?;
        let mut out = Vec::new();
        while out.len() < skip + len {
            file.seek(SeekFrom::Start(position)).ok()?;
            let mut header = [0u8; 2];
            file.read_exact(&mut header).ok()?;
            let header = u16::from_le_bytes(header);
            let size = (header & 0x7FFF) as usize;
            if size == 0 {
                return None;
            }
            let mut raw = vec![0u8; size];
            file.read_exact(&mut raw).ok()?;
            if header & 0x8000 != 0 {
                out.extend_from_slice(&raw);
            } else {
                out.extend(self.decompress(&raw)?);
            }
            position += 2 + size as u64;
        }
        out.get(skip..skip + len).map(<[u8]>::to_vec)
    }

    fn decompress(&self, raw: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let limit = METADATA_BLOCK as u64 + 1;
        match self.compression {
            GZIP => flate2::read::ZlibDecoder::new(raw)
                .take(limit)
                .read_to_end(&mut out)
                .ok()?,
            ZSTD => ruzstd::decoding::StreamingDecoder::new(raw)
                .ok()?
                .take(limit)
                .read_to_end(&mut out)
                .ok()?,
            _ => return None,
        };
        (out.len() <= METADATA_BLOCK).then_some(out)
    }
}

fn read_at(mut file: &File, offset: u64, len: u64) -> Option<Vec<u8>> {
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut out = Vec::new();
    file.take(len).read_to_end(&mut out).ok()?;
    Some(out)
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::testing::{TempDir, write};

    /// A minimal AppImage: an ELF header, `.upd_info`, and a squashfs image
    /// whose root holds `root` (metadata gzip-compressed with `gzip`).
    pub fn fake(root: &[&str], update_info: &str, gzip: bool) -> Vec<u8> {
        let mut squashfs = vec![0u8; 96];
        squashfs[..4].copy_from_slice(b"hsqs");
        squashfs[20..22].copy_from_slice(&GZIP.to_le_bytes());
        squashfs[28..30].copy_from_slice(&4u16.to_le_bytes());

        let mut listing = Vec::new();
        listing.extend((root.len() as u32 - 1).to_le_bytes());
        listing.extend([0u8; 8]);
        for name in root {
            listing.extend([0u8; 6]);
            listing.extend((name.len() as u16 - 1).to_le_bytes());
            listing.extend(name.as_bytes());
        }
        let mut inode = vec![0u8; 32];
        inode[..2].copy_from_slice(&1u16.to_le_bytes());
        inode[24..26].copy_from_slice(&(listing.len() as u16 + 3).to_le_bytes());

        let inodes = squashfs.len() as u64;
        squashfs.extend(metadata_block(&inode, gzip));
        let directories = squashfs.len() as u64;
        squashfs.extend(metadata_block(&listing, gzip));
        squashfs[64..72].copy_from_slice(&inodes.to_le_bytes());
        squashfs[72..80].copy_from_slice(&directories.to_le_bytes());
        elf(update_info, &squashfs)
    }

    /// An AppImage whose filesystem isn't squashfs (uruntime's DwarFS).
    pub fn fake_dwarfs(update_info: &str) -> Vec<u8> {
        elf(update_info, b"DWARFS\x02\x05 packed names")
    }

    fn metadata_block(data: &[u8], gzip: bool) -> Vec<u8> {
        let (header, body) = if gzip {
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            std::io::Write::write_all(&mut encoder, data).unwrap();
            let body = encoder.finish().unwrap();
            (body.len() as u16, body)
        } else {
            (data.len() as u16 | 0x8000, data.to_vec())
        };
        let mut block = header.to_le_bytes().to_vec();
        block.extend(body);
        block
    }

    fn elf(update_info: &str, filesystem: &[u8]) -> Vec<u8> {
        let names = b"\0.shstrtab\0.upd_info\0";
        let mut upd = update_info.as_bytes().to_vec();
        upd.resize(1024, 0);
        let mut out = vec![0u8; 64];
        out[..4].copy_from_slice(b"\x7fELF");
        out[4..7].copy_from_slice(&[2, 1, 1]);
        out[8..11].copy_from_slice(b"AI\x02");
        let names_at = out.len() as u64;
        out.extend(names);
        let upd_at = out.len() as u64;
        out.extend(&upd);
        let sections = out.len() as u64;
        out[0x28..0x30].copy_from_slice(&sections.to_le_bytes());
        out[0x3A..0x3C].copy_from_slice(&64u16.to_le_bytes());
        out[0x3C..0x3E].copy_from_slice(&3u16.to_le_bytes());
        out[0x3E..0x40].copy_from_slice(&1u16.to_le_bytes());
        for (name, at, len) in [
            (0u32, 0, 0),
            (1, names_at, names.len() as u64),
            (11, upd_at, 1024),
        ] {
            let mut header = [0u8; 64];
            header[..4].copy_from_slice(&name.to_le_bytes());
            header[0x18..0x20].copy_from_slice(&at.to_le_bytes());
            header[0x20..0x28].copy_from_slice(&len.to_le_bytes());
            out.extend(header);
        }
        out.extend(filesystem);
        out
    }

    #[test]
    fn reads_the_root_names_and_update_information() {
        let dir = TempDir::new("appimage-read");
        let root = [
            ".DirIcon",
            "AppRun",
            "org.duckstation.DuckStation.desktop",
            "usr",
        ];
        for gzip in [false, true] {
            let path = dir.path().join(format!("DuckStation-{gzip}.AppImage"));
            write(
                &path,
                &fake(&root, "gh-releases-zsync|stenzek|duckstation", gzip),
            );
            let image = read(&path).unwrap();
            assert_eq!(image.root, root, "gzip: {gzip}");
            assert_eq!(image.update_info, "gh-releases-zsync|stenzek|duckstation");
            assert!(image.declares("org.duckstation.DuckStation"));
            assert!(!image.declares("org.duckstation"), "whole IDs only");
        }

        let eden = dir.path().join("Eden.AppImage");
        write(
            &eden,
            &fake_dwarfs("zsync|https://nightly.eden-emu.dev/latest/x.zsync"),
        );
        let image = read(&eden).unwrap();
        assert!(image.root.is_empty());
        assert!(image.update_info.contains("eden-emu.dev/"));
    }

    #[test]
    fn declares_by_desktop_file_or_icon() {
        let image = AppImage {
            root: vec!["azahar.desktop".into(), "org.azahar_emu.Azahar.svg".into()],
            update_info: String::new(),
        };
        assert!(image.declares("org.azahar_emu.Azahar"));
        let other = AppImage {
            root: vec!["org.azahar_emu.Azahar.txt".into()],
            update_info: String::new(),
        };
        assert!(!other.declares("org.azahar_emu.Azahar"));
    }

    #[test]
    fn other_files_and_broken_images_are_no_appimages() {
        let dir = TempDir::new("appimage-refuse");
        let elf_only = dir.path().join("program");
        write(&elf_only, b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0");
        assert_eq!(read(&elf_only), None);
        let short = dir.path().join("short.AppImage");
        write(&short, b"\x7fELF");
        assert_eq!(read(&short), None);

        // Every truncation of a valid image reads as something or nothing,
        // without panicking.
        let full = fake(&["org.duckstation.DuckStation.desktop"], "", true);
        let cut = dir.path().join("cut.AppImage");
        for len in (0..full.len()).step_by(7) {
            write(&cut, &full[..len]);
            let _ = read(&cut);
        }
    }
}
