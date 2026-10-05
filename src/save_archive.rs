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
            out.push(format!("{}/{}/", root, rel.to_string_lossy().replace('\\', "/")));
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
pub fn zip_entries_with_dirs(entries: &[(PathBuf, String)], dirs: &[String], dest: &Path) -> Result<()> {
    anyhow::ensure!(!entries.is_empty(), "no files to archive");
    let out = std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
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
        let mut archive = zip::ZipArchive::new(std::fs::File::open(zip_path)?).context("reading save zip")?;
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
                .ok_or_else(|| anyhow::anyhow!("unexpected entry in save bundle: {}", entry.name()))?;
            let staged = staging.join(&name);
            std::io::copy(&mut entry, &mut std::fs::File::create(&staged)?)?;
            anyhow::ensure!(accept(&staged), "{} in the save bundle does not belong to this game", name);
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

/// Restore a directory-save zip. `roots` maps each root folder the zip may
/// contain (compared case-insensitively) to the directories that receive it:
/// the first is the real save, the rest mirrors (Ryujinx's working copy).
///
/// An entry under any other root fails the restore before anything is touched
/// — it guards against restoring another game's save if RomM has a file
/// attached to the wrong ROM. Each root the zip contains replaces its
/// directories, after the first is copied to `backup_dir/<root>`; a root the
/// zip lacks leaves its directories alone.
pub fn restore_roots(zip_path: &Path, roots: &[(String, Vec<PathBuf>)], backup_dir: &Path) -> Result<()> {
    let staging = tempdir_near(backup_dir)?;
    let result = (|| {
        let expected: Vec<&str> = roots.iter().map(|(r, _)| r.as_str()).collect();
        let present = extract_rooted(zip_path, &expected, &staging)?;
        for (root, targets) in roots {
            let Some(staged) = present.iter().find(|p| p.eq_ignore_ascii_case(root)) else { continue };
            let staged = staging.join(staged);
            if let Some(primary) = targets.first()
                && newest_mtime(primary).is_some() {
                    copy_dir(primary, &backup_dir.join(root))
                        .with_context(|| format!("backing up {}", primary.display()))?;
                }
            for target in targets {
                replace_dir_contents(&staged, target)
                    .with_context(|| format!("restoring into {}", target.display()))?;
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// Extract `zip_path` into `dest/<root>/…`; every entry must sit under one of
/// `expected` roots. Returns the roots the zip contains.
fn extract_rooted(zip_path: &Path, expected: &[&str], dest: &Path) -> Result<Vec<String>> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).context("reading save zip")?;
    let mut present: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(name) = entry.enclosed_name() else {
            anyhow::bail!("unsafe path in save zip: {}", entry.name());
        };
        let mut comps = name.components();
        let root = match comps.next() {
            Some(Component::Normal(r)) => r.to_string_lossy().into_owned(),
            _ => anyhow::bail!("unexpected entry in save zip: {}", entry.name()),
        };
        anyhow::ensure!(
            expected.iter().any(|e| e.eq_ignore_ascii_case(&root)),
            "save zip holds {} but this game's save is {}",
            root,
            expected.join(" + ")
        );
        if !present.contains(&root) {
            present.push(root.clone());
        }
        let rel: PathBuf = comps.collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&root).join(&rel);
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
    let parent = path.parent().unwrap_or(path);
    let dir = parent.join(format!(
        ".staging-{}",
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_nanos()
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
        restore_roots(&zip, &[("0100000000010000".into(), vec![dest.clone(), mirror.clone()])], &backup).unwrap();

        for d in [&dest, &mirror] {
            assert_eq!(std::fs::read(d.join("Common.bin")).unwrap(), b"common");
            assert_eq!(std::fs::read(d.join("slot_00/progress.sav")).unwrap(), b"progress");
            assert!(!d.join("stale.bin").exists());
        }
        assert_eq!(std::fs::read(backup.join("0100000000010000/stale.bin")).unwrap(), b"old");
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
        assert_eq!(entries_hash(&entries).unwrap().as_deref(), Some(expected.as_str()));
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
        assert!(restore_roots(&zip, &[("0100000000010000".into(), vec![dest.clone()])], &base.join("bk")).is_err());
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
        restore_bundle(&zip, &card, &[card.join("01-GALE-old.gci")], is_melee, &backup).unwrap();
        assert!(!card.join("01-GALE-old.gci").exists());
        assert_eq!(std::fs::read(card.join("01-GALE-a.gci")).unwrap(), b"GALE01 new");
        assert_eq!(std::fs::read(card.join("01-GMSE-other.gci")).unwrap(), b"GMSE01 keep");
        assert_eq!(std::fs::read(backup.join("01-GALE-old.gci")).unwrap(), b"GALE01 old");

        let reject = |_: &Path| false;
        assert!(restore_bundle(&zip, &card, &[], reject, &base.join("bk/2")).is_err());
        assert_eq!(std::fs::read(card.join("01-GALE-a.gci")).unwrap(), b"GALE01 new");
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
        let roots = [("data".to_owned(), vec![data.clone()]), ("extdata".to_owned(), vec![ext.clone()])];
        restore_roots(&zip, &roots, &base.join("bk")).unwrap();
        assert_eq!(std::fs::read(data.join("save.bin")).unwrap(), b"keep", "a root the zip lacks stays");
        assert_eq!(std::fs::read(ext.join("ext.bin")).unwrap(), b"new ext");

        let only_data = [("data".to_owned(), vec![data.clone()])];
        assert!(restore_roots(&zip, &only_data, &base.join("bk2")).is_err(), "unexpected root");
        let _ = std::fs::remove_dir_all(&base);
    }
}
