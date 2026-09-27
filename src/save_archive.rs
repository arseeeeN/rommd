//! Whole-directory save archives, used for emulators whose per-game save is a
//! folder rather than a single file (Switch). The zip layout matches argosy-launcher:
//! every entry lives under a single root folder named after the title ID, e.g.
//! `0100000000010000/File1.bin`, so saves round-trip between rommd and argosy.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

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

/// Zip the contents of `dir` into `dest`, placing every entry under `root/`.
pub fn zip_dir(dir: &Path, root: &str, dest: &Path) -> Result<()> {
    let files = walk_files(dir)?;
    anyhow::ensure!(!files.is_empty(), "no files to archive in {}", dir.display());

    let out = std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut zip = zip::ZipWriter::new(out);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (path, rel) in files {
        let name = format!("{}/{}", root, rel.to_string_lossy().replace('\\', "/"));
        zip.start_file(name, opts)?;
        let mut buf = Vec::new();
        std::fs::File::open(&path)?.read_to_end(&mut buf)?;
        zip.write_all(&buf)?;
    }
    zip.finish()?;
    Ok(())
}

/// Replace the contents of `targets` with the save in `zip_path`.
///
/// The archive must contain a single root folder whose name equals
/// `expected_root` (case-insensitive) — this guards against restoring another
/// game's save if RomM has a file attached to the wrong ROM. The current local
/// save of the first target is copied to `backup_dir` before anything is touched.
pub fn restore_zip(
    zip_path: &Path,
    expected_root: &str,
    targets: &[PathBuf],
    backup_dir: &Path,
) -> Result<()> {
    let staging = tempdir_near(backup_dir)?;
    let result = (|| {
        extract_rooted(zip_path, expected_root, &staging)?;
        if let Some(primary) = targets.first() {
            if newest_mtime(primary).is_some() {
                copy_dir(primary, backup_dir)
                    .with_context(|| format!("backing up {}", primary.display()))?;
            }
        }
        for target in targets {
            replace_dir_contents(&staging, target)
                .with_context(|| format!("restoring into {}", target.display()))?;
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn extract_rooted(zip_path: &Path, expected_root: &str, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).context("reading save zip")?;
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
            root.eq_ignore_ascii_case(expected_root),
            "save zip is for {} but this game is {}",
            root,
            expected_root
        );
        let rel: PathBuf = comps.collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&rel);
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
    Ok(())
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
    for (path, rel) in walk_files(src)? {
        let out = dest.join(rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&path, &out)?;
    }
    Ok(())
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
        zip_dir(&src, "0100000000010000", &zip).unwrap();

        let dest = base.join("dest");
        let mirror = base.join("mirror");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("stale.bin"), b"old").unwrap();

        let backup = base.join("backups/1");
        restore_zip(&zip, "0100000000010000", &[dest.clone(), mirror.clone()], &backup).unwrap();

        for d in [&dest, &mirror] {
            assert_eq!(std::fs::read(d.join("Common.bin")).unwrap(), b"common");
            assert_eq!(std::fs::read(d.join("slot_00/progress.sav")).unwrap(), b"progress");
            assert!(!d.join("stale.bin").exists());
        }
        assert_eq!(std::fs::read(backup.join("stale.bin")).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rejects_other_title() {
        let base = scratch("reject");
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.bin"), b"a").unwrap();
        let zip = base.join("save.zip");
        zip_dir(&src, "0100AAAA00000000", &zip).unwrap();

        let dest = base.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("keep.bin"), b"keep").unwrap();
        assert!(restore_zip(&zip, "0100000000010000", &[dest.clone()], &base.join("bk")).is_err());
        assert_eq!(std::fs::read(dest.join("keep.bin")).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(&base);
    }
}
