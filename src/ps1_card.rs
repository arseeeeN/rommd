//! PlayStation memory cards: a game's saves taken off its cards and put back,
//! as argosy-launcher's Sigil does (`src/card_ps1.c`, `src/sync_units.c`,
//! `docs/platforms/psx.md`).
//!
//! The unit is one raw 128 KiB card, freshly formatted, holding the game's
//! saves from its card in slot 1 and then slot 2, in directory order; it
//! uploads as `<rom stem>.mcd` and its hash is the card's MD5. Built the same
//! way from the same saves, it is byte for byte Sigil's, so both sides agree
//! it is in sync. argosy's older uploads (the emulator's raw card, a
//! libretro `.srm`) and rommd's earlier zip of the `_1`/`_2` cards restore
//! too, as do DexDrive `.gme` and PSP `.vmp` cards.
//!
//! DuckStation names a card per game, so every save on the game's cards is
//! taken as the game's. Sigil instead takes the saves whose product code is
//! one of the game's; the two agree unless a card holds another game's save.

use anyhow::{Context, Result, ensure};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::emulator::Ps1Cards;
use crate::save_archive;

pub const CARD_SIZE: usize = 128 * 1024;
const BLOCK_SIZE: usize = 8192;
const FRAME_SIZE: usize = 128;
const DATA_BLOCKS: usize = 15;
const NAME: std::ops::Range<usize> = 0x0A..0x0A + 20;

const STATE_FIRST: u32 = 0x51;
const STATE_MIDDLE: u32 = 0x52;
const STATE_LAST: u32 = 0x53;
const STATE_FREE: u32 = 0xA0;
const STATE_DELETED_LAST: u32 = 0xA3;
const LINK_END: u16 = 0xFFFF;
const WRITE_TEST_FRAME: usize = 63;
const BROKEN_FRAMES: std::ops::Range<usize> = 16..36;

const GME_HEADER: usize = 0xF40;
const VMP_HEADER: usize = 0x80;

/// One save in `.mcs` form: its directory frame with the link cleared, then
/// its blocks in chain order.
#[derive(Debug, Clone, PartialEq)]
pub struct Save(Vec<u8>);

impl Save {
    fn name(&self) -> &[u8] {
        &self.0[NAME]
    }

    fn blocks(&self) -> usize {
        (self.0.len() - FRAME_SIZE) / BLOCK_SIZE
    }
}

/// A raw card image.
pub struct Card(Vec<u8>);

impl Card {
    /// An empty card as PS1 emulators format one (`sigil_ps1_format`).
    pub fn formatted() -> Self {
        let mut image = vec![0u8; CARD_SIZE];
        image[0] = b'M';
        image[1] = b'C';
        seal(&mut image[..FRAME_SIZE]);
        for block in 1..=DATA_BLOCKS {
            let f = frame_mut(&mut image, block);
            write_frame(f, STATE_FREE, 0, LINK_END);
            seal(f);
        }
        for frame in BROKEN_FRAMES {
            let f = frame_mut(&mut image, frame);
            write_frame(f, 0xFFFF_FFFF, 0, LINK_END);
            seal(f);
        }
        image.copy_within(..FRAME_SIZE, WRITE_TEST_FRAME * FRAME_SIZE);
        Card(image)
    }

    /// A card from a file's bytes: raw (`.mcd`, `.mcr`, a libretro `.srm`),
    /// DexDrive `.gme` or PSP `.vmp`. `None` when it isn't one.
    pub fn load(data: &[u8]) -> Option<Self> {
        let offset = if data.starts_with(b"\0PMV") {
            VMP_HEADER
        } else if data.starts_with(b"123-456-STD") {
            GME_HEADER
        } else if data.starts_with(b"MC") && data.len() <= CARD_SIZE {
            0
        } else {
            return None;
        };
        let body = data.get(offset..)?;
        // A .gme may end early, as tools write them; anything else is whole.
        let whole = body.len() >= CARD_SIZE;
        if !body.starts_with(b"MC") || body.len() < BLOCK_SIZE || !whole && offset != GME_HEADER {
            return None;
        }
        let mut image = vec![0u8; CARD_SIZE];
        let n = body.len().min(CARD_SIZE);
        image[..n].copy_from_slice(&body[..n]);
        Some(Card(image))
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    fn state(&self, block: usize) -> u32 {
        u32::from_le_bytes(
            self.0[block * FRAME_SIZE..block * FRAME_SIZE + 4]
                .try_into()
                .unwrap(),
        )
    }

    fn link(&self, block: usize) -> u16 {
        let at = block * FRAME_SIZE + 8;
        u16::from_le_bytes([self.0[at], self.0[at + 1]])
    }

    /// The blocks of the save starting at `first`, in chain order; `None`
    /// when a link leaves the card, loops, or lands outside a live save.
    fn chain(&self, first: usize) -> Option<Vec<usize>> {
        let mut order = vec![first];
        let mut block = first;
        loop {
            let link = self.link(block);
            if link == LINK_END {
                return Some(order);
            }
            let next = link as usize + 1;
            if next > DATA_BLOCKS
                || order.contains(&next)
                || !matches!(self.state(next), STATE_MIDDLE | STATE_LAST)
            {
                return None;
            }
            order.push(next);
            block = next;
        }
    }

    /// The live saves in directory order. A save whose chain is broken or
    /// shares a block with another is left out, as Sigil lists it corrupt.
    pub fn saves(&self) -> Vec<Save> {
        let chains: Vec<(usize, Option<Vec<usize>>)> = (1..=DATA_BLOCKS)
            .filter(|&b| self.state(b) == STATE_FIRST)
            .map(|b| (b, self.chain(b)))
            .collect();
        let mut claims = [0u8; DATA_BLOCKS + 1];
        for (_, chain) in &chains {
            for &b in chain.iter().flatten() {
                claims[b] += 1;
            }
        }
        chains
            .into_iter()
            .filter_map(|(first, chain)| {
                let chain = chain.filter(|c| c.iter().all(|&b| claims[b] == 1))?;
                let mut mcs = self.0[first * FRAME_SIZE..(first + 1) * FRAME_SIZE].to_vec();
                mcs[8] = 0xFF;
                mcs[9] = 0xFF;
                seal(&mut mcs);
                for b in chain {
                    mcs.extend_from_slice(&self.0[b * BLOCK_SIZE..(b + 1) * BLOCK_SIZE]);
                }
                Some(Save(mcs))
            })
            .collect()
    }

    /// Add `save`, taking the lowest free blocks (`sigil_ps1_inject`).
    pub fn inject(&mut self, save: &Save) -> Result<()> {
        let blocks = save.blocks();
        ensure!(
            !(1..=DATA_BLOCKS)
                .any(|b| self.state(b) == STATE_FIRST
                    && self.0[b * FRAME_SIZE..][NAME] == *save.name()),
            "the card already holds {}",
            String::from_utf8_lossy(save.name())
        );
        let free: Vec<usize> = (1..=DATA_BLOCKS)
            .filter(|&b| (STATE_FREE..=STATE_DELETED_LAST).contains(&self.state(b)))
            .take(blocks)
            .collect();
        ensure!(
            free.len() == blocks,
            "the card has no room for the game's saves"
        );
        for (i, &block) in free.iter().enumerate() {
            let last = i + 1 == blocks;
            let link = if last {
                LINK_END
            } else {
                (free[i + 1] - 1) as u16
            };
            let f = frame_mut(&mut self.0, block);
            if i == 0 {
                f.copy_from_slice(&save.0[..FRAME_SIZE]);
                write_frame(f, STATE_FIRST, (blocks * BLOCK_SIZE) as u32, link);
            } else {
                f.fill(0);
                write_frame(f, if last { STATE_LAST } else { STATE_MIDDLE }, 0, link);
            }
            seal(f);
            let data = &save.0[FRAME_SIZE + i * BLOCK_SIZE..FRAME_SIZE + (i + 1) * BLOCK_SIZE];
            self.0[block * BLOCK_SIZE..(block + 1) * BLOCK_SIZE].copy_from_slice(data);
        }
        Ok(())
    }

    /// A formatted card holding `saves`, in order.
    pub fn with_saves(saves: &[Save]) -> Result<Self> {
        let mut card = Card::formatted();
        for save in saves {
            card.inject(save)?;
        }
        Ok(card)
    }
}

fn frame_mut(image: &mut [u8], frame: usize) -> &mut [u8] {
    &mut image[frame * FRAME_SIZE..(frame + 1) * FRAME_SIZE]
}

fn write_frame(f: &mut [u8], state: u32, size: u32, link: u16) {
    f[0..4].copy_from_slice(&state.to_le_bytes());
    f[4..8].copy_from_slice(&size.to_le_bytes());
    f[8..10].copy_from_slice(&link.to_le_bytes());
}

/// The frame's last byte: the XOR of the others.
fn seal(f: &mut [u8]) {
    let x = f[..FRAME_SIZE - 1].iter().fold(0, |x, b| x ^ b);
    f[FRAME_SIZE - 1] = x;
}

/// Saves in order, the first of each name kept.
fn push_unique(saves: &mut Vec<Save>, more: Vec<Save>) {
    for save in more {
        if !saves.iter().any(|s| s.name() == save.name()) {
            saves.push(save);
        }
    }
}

// ── A game's cards ────────────────────────────────────────────────────────────

/// The game's saves on its cards, slot 1 first.
fn local_saves(cards: &Ps1Cards) -> Result<Vec<Save>> {
    let mut saves = Vec::new();
    for path in cards.cards.iter().filter(|p| p.is_file()) {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let card = Card::load(&data)
            .with_context(|| format!("{} is not a PS1 memory card", path.display()))?;
        push_unique(&mut saves, card.saves());
    }
    Ok(saves)
}

/// The unit for the game's saves; `None` when it has none.
fn unit(cards: &Ps1Cards) -> Result<Option<Card>> {
    let saves = local_saves(cards)?;
    if saves.is_empty() {
        return Ok(None);
    }
    Card::with_saves(&saves).map(Some)
}

/// RomM's `content_hash` of the unit; `None` when the game has no save.
pub fn local_hash(cards: &Ps1Cards) -> Result<Option<String>> {
    Ok(unit(cards)?.map(|c| save_archive::bytes_md5(c.bytes())))
}

pub fn local_mtime(cards: &Ps1Cards) -> Option<SystemTime> {
    cards
        .cards
        .iter()
        .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .max()
}

/// Write the unit to `dest`.
pub fn write_unit(cards: &Ps1Cards, dest: &Path) -> Result<()> {
    let card = unit(cards)?.context("the game has no save on its memory cards")?;
    Ok(std::fs::write(dest, card.bytes())?)
}

/// The saves a RomM entry holds: a card in any form, or a zip of cards
/// (rommd's earlier uploads, by member name so `_1` comes before `_2`).
pub fn read_remote(path: &Path) -> Result<Vec<Save>> {
    let mut data = Vec::new();
    std::fs::File::open(path)?
        .take(4 * 1024 * 1024)
        .read_to_end(&mut data)?;
    let mut saves = Vec::new();
    if data.starts_with(b"PK\x03\x04") {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&data))?;
        let mut names: Vec<String> = zip.file_names().map(str::to_owned).collect();
        names.sort();
        for name in names {
            let mut member = Vec::new();
            zip.by_name(&name)?
                .take(CARD_SIZE as u64 + GME_HEADER as u64)
                .read_to_end(&mut member)?;
            let card = Card::load(&member).with_context(|| format!("{name} is not a card"))?;
            push_unique(&mut saves, card.saves());
        }
    } else {
        let card = Card::load(&data).context("not a PS1 memory card")?;
        saves = card.saves();
    }
    ensure!(!saves.is_empty(), "the memory card holds no save");
    Ok(saves)
}

/// The hash `local_hash` gives once the saves in `path` are restored.
pub fn remote_hash(path: &Path) -> Result<String> {
    Ok(save_archive::bytes_md5(
        Card::with_saves(&read_remote(path)?)?.bytes(),
    ))
}

/// Make the game's cards hold the saves in `remote`: a save goes back to the
/// slot 2 card when that card holds one of its name, else to the slot 1
/// card. Each card is rebuilt with only its saves; the old cards are backed
/// up to `backup` first. A slot 2 card that doesn't exist is not created.
pub fn restore(cards: &Ps1Cards, remote: &Path, backup: &Path) -> Result<()> {
    let saves = read_remote(remote)?;
    let [first, second] = &cards.cards;
    let on_second: Vec<Save> = match std::fs::read(second).ok().and_then(|d| Card::load(&d)) {
        Some(card) => card.saves(),
        None => Vec::new(),
    };
    let (to_second, to_first): (Vec<Save>, Vec<Save>) = saves
        .into_iter()
        .partition(|s| on_second.iter().any(|o| o.name() == s.name()));
    let mut writes: Vec<(&PathBuf, Card)> = vec![(first, Card::with_saves(&to_first)?)];
    if second.is_file() {
        writes.push((second, Card::with_saves(&to_second)?));
    }

    let existing: Vec<&PathBuf> = cards.cards.iter().filter(|p| p.is_file()).collect();
    if !existing.is_empty() {
        std::fs::create_dir_all(backup)?;
        for path in existing {
            std::fs::copy(path, backup.join(path.file_name().unwrap_or_default()))?;
        }
    }
    for (path, card) in writes {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let staged = path.with_file_name(format!(
            ".{}.rommd-tmp",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        std::fs::write(&staged, card.bytes())?;
        std::fs::rename(&staged, path)?;
    }
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::testing::{TempDir, write};

    /// A save of `blocks` blocks named `name` (e.g. `BASLUS-00594GAME00`).
    pub fn save(name: &str, blocks: usize, fill: u8) -> Save {
        let mut mcs = vec![0u8; FRAME_SIZE];
        write_frame(
            &mut mcs,
            STATE_FIRST,
            (blocks * BLOCK_SIZE) as u32,
            LINK_END,
        );
        mcs[NAME][..name.len()].copy_from_slice(name.as_bytes());
        seal(&mut mcs);
        mcs.extend(std::iter::repeat_n(fill, blocks * BLOCK_SIZE));
        Save(mcs)
    }

    pub fn card(saves: &[Save]) -> Vec<u8> {
        Card::with_saves(saves).unwrap().bytes().to_vec()
    }

    #[test]
    fn a_formatted_card_matches_sigils() {
        let card = Card::formatted();
        let b = card.bytes();
        assert_eq!(&b[..2], b"MC");
        assert_eq!(b[FRAME_SIZE - 1], b'M' ^ b'C');
        assert_eq!(&b[FRAME_SIZE..FRAME_SIZE + 4], &STATE_FREE.to_le_bytes());
        assert_eq!(&b[FRAME_SIZE + 8..FRAME_SIZE + 10], &[0xFF, 0xFF]);
        assert_eq!(&b[16 * FRAME_SIZE..16 * FRAME_SIZE + 4], &[0xFF; 4]);
        assert_eq!(
            &b[WRITE_TEST_FRAME * FRAME_SIZE..][..FRAME_SIZE],
            &b[..FRAME_SIZE]
        );
        assert!(card.saves().is_empty());
    }

    #[test]
    fn saves_round_trip_through_a_card() {
        let (a, b) = (save("BASLUS-00594AAA", 2, 1), save("BASLUS-00594BBB", 1, 2));
        let card = Card::load(&card(&[a.clone(), b.clone()])).unwrap();
        assert_eq!(card.saves(), [a.clone(), b.clone()]);
        let mut full = Card::with_saves(std::slice::from_ref(&a)).unwrap();
        assert!(full.inject(&a).is_err(), "same name");
        let big = save("BASLUS-00594CCC", 14, 3);
        assert!(full.inject(&big).is_err(), "no room");
    }

    #[test]
    fn a_broken_chain_is_left_out() {
        let mut image = card(&[save("BASLUS-00594AAA", 2, 1), save("BASLUS-00594BBB", 1, 2)]);
        // Point the first save's chain at the second save's first block.
        image[FRAME_SIZE + 8..FRAME_SIZE + 10].copy_from_slice(&2u16.to_le_bytes());
        let names: Vec<_> = Card::load(&image)
            .unwrap()
            .saves()
            .iter()
            .map(|s| s.name()[..15].to_vec())
            .collect();
        assert_eq!(names, [b"BASLUS-00594BBB".to_vec()]);
    }

    #[test]
    fn loads_gme_and_vmp_cards() {
        let raw = card(&[save("BASLUS-00594AAA", 1, 1)]);
        let mut gme = b"123-456-STD".to_vec();
        gme.resize(GME_HEADER, 0);
        gme.extend_from_slice(&raw[..BLOCK_SIZE * 2]);
        assert_eq!(Card::load(&gme).unwrap().saves().len(), 1);
        let mut vmp = b"\0PMV".to_vec();
        vmp.resize(VMP_HEADER, 0);
        vmp.extend_from_slice(&raw);
        assert_eq!(Card::load(&vmp).unwrap().saves().len(), 1);
        assert!(Card::load(b"nope").is_none());
    }

    fn cards(root: &Path) -> Ps1Cards {
        Ps1Cards {
            cards: [root.join("Game_1.mcd"), root.join("Game_2.mcd")],
        }
    }

    #[test]
    fn restore_puts_saves_back_on_their_cards() {
        let root = TempDir::new("ps1-restore");
        let (a, b, c) = (
            save("BASLUS-00594AAA", 1, 1),
            save("BASLUS-00594BBB", 1, 2),
            save("BASLUS-00594CCC", 1, 3),
        );
        write(
            &root.path().join("Game_1.mcd"),
            &card(std::slice::from_ref(&a)),
        );
        write(
            &root.path().join("Game_2.mcd"),
            &card(std::slice::from_ref(&b)),
        );
        let set = cards(root.path());
        assert_eq!(
            local_hash(&set).unwrap(),
            Some(save_archive::bytes_md5(&card(&[a.clone(), b.clone()])))
        );

        let b2 = save("BASLUS-00594BBB", 1, 9);
        write(
            &root.path().join("unit.mcd"),
            &card(&[c.clone(), b2.clone()]),
        );
        restore(&set, &root.path().join("unit.mcd"), &root.path().join("bk")).unwrap();
        assert_eq!(
            std::fs::read(root.path().join("Game_1.mcd")).unwrap(),
            card(&[c])
        );
        assert_eq!(
            std::fs::read(root.path().join("Game_2.mcd")).unwrap(),
            card(&[b2])
        );
        assert!(root.path().join("bk/Game_1.mcd").is_file());
    }

    #[test]
    fn restores_rommds_earlier_zip_of_cards() {
        let root = TempDir::new("ps1-zip");
        let (a, b) = (save("BASLUS-00594AAA", 1, 1), save("BASLUS-00594BBB", 2, 2));
        let zip = root.path().join("old.zip");
        let entries = [
            (root.path().join("z2"), "Game_2.mcd".to_owned()),
            (root.path().join("z1"), "Game_1.mcd".to_owned()),
        ];
        write(&entries[0].0, &card(std::slice::from_ref(&b)));
        write(&entries[1].0, &card(std::slice::from_ref(&a)));
        save_archive::zip_entries(&entries, &zip).unwrap();
        assert_eq!(read_remote(&zip).unwrap(), [a, b]);

        write(&root.path().join("empty.mcd"), Card::formatted().bytes());
        assert!(read_remote(&root.path().join("empty.mcd")).is_err());
    }
}
