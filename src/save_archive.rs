// SPDX-License-Identifier: MPL-2.0
//! Zip archives for saves made of several files, in argosy-launcher's layouts so
//! saves round-trip between rommd and argosy:
//!
//! - **Directory saves** (Switch, Wii): every entry under one root folder named
//!   after the title, e.g. `0100000000010000/File1.bin`.
//! - **File bundles** (GameCube `.gci` files): flat entries, no root folder.
//!
//! An archive is described by its entries: (local file, name inside the zip).

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use md5::Digest;

/// Newest mtime of any file under `dir`, recursively. `None` when the directory
/// is missing or holds no files — i.e. there is no local save.
pub fn newest_mtime(dir: &Path) -> Option<SystemTime> {
    let mut newest = None;
    for (path, _) in walk_files(dir).ok()? {
        if let Ok(t) = std::fs::metadata(&path).and_then(|m| m.modified()) {
            newest = Some(newest.map_or(t, |n: SystemTime| n.max(t)));
        }
    }
    newest
}

/// Entries of a directory save: every file under `dir`, named `<root>/<rel>`.
/// Empty when the directory is missing.
pub fn dir_entries(dir: &Path, root: &str) -> Result<Vec<(PathBuf, String)>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    Ok(walk_files(dir)?
        .into_iter()
        .map(|(path, rel)| {
            let name = format!("{}/{}", root, rel.to_string_lossy().replace('\\', "/"));
            (path, name)
        })
        .collect())
}

/// RomM's `content_hash` for an archive with these entries: MD5 over the sorted
/// `"<entry name>:<md5 hex>"` lines joined by `\n`. This is the scheme RomM and
/// argosy-launcher use for zips, so local files can be compared with a remote
/// zip without downloading it. `None` when there are no entries.
pub fn entries_hash(entries: &[(PathBuf, String)]) -> Result<Option<String>> {
    if entries.is_empty() {
        return Ok(None);
    }
    let mut lines = Vec::with_capacity(entries.len());
    for (path, name) in entries {
        lines.push((name.clone(), file_md5(path)?));
    }
    Ok(Some(finalize_hash(lines)))
}

/// Newest mtime among the entries' files.
pub fn entries_mtime(entries: &[(PathBuf, String)]) -> Option<SystemTime> {
    entries
        .iter()
        .filter_map(|(p, _)| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .max()
}

/// RomM's `content_hash` for a zip file (see `entries_hash`).
pub fn zip_content_hash(zip_path: &Path) -> Result<String> {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(zip_path)?)?;
    let mut lines = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let mut ctx = md5::Md5::new();
        std::io::copy(&mut entry, &mut ctx)?;
        lines.push((entry.name().to_owned(), hex(&ctx.finalize())));
    }
    Ok(finalize_hash(lines))
}

/// The MD5 of `data` as RomM writes a `content_hash`.
pub fn bytes_md5(data: &[u8]) -> String {
    hex(&md5::Md5::digest(data))
}

/// Write a zip of these in-memory members to `dest`.
#[cfg(test)]
pub fn zip_members(members: &[(String, Vec<u8>)], dest: &Path) -> Result<()> {
    anyhow::ensure!(!members.is_empty(), "no files to archive");
    let out =
        std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut zip = zip::ZipWriter::new(out);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in members {
        zip.start_file(name.as_str(), opts)?;
        zip.write_all(data)?;
    }
    zip.finish()?;
    Ok(())
}

/// Remove the marker argosy-launcher appends to a save uploaded from a
/// RetroAchievements hardcore session (`SaveArchiver.appendHardcoreTrailer`;
/// Sigil's `sigil_sync_unmarked_len`): JSON holding `"h":true`, its length as
/// a little-endian u32, then `ARGOSY` 01 00. Left in, it would become part of
/// the save. Returns whether there was one.
pub fn strip_hardcore_marker(path: &Path) -> Result<bool> {
    const MAGIC: &[u8] = b"ARGOSY\x01\x00";
    const JSON_MAX: u64 = 1024;
    let tail_len = MAGIC.len() as u64 + 4;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let len = file.metadata()?.len();
    if len < tail_len {
        return Ok(false);
    }
    let read_at = |file: &mut std::fs::File, at: u64, n: u64| -> Result<Vec<u8>> {
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(at))?;
        let mut buf = vec![0u8; n as usize];
        file.read_exact(&mut buf)?;
        Ok(buf)
    };
    let tail = read_at(&mut file, len - tail_len, tail_len)?;
    if &tail[4..] != MAGIC {
        return Ok(false);
    }
    let json_len = u32::from_le_bytes(tail[..4].try_into().unwrap()) as u64;
    if json_len > JSON_MAX || json_len > len - tail_len {
        return Ok(false);
    }
    let json = read_at(&mut file, len - tail_len - json_len, json_len)?;
    if !json.windows(8).any(|w| w == b"\"h\":true") {
        return Ok(false);
    }
    file.set_len(len - tail_len - json_len)?;
    Ok(true)
}

pub fn file_md5(path: &Path) -> Result<String> {
    let mut ctx = md5::Md5::new();
    std::io::copy(&mut std::fs::File::open(path)?, &mut ctx)?;
    Ok(hex(&ctx.finalize()))
}

fn finalize_hash(mut lines: Vec<(String, String)>) -> String {
    lines.sort();
    let joined = lines
        .iter()
        .map(|(name, hash)| format!("{}:{}", name, hash))
        .collect::<Vec<_>>()
        .join("\n");
    hex(&md5::Md5::digest(joined.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Empty directories under `dir`, named `<root>/<rel>/`, so a directory save
/// keeps folders a game created but has not written to yet (a 3DS save archive
/// formatted before the first save). Zip directory entries do not count
/// towards `content_hash`, for RomM or argosy.
pub fn dir_empty_dirs(dir: &Path, root: &str) -> Result<Vec<String>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for rel in walk_dirs(dir)? {
        if std::fs::read_dir(dir.join(&rel))?.next().is_none() {
            out.push(format!(
                "{}/{}/",
                root,
                rel.to_string_lossy().replace('\\', "/")
            ));
        }
    }
    Ok(out)
}

/// Write a zip with these entries to `dest`.
#[cfg(test)]
pub fn zip_entries(entries: &[(PathBuf, String)], dest: &Path) -> Result<()> {
    zip_entries_with_dirs(entries, &[], dest)
}

/// Write a zip with these file entries and empty directory entries.
pub fn zip_entries_with_dirs(
    entries: &[(PathBuf, String)],
    dirs: &[String],
    dest: &Path,
) -> Result<()> {
    anyhow::ensure!(!entries.is_empty(), "no files to archive");
    let out =
        std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut zip = zip::ZipWriter::new(out);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for dir in dirs {
        zip.add_directory(dir.as_str(), opts)?;
    }
    for (path, name) in entries {
        zip.start_file(name.as_str(), opts)?;
        let mut buf = Vec::new();
        std::fs::File::open(path)?.read_to_end(&mut buf)?;
        zip.write_all(&buf)?;
    }
    zip.finish()?;
    Ok(())
}

/// Replace a file bundle in `dest_dir` with the flat zip at `zip_path`.
///
/// Every entry must be a plain file name that `accept` approves once
/// extracted (e.g. a GCI whose header names this game), so another game's save
/// is never restored. `existing` (the bundle's current files) is copied to
/// `backup_dir` and removed before the new files are put in place.
pub fn restore_bundle(
    zip_path: &Path,
    dest_dir: &Path,
    existing: &[PathBuf],
    accept: impl Fn(&Path) -> bool,
    backup_dir: &Path,
) -> Result<()> {
    let staging = tempdir_near(backup_dir)?;
    let result = (|| {
        let mut archive =
            zip::ZipArchive::new(std::fs::File::open(zip_path)?).context("reading save zip")?;
        let mut names = Vec::new();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            if entry.is_dir() {
                continue;
            }
            let name = entry
                .enclosed_name()
                .filter(|n| n.components().count() == 1)
                .and_then(|n| n.to_str().map(str::to_owned))
                .ok_or_else(|| {
                    anyhow::anyhow!("unexpected entry in save bundle: {}", entry.name())
                })?;
            let staged = staging.join(&name);
            std::io::copy(&mut entry, &mut std::fs::File::create(&staged)?)?;
            anyhow::ensure!(
                accept(&staged),
                "{} in the save bundle does not belong to this game",
                name
            );
            names.push(name);
        }
        anyhow::ensure!(!names.is_empty(), "save bundle contains no files");

        if !existing.is_empty() {
            std::fs::create_dir_all(backup_dir)?;
            for path in existing {
                std::fs::copy(path, backup_dir.join(path.file_name().unwrap_or_default()))
                    .with_context(|| format!("backing up {}", path.display()))?;
            }
        }
        for path in existing {
            std::fs::remove_file(path)?;
        }
        std::fs::create_dir_all(dest_dir)?;
        for name in names {
            std::fs::copy(staging.join(&name), dest_dir.join(&name))?;
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// One root folder of a directory-save zip and where it is restored.
#[derive(Debug, Clone)]
pub struct ZipRoot {
    /// The root as rommd writes it (`0100000000010000`, `data`); several
    /// segments for a folder deeper in the zip (`524d3845/data`).
    pub name: String,
    /// The directories that receive it: the first is the real save, the rest
    /// mirrors (Ryujinx's working copy).
    pub targets: Vec<PathBuf>,
    /// Other prefixes read as this root, preferred over `name` when a zip has
    /// both: how argosy-launcher's Sigil units spell it
    /// (`00040000/00033500/data`, `device/<title>`).
    pub alternatives: Vec<String>,
}

/// Restore a directory-save zip. Prefixes compare case-insensitively.
///
/// An entry under no root and no `ignored` prefix fails the restore before
/// anything is touched — it guards against restoring another game's save if
/// RomM has a file attached to the wrong ROM. Each root the zip contains
/// replaces its directories, after the first is copied to
/// `backup_dir/<root>`; a root the zip lacks leaves its directories alone.
/// Entries under `ignored` (parts of a unit this save doesn't hold) are
/// skipped.
pub fn restore_roots(
    zip_path: &Path,
    roots: &[ZipRoot],
    ignored: &[String],
    backup_dir: &Path,
) -> Result<()> {
    let staging = tempdir_near(backup_dir)?;
    let result = (|| {
        let present = extract_rooted(zip_path, roots, ignored, &staging)?;
        for (i, root) in roots.iter().enumerate() {
            let variants = root.alternatives.len() + 1;
            let Some(variant) = (0..variants).find(|v| present.contains(&(i, *v))) else {
                continue;
            };
            let staged = staging.join(format!("{i}-{variant}"));
            if let Some(primary) = root.targets.first()
                && newest_mtime(primary).is_some()
            {
                copy_dir(primary, &backup_dir.join(&root.name))
                    .with_context(|| format!("backing up {}", primary.display()))?;
            }
            for target in &root.targets {
                replace_dir_contents(&staged, target)
                    .with_context(|| format!("restoring into {}", target.display()))?;
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// A prefix's path segments, lowercased.
fn segments(prefix: &str) -> Vec<String> {
    prefix
        .split(['/', '\\'])
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Extract `zip_path` into `dest/<root index>-<variant>/…`, where variant 0
/// to n-1 are a root's alternatives and n its own name. Every entry must sit
/// under a root or an ignored prefix; the longest prefix wins. Returns the
/// (root, variant) pairs the zip contains.
fn extract_rooted(
    zip_path: &Path,
    roots: &[ZipRoot],
    ignored: &[String],
    dest: &Path,
) -> Result<Vec<(usize, usize)>> {
    // (segments, the root and variant they stand for; `None` when ignored)
    type Prefix = (Vec<String>, Option<(usize, usize)>);
    let mut prefixes: Vec<Prefix> = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        for (v, alt) in root.alternatives.iter().enumerate() {
            prefixes.push((segments(alt), Some((i, v))));
        }
        prefixes.push((segments(&root.name), Some((i, root.alternatives.len()))));
    }
    prefixes.extend(ignored.iter().map(|p| (segments(p), None)));
    prefixes.sort_by_key(|(segs, _)| std::cmp::Reverse(segs.len()));

    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).context("reading save zip")?;
    let mut present: Vec<(usize, usize)> = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(name) = entry.enclosed_name() else {
            anyhow::bail!("unsafe path in save zip: {}", entry.name());
        };
        let parts: Vec<String> = name
            .components()
            .map(|c| match c {
                Component::Normal(p) => Ok(p.to_string_lossy().into_owned()),
                _ => anyhow::bail!("unexpected entry in save zip: {}", entry.name()),
            })
            .collect::<Result<_>>()?;
        let Some((len, target)) = prefixes.iter().find_map(|(segs, target)| {
            let matches = parts.len() >= segs.len()
                && segs.iter().zip(&parts).all(|(s, p)| *s == p.to_lowercase());
            matches.then_some((segs.len(), *target))
        }) else {
            anyhow::bail!(
                "save zip holds {} but this game's save is {}",
                parts.first().map(String::as_str).unwrap_or_default(),
                roots
                    .iter()
                    .map(|r| r.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" + ")
            );
        };
        let Some((root, variant)) = target else {
            continue;
        };
        if !present.contains(&(root, variant)) {
            present.push((root, variant));
        }
        let rel: PathBuf = parts[len..].iter().collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(format!("{root}-{variant}")).join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::File::create(&out)?;
        std::io::copy(&mut entry, &mut f)?;
    }
    anyhow::ensure!(newest_mtime(dest).is_some(), "save zip contains no files");
    Ok(present)
}

fn replace_dir_contents(src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        for entry in std::fs::read_dir(dest)? {
            let path = entry?.path();
            if path.is_dir() {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
        }
    }
    copy_dir(src, dest)
}

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for rel in walk_dirs(src)? {
        std::fs::create_dir_all(dest.join(rel))?;
    }
    for (path, rel) in walk_files(src)? {
        let out = dest.join(rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&path, &out)?;
    }
    Ok(())
}

/// All directories under `dir` (not `dir` itself), relative to it, sorted.
fn walk_dirs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let path = entry?.path();
            if path.is_dir() {
                out.push(path.strip_prefix(dir).unwrap().to_path_buf());
                stack.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// All files under `dir` as (absolute path, path relative to `dir`), sorted.
fn walk_files(dir: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path.strip_prefix(dir).unwrap().to_path_buf();
                out.push((path, rel));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn tempdir_near(path: &Path) -> Result<PathBuf> {
    unique_dir(path.parent().unwrap_or(path), ".staging-")
}

/// Create a new directory in `parent` that no other caller in this process
/// gets: the clock alone can repeat between threads (seen on Windows), and two
/// syncs sharing a directory would write into each other's zips.
pub fn unique_dir(parent: &Path, prefix: &str) -> Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = parent.join(format!(
        "{}{}-{}-{}",
        prefix,
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_nanos(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rommd-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn roundtrip_with_subdirs_and_backup() {
        let base = scratch("roundtrip");
        let src = base.join("src");
        std::fs::create_dir_all(src.join("slot_00")).unwrap();
        std::fs::write(src.join("Common.bin"), b"common").unwrap();
        std::fs::write(src.join("slot_00/progress.sav"), b"progress").unwrap();

        let zip = base.join("save.zip");
        zip_entries(&dir_entries(&src, "0100000000010000").unwrap(), &zip).unwrap();

        let dest = base.join("dest");
        let mirror = base.join("mirror");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("stale.bin"), b"old").unwrap();

        let backup = base.join("backups/1");
        restore_roots(
            &zip,
            &[root("0100000000010000", vec![dest.clone(), mirror.clone()])],
            &[],
            &backup,
        )
        .unwrap();

        for d in [&dest, &mirror] {
            assert_eq!(std::fs::read(d.join("Common.bin")).unwrap(), b"common");
            assert_eq!(
                std::fs::read(d.join("slot_00/progress.sav")).unwrap(),
                b"progress"
            );
            assert!(!d.join("stale.bin").exists());
        }
        assert_eq!(
            std::fs::read(backup.join("0100000000010000/stale.bin")).unwrap(),
            b"old"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn content_hash_matches_romm() {
        // Directory and zip hashing must agree, and follow the RomM scheme.
        let base = scratch("hash");
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.bin"), b"a").unwrap();
        std::fs::write(src.join("b.bin"), b"b").unwrap();
        let expected = {
            let lines = format!(
                "ROOT/a.bin:{}\nROOT/b.bin:{}",
                hex(&md5::Md5::digest(b"a")),
                hex(&md5::Md5::digest(b"b"))
            );
            hex(&md5::Md5::digest(lines.as_bytes()))
        };
        let entries = dir_entries(&src, "ROOT").unwrap();
        assert_eq!(
            entries_hash(&entries).unwrap().as_deref(),
            Some(expected.as_str())
        );
        let zip = base.join("s.zip");
        zip_entries(&entries, &zip).unwrap();
        assert_eq!(zip_content_hash(&zip).unwrap(), expected);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rejects_other_title() {
        let base = scratch("reject");
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.bin"), b"a").unwrap();
        let zip = base.join("save.zip");
        zip_entries(&dir_entries(&src, "0100AAAA00000000").unwrap(), &zip).unwrap();

        let dest = base.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("keep.bin"), b"keep").unwrap();
        assert!(
            restore_roots(
                &zip,
                &[root("0100000000010000", vec![dest.clone()])],
                &[],
                &base.join("bk")
            )
            .is_err()
        );
        assert_eq!(std::fs::read(dest.join("keep.bin")).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn bundle_restore_replaces_only_accepted_files() {
        let base = scratch("bundle");
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("01-GALE-a.gci"), b"GALE01 new").unwrap();
        let entries = vec![(src.join("01-GALE-a.gci"), "01-GALE-a.gci".to_owned())];
        let zip = base.join("b.zip");
        zip_entries(&entries, &zip).unwrap();

        let card = base.join("Card A");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(card.join("01-GALE-old.gci"), b"GALE01 old").unwrap();
        std::fs::write(card.join("01-GMSE-other.gci"), b"GMSE01 keep").unwrap();
        let is_melee = |p: &Path| std::fs::read(p).unwrap().starts_with(b"GALE01");

        let backup = base.join("bk/1");
        restore_bundle(
            &zip,
            &card,
            &[card.join("01-GALE-old.gci")],
            is_melee,
            &backup,
        )
        .unwrap();
        assert!(!card.join("01-GALE-old.gci").exists());
        assert_eq!(
            std::fs::read(card.join("01-GALE-a.gci")).unwrap(),
            b"GALE01 new"
        );
        assert_eq!(
            std::fs::read(card.join("01-GMSE-other.gci")).unwrap(),
            b"GMSE01 keep"
        );
        assert_eq!(
            std::fs::read(backup.join("01-GALE-old.gci")).unwrap(),
            b"GALE01 old"
        );

        let reject = |_: &Path| false;
        assert!(restore_bundle(&zip, &card, &[], reject, &base.join("bk/2")).is_err());
        assert_eq!(
            std::fs::read(card.join("01-GALE-a.gci")).unwrap(),
            b"GALE01 new"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn multi_root_restore_replaces_only_the_roots_it_carries() {
        let base = scratch("roots");
        let src = base.join("src");
        std::fs::create_dir_all(src.join("extdata")).unwrap();
        std::fs::write(src.join("extdata/ext.bin"), b"new ext").unwrap();
        let zip = base.join("s.zip");
        zip_entries(&dir_entries(&src.join("extdata"), "extdata").unwrap(), &zip).unwrap();

        let (data, ext) = (base.join("data"), base.join("ext"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("save.bin"), b"keep").unwrap();
        let roots = [
            root("data", vec![data.clone()]),
            root("extdata", vec![ext.clone()]),
        ];
        restore_roots(&zip, &roots, &[], &base.join("bk")).unwrap();
        assert_eq!(
            std::fs::read(data.join("save.bin")).unwrap(),
            b"keep",
            "a root the zip lacks stays"
        );
        assert_eq!(std::fs::read(ext.join("ext.bin")).unwrap(), b"new ext");

        let only_data = [root("data", vec![data.clone()])];
        assert!(
            restore_roots(&zip, &only_data, &[], &base.join("bk2")).is_err(),
            "unexpected root"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    fn root(name: &str, targets: Vec<PathBuf>) -> ZipRoot {
        ZipRoot {
            name: name.into(),
            targets,
            alternatives: Vec::new(),
        }
    }

    #[test]
    fn reads_sigil_prefixes_and_skips_ignored_areas() {
        let base = scratch("sigil-roots");
        let zip = base.join("s.zip");
        let file = std::fs::File::create(&zip).unwrap();
        let mut w = zip::ZipWriter::new(file);
        for (name, data) in [
            ("0100000000010000/account.bin", &b"account"[..]),
            ("device/0100000000010000/device.bin", b"device"),
            ("Device/0100000000010000/sub/more.bin", b"more"),
        ] {
            w.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();

        // A device save takes Sigil's device area over the account one.
        let device = base.join("device");
        let mut r = root("0100000000010000", vec![device.clone()]);
        r.alternatives = vec!["device/0100000000010000".into()];
        restore_roots(&zip, &[r], &[], &base.join("bk")).unwrap();
        assert_eq!(std::fs::read(device.join("device.bin")).unwrap(), b"device");
        assert_eq!(std::fs::read(device.join("sub/more.bin")).unwrap(), b"more");
        assert!(!device.join("account.bin").exists());

        // An account save skips the device area, and refuses it unless told to.
        let account = base.join("account");
        let r = root("0100000000010000", vec![account.clone()]);
        assert!(restore_roots(&zip, std::slice::from_ref(&r), &[], &base.join("bk2")).is_err());
        restore_roots(&zip, &[r], &["device".into()], &base.join("bk3")).unwrap();
        assert_eq!(
            std::fs::read(account.join("account.bin")).unwrap(),
            b"account"
        );
        assert!(!account.join("device.bin").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn strips_argosys_hardcore_marker_only() {
        let base = scratch("hardcore");
        let path = base.join("save.gci");
        let marker = |json: &[u8]| {
            let mut m = json.to_vec();
            m.extend_from_slice(&(json.len() as u32).to_le_bytes());
            m.extend_from_slice(b"ARGOSY\x01\x00");
            m
        };
        let mut data = b"GALE01 the save".to_vec();
        data.extend(marker(br#"{"h":true,"v":1}"#));
        std::fs::write(&path, &data).unwrap();
        assert!(strip_hardcore_marker(&path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"GALE01 the save");
        assert!(
            !strip_hardcore_marker(&path).unwrap(),
            "nothing left to strip"
        );

        let mut softcore = b"GALE01 the save".to_vec();
        softcore.extend(marker(br#"{"h":false,"v":1}"#));
        std::fs::write(&path, &softcore).unwrap();
        assert!(!strip_hardcore_marker(&path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), softcore);

        std::fs::write(&path, b"short").unwrap();
        assert!(!strip_hardcore_marker(&path).unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }
}
