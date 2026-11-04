//! PS2 saves on memory cards every game shares, synced as argosy-launcher's
//! Sigil does (`src/sync_cards.c`, `src/sync_units.c`,
//! `docs/platforms/ps2.md`).
//!
//! The unit is one 8 MB card with ECC, formatted with the root time
//! 2000-01-01, holding the game's save folders from the slot 1 card and then
//! slot 2 in directory order; it uploads as `<rom stem>.ps2` and its hash is
//! the file's MD5, byte for byte Sigil's for the same folders. A folder is
//! the game's when its name carries one of the game's product codes
//! (`BASLUS-20152…`). argosy sends this unit for a game on shared cards
//! whatever RomM's version.
//!
//! Restore takes the game's folders off every card, puts each one the unit
//! holds back on the card it was on and a new one on the slot 1 card, and
//! writes nothing until every folder fits; other games' saves stay as they
//! are. Each card it changes is backed up first.

pub mod card;

use anyhow::{Context, Result, bail, ensure};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::emulator::Ps2Cards;
use crate::save_archive;
use card::{Card, Folder, Tod};

/// The root time of a card Sigil builds: 2000-01-01 00:00:00.
const BUILT_TOD: Tod = [0, 0, 0, 0, 1, 1, 0xD0, 0x07];

/// A card file: absent, empty, or every byte 0xFF (a slot PCSX2 created and
/// nothing formatted) is no card; anything else must be one.
fn load(path: &Path) -> Result<Option<Card>> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if data.iter().all(|&b| b == 0xFF) {
        return Ok(None);
    }
    Card::load(&data)
        .map(Some)
        .with_context(|| format!("{} is not a PS2 memory card", path.display()))
}

fn owned(cards: &Ps2Cards, owner: Option<String>) -> bool {
    owner.is_some_and(|o| cards.game_ids.contains(&o))
}

/// The game's folders on `card`, in directory order. A folder of the game
/// that doesn't read is refused: going on would take it as deleted.
fn game_folders(cards: &Ps2Cards, card: &Card, path: &Path) -> Result<Vec<(u32, Folder)>> {
    let (saves, corrupt) = card.list()?;
    if let Some(bad) = corrupt.iter().find(|c| owned(cards, c.owner())) {
        bail!(
            "{} holds a damaged save of the game ({}); repair the card first",
            path.display(),
            String::from_utf8_lossy(&bad.name)
        );
    }
    saves
        .iter()
        .filter(|s| owned(cards, s.owner()))
        .map(|s| Ok((s.first_cluster, card.extract(s.first_cluster)?)))
        .collect()
}

/// The game's folders on its cards, slot 1 first; the first of a name kept.
fn local_folders(cards: &Ps2Cards) -> Result<Vec<Folder>> {
    let mut folders: Vec<Folder> = Vec::new();
    for path in &cards.cards {
        let Some(card) = load(path)? else { continue };
        for (_, folder) in game_folders(cards, &card, path)? {
            if !folders.iter().any(|f| f.name() == folder.name()) {
                folders.push(folder);
            }
        }
    }
    Ok(folders)
}

fn unit_of(folders: &[Folder]) -> Result<Card> {
    let mut unit = Card::formatted(BUILT_TOD);
    for folder in folders {
        unit.inject(folder)?;
    }
    Ok(unit)
}

/// The unit file for the game's saves; `None` when it has none.
fn unit(cards: &Ps2Cards) -> Result<Option<Vec<u8>>> {
    let folders = local_folders(cards)?;
    if folders.is_empty() {
        return Ok(None);
    }
    Ok(Some(unit_of(&folders)?.to_file()))
}

/// RomM's `content_hash` of the unit; `None` when the game has no save.
pub fn local_hash(cards: &Ps2Cards) -> Result<Option<String>> {
    Ok(unit(cards)?.map(|u| save_archive::bytes_md5(&u)))
}

/// When the cards holding the game's saves last changed. Every game writes
/// to them, so this can be later than the game's own last save; the card's
/// own times are in the PS2's clock, whose zone PCSX2 doesn't record.
pub fn local_mtime(cards: &Ps2Cards) -> Option<SystemTime> {
    cards
        .cards
        .iter()
        .filter(|p| {
            load(p)
                .ok()
                .flatten()
                .and_then(|c| game_folders(cards, &c, p).ok())
                .is_some_and(|f| !f.is_empty())
        })
        .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .max()
}

pub fn write_unit(cards: &Ps2Cards, dest: &Path) -> Result<()> {
    let unit = unit(cards)?.context("the game has no save on the memory cards")?;
    Ok(std::fs::write(dest, unit)?)
}

/// The game's folders a RomM entry holds: a `.ps2` card. A card holding a
/// damaged save of the game, or none of its saves, is refused.
fn read_remote(cards: &Ps2Cards, path: &Path) -> Result<Vec<Folder>> {
    let card = load(path)?.context("RomM's save is not a PS2 memory card")?;
    let folders: Vec<Folder> = game_folders(cards, &card, path)?
        .into_iter()
        .map(|(_, f)| f)
        .collect();
    ensure!(
        !folders.is_empty(),
        "RomM's memory card holds no save of {}",
        cards.game_ids.join(" or ")
    );
    Ok(folders)
}

/// The hash `local_hash` gives once the save in `path` is restored.
pub fn remote_hash(cards: &Ps2Cards, path: &Path) -> Result<String> {
    Ok(save_archive::bytes_md5(
        &unit_of(&read_remote(cards, path)?)?.to_file(),
    ))
}

pub fn restore(cards: &Ps2Cards, remote: &Path, backup: &Path) -> Result<()> {
    let incoming = read_remote(cards, remote)?;
    if let Some(check) = &cards.writable {
        (check.0)()?;
    }
    let mut loaded: Vec<(PathBuf, Option<Card>, bool)> = Vec::new();
    for path in &cards.cards {
        loaded.push((path.clone(), load(path)?, false));
    }

    // Take the game's folders off every card, noting where each name sat.
    let mut placed_on: Vec<Option<usize>> = vec![None; incoming.len()];
    for (c, (path, card, changed)) in loaded.iter_mut().enumerate() {
        let Some(card) = card else { continue };
        for (first, folder) in game_folders(cards, card, path)? {
            card.delete(first)?;
            *changed = true;
            if let Some(i) = incoming.iter().position(|f| f.name() == folder.name()) {
                placed_on[i] = Some(c);
            }
        }
    }
    // A new folder goes on the slot 1 card, formatted now when it holds no
    // card yet. Sigil takes the first card file present, which is slot 2's
    // when slot 1's is missing, but games look in slot 1 first, and PCSX2
    // creates a missing card when it starts.
    for (folder, on) in incoming.iter().zip(placed_on) {
        let c = on.unwrap_or(0);
        let (path, card, changed) = &mut loaded[c];
        let card = card.get_or_insert_with(|| Card::formatted(BUILT_TOD));
        card.inject(folder).with_context(|| {
            format!(
                "putting {} on {}",
                String::from_utf8_lossy(folder.name()),
                path.display()
            )
        })?;
        *changed = true;
    }

    let mut writes = Vec::new();
    for (path, card, changed) in &loaded {
        let Some(card) = card.as_ref().filter(|_| *changed) else {
            continue;
        };
        let file = card.to_file();
        verify(cards, &file, &incoming, path)?;
        writes.push((path, file));
    }
    let existing: Vec<&PathBuf> = writes
        .iter()
        .map(|(p, _)| *p)
        .filter(|p| p.is_file())
        .collect();
    if !existing.is_empty() {
        std::fs::create_dir_all(backup)?;
        for path in existing {
            std::fs::copy(path, backup.join(path.file_name().unwrap_or_default()))?;
        }
    }
    for (path, file) in writes {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let staged = path.with_file_name(format!(
            ".{}.rommd-tmp",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        std::fs::write(&staged, &file)?;
        std::fs::rename(&staged, path)?;
    }
    Ok(())
}

/// The card `file` reads back, and every folder of `incoming` it holds is
/// whole: the same entries (but for their clusters) and bytes.
fn verify(cards: &Ps2Cards, file: &[u8], incoming: &[Folder], path: &Path) -> Result<()> {
    let card = Card::load(file).context("the rebuilt card doesn't read back")?;
    for (_, folder) in game_folders(cards, &card, path)? {
        let want = incoming
            .iter()
            .find(|f| f.name() == folder.name())
            .context("the rebuilt card holds a save it shouldn't")?;
        let same = want.files.len() == folder.files.len()
            && want
                .files
                .iter()
                .zip(&folder.files)
                .all(|((ea, da), (eb, db))| card::same_entry(ea, eb) && da == db);
        ensure!(
            same,
            "{} didn't read back whole from {}",
            String::from_utf8_lossy(want.name()),
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::card::tests::folder;
    use super::*;
    use crate::testing::{TempDir, write};

    fn cards(dir: &Path) -> Ps2Cards {
        Ps2Cards {
            cards: vec![dir.join("Mcd001.ps2"), dir.join("Mcd002.ps2")],
            game_ids: vec!["SLUS-20152".into()],
            writable: None,
        }
    }

    fn card_with(folders: &[Folder]) -> Vec<u8> {
        unit_of(folders).unwrap().to_file()
    }

    fn names(file: &[u8]) -> Vec<String> {
        let card = Card::load(file).unwrap();
        card.list()
            .unwrap()
            .0
            .iter()
            .map(|s| String::from_utf8_lossy(&s.name).into_owned())
            .collect()
    }

    #[test]
    fn the_unit_holds_only_the_games_saves_slot_1_first() {
        let root = TempDir::new("ps2-unit");
        let (a, b) = (
            folder("BASLUS-20152AAA", &[("data", vec![1; 100])]),
            folder("BASLUS-20152BBB", &[("data", vec![2; 100])]),
        );
        let other = folder("BASLUS-99999ZZZ", &[("data", vec![9; 100])]);
        write(
            &root.path().join("Mcd001.ps2"),
            &card_with(&[other.clone(), b.clone()]),
        );
        write(
            &root.path().join("Mcd002.ps2"),
            &card_with(std::slice::from_ref(&a)),
        );
        let set = cards(root.path());
        let dest = root.path().join("unit.ps2");
        write_unit(&set, &dest).unwrap();
        let unit = std::fs::read(&dest).unwrap();
        assert_eq!(names(&unit), ["BASLUS-20152BBB", "BASLUS-20152AAA"]);
        assert_eq!(unit, card_with(&[b, a]), "built as Sigil builds it");
        assert_eq!(
            local_hash(&set).unwrap(),
            Some(save_archive::bytes_md5(&unit))
        );
    }

    #[test]
    fn erased_and_missing_cards_hold_nothing() {
        let root = TempDir::new("ps2-erased");
        write(&root.path().join("Mcd001.ps2"), &vec![0xFF; 8_650_752]);
        assert_eq!(local_hash(&cards(root.path())).unwrap(), None);
        write(&root.path().join("Mcd002.ps2"), b"junk");
        assert!(local_hash(&cards(root.path())).is_err());
    }

    #[test]
    fn restore_replaces_only_the_games_saves_where_they_were() {
        let root = TempDir::new("ps2-restore");
        let other = folder("BASLUS-99999ZZZ", &[("data", vec![9; 100])]);
        let old_a = folder("BASLUS-20152AAA", &[("data", vec![1; 100])]);
        let gone = folder("BASLUS-20152GONE", &[("data", vec![3; 100])]);
        write(
            &root.path().join("Mcd001.ps2"),
            &card_with(&[other.clone(), gone]),
        );
        write(&root.path().join("Mcd002.ps2"), &card_with(&[old_a]));

        let new_a = folder("BASLUS-20152AAA", &[("data", vec![7; 3000])]);
        let new_b = folder("BASLUS-20152BBB", &[("data", vec![8; 10])]);
        let unit = root.path().join("unit.ps2");
        write(&unit, &card_with(&[new_a.clone(), new_b.clone()]));
        let set = cards(root.path());
        restore(&set, &unit, &root.path().join("bk")).unwrap();

        let slot1 = std::fs::read(root.path().join("Mcd001.ps2")).unwrap();
        let slot2 = std::fs::read(root.path().join("Mcd002.ps2")).unwrap();
        assert_eq!(names(&slot1), ["BASLUS-99999ZZZ", "BASLUS-20152BBB"]);
        assert_eq!(names(&slot2), ["BASLUS-20152AAA"]);
        assert!(root.path().join("bk/Mcd001.ps2").is_file());
    }

    #[test]
    fn restore_formats_an_erased_slot_1_card() {
        let root = TempDir::new("ps2-fresh");
        write(&root.path().join("Mcd001.ps2"), &vec![0xFF; 8_650_752]);
        let a = folder("BASLUS-20152AAA", &[("data", vec![1; 100])]);
        let unit = root.path().join("unit.ps2");
        write(&unit, &card_with(std::slice::from_ref(&a)));
        let set = cards(root.path());
        restore(&set, &unit, &root.path().join("bk")).unwrap();
        assert_eq!(
            std::fs::read(root.path().join("Mcd001.ps2")).unwrap(),
            card_with(&[a])
        );
        assert_eq!(
            remote_hash(&set, &unit).unwrap(),
            local_hash(&set).unwrap().unwrap()
        );
        assert!(!root.path().join("Mcd002.ps2").exists());
    }

    #[test]
    fn a_new_save_goes_to_slot_1_even_when_only_slot_2_has_a_card() {
        let root = TempDir::new("ps2-slot1");
        write(&root.path().join("Mcd002.ps2"), &vec![0xFF; 8_650_752]);
        let a = folder("BASLUS-20152AAA", &[("data", vec![1; 100])]);
        let unit = root.path().join("unit.ps2");
        write(&unit, &card_with(std::slice::from_ref(&a)));
        restore(&cards(root.path()), &unit, &root.path().join("bk")).unwrap();
        assert_eq!(
            std::fs::read(root.path().join("Mcd001.ps2")).unwrap(),
            card_with(&[a])
        );
        assert!(
            std::fs::read(root.path().join("Mcd002.ps2"))
                .unwrap()
                .iter()
                .all(|&b| b == 0xFF),
            "slot 2's erased card untouched"
        );
    }

    #[test]
    fn restore_refuses_while_the_emulator_runs_or_a_unit_of_another_game() {
        let root = TempDir::new("ps2-refuse");
        let unit = root.path().join("unit.ps2");
        write(
            &unit,
            &card_with(&[folder("BASLUS-99999ZZZ", &[("d", vec![1])])]),
        );
        assert!(restore(&cards(root.path()), &unit, &root.path().join("bk")).is_err());

        write(
            &unit,
            &card_with(&[folder("BASLUS-20152AAA", &[("d", vec![1])])]),
        );
        let mut set = cards(root.path());
        set.writable = Some(crate::emulator::Hook(std::sync::Arc::new(|| {
            anyhow::bail!("PCSX2 is running")
        })));
        assert!(restore(&set, &unit, &root.path().join("bk")).is_err());
        assert!(!root.path().join("Mcd001.ps2").exists());
    }

    #[test]
    fn a_full_card_changes_nothing() {
        let root = TempDir::new("ps2-full");
        let filler = folder("BASLUS-99999ZZZ", &[("d", vec![0; 8_200_000])]);
        let before = card_with(std::slice::from_ref(&filler));
        write(&root.path().join("Mcd001.ps2"), &before);
        let unit = root.path().join("unit.ps2");
        write(
            &unit,
            &card_with(&[folder("BASLUS-20152AAA", &[("d", vec![1; 500_000])])]),
        );
        assert!(restore(&cards(root.path()), &unit, &root.path().join("bk")).is_err());
        assert_eq!(
            std::fs::read(root.path().join("Mcd001.ps2")).unwrap(),
            before
        );
    }
}
