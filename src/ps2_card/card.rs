//! The PS2 memory card filesystem, ported from argosy-launcher's Sigil
//! (`src/card_ps2.c`); layout notes in mymc's `ps2mc.py` and
//! <https://ps2savetools.com/ps2memcardformat.html>.
//!
//! A card is 512-byte pages, each followed in a `.ps2` file by 16 spare bytes
//! holding its ECC. Pages pair into 1 KiB clusters. A two-level FAT (the
//! superblock's indirect FAT clusters list the FAT clusters) chains clusters
//! from `alloc_offset` on; the root directory holds one folder per save, each
//! a directory of 512-byte entries followed by its files. A card built here
//! is byte for byte the one Sigil builds from the same folders.

use anyhow::{Context, Result, ensure};

const MAGIC: &[u8] = b"Sony PS2 Memory Card Format ";
const PAGE_LEN: usize = 512;
const SPARE_LEN: usize = 16;
const MAX_CLUSTERS: u32 = 1 << 20;
const IFC_SLOTS: usize = 32;
pub const ENTRY_SIZE: usize = 512;
const NAME_LEN: usize = 32;

const FAT_ALLOCATED: u32 = 0x8000_0000;
const FAT_CHAIN_END: u32 = 0x7FFF_FFFF;

const MODE_EXISTS: u16 = 0x8000;
const MODE_DIR: u16 = 0x0020;
const MODE_FILE: u16 = 0x0010;

const ENTRY_MODE: usize = 0x00;
const ENTRY_LENGTH: usize = 0x04;
const ENTRY_CREATED: usize = 0x08;
const ENTRY_CLUSTER: usize = 0x10;
const ENTRY_PARENT_INDEX: usize = 0x14;
const ENTRY_MODIFIED: usize = 0x18;
const ENTRY_NAME: usize = 0x40;

/// An 8 MB card as PCSX2 and mymc format one.
const FORMAT_CLUSTERS: u32 = 8192;
const PAGES_PER_CLUSTER: usize = 2;
const PAGES_PER_BLOCK: usize = 16;
const FORMAT_IFC: u32 = 8;
const FORMAT_FAT_FIRST: u32 = 9;
const FORMAT_ALLOC_OFFSET: u32 = 41;
const FORMAT_ALLOC_END: u32 = 8135;
const FORMAT_BACKUP1: u32 = 1023;
const FORMAT_BACKUP2: u32 = 1022;
const MODE_ROOT_SELF: u16 = 0x8427;
const MODE_ROOT_PARENT: u16 = 0xA426;
/// Fills a cluster's unused bytes.
const PAD: u8 = 0xFF;

/// A card time: unused, second, minute, hour, day, month, year (LE16).
pub type Tod = [u8; 8];

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn set_le16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn set_le32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn clusters_for(bytes: u64, cluster_size: usize) -> u32 {
    bytes.div_ceil(cluster_size as u64) as u32
}

/// One 512-byte directory entry.
#[derive(Clone, PartialEq, Eq)]
pub struct Entry(pub [u8; ENTRY_SIZE]);

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Entry({:?})", String::from_utf8_lossy(self.name()))
    }
}

impl Entry {
    fn from(b: &[u8]) -> Self {
        Entry(b[..ENTRY_SIZE].try_into().unwrap())
    }

    fn mode(&self) -> u16 {
        le16(&self.0, ENTRY_MODE)
    }

    pub fn length(&self) -> u32 {
        le32(&self.0, ENTRY_LENGTH)
    }

    fn cluster(&self) -> u32 {
        le32(&self.0, ENTRY_CLUSTER)
    }

    fn exists(&self) -> bool {
        self.mode() & MODE_EXISTS != 0
    }

    fn is_dir(&self) -> bool {
        self.exists() && self.mode() & MODE_DIR != 0
    }

    fn is_file(&self) -> bool {
        self.exists() && self.mode() & MODE_FILE != 0
    }

    /// The 32-byte name field, as compared on the card.
    fn name_field(&self) -> &[u8] {
        &self.0[ENTRY_NAME..ENTRY_NAME + NAME_LEN]
    }

    /// The name, up to its first NUL.
    pub fn name(&self) -> &[u8] {
        let field = self.name_field();
        &field[..field.iter().position(|&b| b == 0).unwrap_or(NAME_LEN)]
    }

    /// The time it was last modified.
    pub fn modified(&self) -> Tod {
        self.0[ENTRY_MODIFIED..ENTRY_MODIFIED + 8]
            .try_into()
            .unwrap()
    }

    fn created(&self) -> Tod {
        self.0[ENTRY_CREATED..ENTRY_CREATED + 8].try_into().unwrap()
    }
}

/// One save: a folder of the card's root, lifted off the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// The root's entry for it.
    pub entry: Entry,
    /// Its own "." and ".." entries, which carry the folder's times.
    pub self_entry: Entry,
    pub parent_entry: Entry,
    /// Its files in directory order: entry and bytes (the entry's length).
    pub files: Vec<(Entry, Vec<u8>)>,
}

impl Folder {
    pub fn name(&self) -> &[u8] {
        self.entry.name()
    }
}

/// Two entries alike but for the cluster they point at, which placement on a
/// card decides.
pub fn same_entry(a: &Entry, b: &Entry) -> bool {
    let (mut a, mut b) = (a.0, b.0);
    set_le32(&mut a, ENTRY_CLUSTER, 0);
    set_le32(&mut b, ENTRY_CLUSTER, 0);
    a == b
}

/// The product code a save folder's name carries (`BASLUS-20152…` →
/// `SLUS-20152`), which says whose save it is (Sigil's
/// `sigil_card_sony_owner`): `B`, a region letter (A, E, I), four capitals,
/// any byte, five digits.
pub fn owner(name: &[u8]) -> Option<String> {
    let ok = name.len() >= 12
        && name[0] == b'B'
        && matches!(name[1], b'A' | b'E' | b'I')
        && name[2..6].iter().all(u8::is_ascii_uppercase)
        && name[7..12].iter().all(u8::is_ascii_digit);
    ok.then(|| {
        format!(
            "{}-{}",
            String::from_utf8_lossy(&name[2..6]),
            String::from_utf8_lossy(&name[7..12])
        )
    })
}

/// A folder in the root, as `Card::list` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: Vec<u8>,
    pub first_cluster: u32,
}

impl Listed {
    pub fn owner(&self) -> Option<String> {
        owner(&self.name)
    }
}

/// A card with its pages joined into clusters and the ECC set aside.
#[derive(Clone)]
pub struct Card {
    /// `clusters_per_card` clusters of `cluster_size` bytes.
    clusters: Vec<u8>,
    cluster_size: usize,
    clusters_per_card: u32,
    /// Absolute cluster where allocatable clusters start.
    alloc_offset: u32,
    /// Allocatable clusters, counted from `alloc_offset`.
    alloc_end: u32,
    /// Relative to `alloc_offset`.
    rootdir_cluster: u32,
    /// Absolute clusters of the indirect FAT.
    ifc: [u32; IFC_SLOTS],
    /// 16 spare bytes per page as read; `None` for a card without ECC. Kept
    /// as read, since cards carry erased (0xFF) and stale spares.
    spare: Option<Vec<u8>>,
    /// Pages whose data changed; their spare is recomputed on write.
    dirty: Vec<bool>,
}

impl Card {
    /// A `.ps2` file card, with ECC or without. `None` when it isn't one.
    pub fn load(file: &[u8]) -> Option<Card> {
        let sb = file.get(..PAGE_LEN)?;
        if &sb[..MAGIC.len()] != MAGIC {
            return None;
        }
        let page_len = le16(sb, 0x28) as usize;
        let pages_per_cluster = le16(sb, 0x2A) as usize;
        let clusters = le32(sb, 0x30);
        if page_len != PAGE_LEN || !(1..=16).contains(&pages_per_cluster) {
            return None;
        }
        if clusters == 0 || clusters > MAX_CLUSTERS {
            return None;
        }
        let mut ifc = [0u32; IFC_SLOTS];
        for (i, slot) in ifc.iter_mut().enumerate() {
            *slot = le32(sb, 0x50 + i * 4);
        }
        let (alloc_offset, alloc_end) = (le32(sb, 0x34), le32(sb, 0x38));
        if alloc_offset >= clusters || alloc_end == 0 || alloc_end > clusters - alloc_offset {
            return None;
        }
        let pages = clusters as usize * pages_per_cluster;
        let ecc = if file.len() == pages * (PAGE_LEN + SPARE_LEN) {
            true
        } else if file.len() == pages * PAGE_LEN {
            false
        } else {
            return None;
        };
        let stride = PAGE_LEN + if ecc { SPARE_LEN } else { 0 };
        let mut data = Vec::with_capacity(pages * PAGE_LEN);
        let mut spare = ecc.then(|| Vec::with_capacity(pages * SPARE_LEN));
        for page in file.chunks_exact(stride) {
            data.extend_from_slice(&page[..PAGE_LEN]);
            if let Some(spare) = &mut spare {
                spare.extend_from_slice(&page[PAGE_LEN..]);
            }
        }
        Some(Card {
            clusters: data,
            cluster_size: page_len * pages_per_cluster,
            clusters_per_card: clusters,
            alloc_offset,
            alloc_end,
            rootdir_cluster: le32(sb, 0x3C),
            ifc,
            spare,
            dirty: vec![false; pages],
        })
    }

    /// An empty formatted 8 MB card with ECC, laid out as PCSX2 and mymc lay
    /// one out; `tod` is stamped on the root directory.
    pub fn formatted(tod: Tod) -> Card {
        let cluster_size = PAGE_LEN * PAGES_PER_CLUSTER;
        let pages = FORMAT_CLUSTERS as usize * PAGES_PER_CLUSTER;
        let mut ifc = [0u32; IFC_SLOTS];
        ifc[0] = FORMAT_IFC;
        let mut card = Card {
            clusters: vec![0; FORMAT_CLUSTERS as usize * cluster_size],
            cluster_size,
            clusters_per_card: FORMAT_CLUSTERS,
            alloc_offset: FORMAT_ALLOC_OFFSET,
            alloc_end: FORMAT_ALLOC_END,
            rootdir_cluster: 0,
            ifc,
            spare: Some(vec![0; pages * SPARE_LEN]),
            dirty: vec![true; pages],
        };
        superblock(&mut card.clusters[..PAGE_LEN]);

        let per_cluster = (cluster_size / 4) as u32;
        let fat_clusters = FORMAT_ALLOC_OFFSET - FORMAT_FAT_FIRST;
        let indirect = card.cluster_mut(FORMAT_IFC);
        indirect.fill(0xFF);
        for i in 0..fat_clusters {
            set_le32(indirect, i as usize * 4, FORMAT_FAT_FIRST + i);
        }
        for n in 0..fat_clusters * per_cluster {
            let value = if n == 0 || n >= FORMAT_ALLOC_END {
                0xFFFF_FFFF
            } else {
                FAT_CHAIN_END
            };
            let fat = card.cluster_mut(FORMAT_FAT_FIRST + n / per_cluster);
            set_le32(fat, (n % per_cluster) as usize * 4, value);
        }

        let root = card.cluster_mut(FORMAT_ALLOC_OFFSET);
        write_dir_entry(&mut root[..ENTRY_SIZE], MODE_ROOT_SELF, 2, 0, &tod, b".");
        write_dir_entry(
            &mut root[ENTRY_SIZE..2 * ENTRY_SIZE],
            MODE_ROOT_PARENT,
            0,
            0,
            &tod,
            b"..",
        );

        // The second backup block stays erased, as PCSX2 leaves it.
        let clusters_per_block = (PAGES_PER_BLOCK / PAGES_PER_CLUSTER) as u32;
        let first = (FORMAT_BACKUP2 * clusters_per_block) as usize * cluster_size;
        card.clusters[first..first + clusters_per_block as usize * cluster_size].fill(0xFF);
        let erased = FORMAT_BACKUP2 as usize * PAGES_PER_BLOCK;
        card.dirty[erased..erased + PAGES_PER_BLOCK].fill(false);
        card.spare.as_mut().unwrap()[erased * SPARE_LEN..(erased + PAGES_PER_BLOCK) * SPARE_LEN]
            .fill(0xFF);
        card
    }

    /// The card as a `.ps2` file: each page, then (with ECC) its spare bytes,
    /// recomputed for pages that changed.
    pub fn to_file(&self) -> Vec<u8> {
        let stride = PAGE_LEN + if self.spare.is_some() { SPARE_LEN } else { 0 };
        let mut out = Vec::with_capacity(self.dirty.len() * stride);
        for (p, page) in self.clusters.as_chunks::<PAGE_LEN>().0.iter().enumerate() {
            out.extend_from_slice(page);
            let Some(spare) = &self.spare else { continue };
            if !self.dirty[p] {
                out.extend_from_slice(&spare[p * SPARE_LEN..(p + 1) * SPARE_LEN]);
                continue;
            }
            let mut fresh = [0u8; SPARE_LEN];
            let chunks = page.as_chunks::<128>().0;
            for (chunk, ecc) in chunks.iter().zip(fresh.as_chunks_mut::<3>().0) {
                *ecc = chunk_ecc(chunk);
            }
            out.extend_from_slice(&fresh);
        }
        out
    }

    fn in_card(&self, absolute: u32) -> bool {
        absolute < self.clusters_per_card
    }

    fn cluster(&self, absolute: u32) -> &[u8] {
        let at = absolute as usize * self.cluster_size;
        &self.clusters[at..at + self.cluster_size]
    }

    fn cluster_mut(&mut self, absolute: u32) -> &mut [u8] {
        let at = absolute as usize * self.cluster_size;
        &mut self.clusters[at..at + self.cluster_size]
    }

    fn mark_dirty(&mut self, absolute: u32) {
        let pages = self.cluster_size / PAGE_LEN;
        let first = absolute as usize * pages;
        self.dirty[first..first + pages].fill(true);
    }

    /// Relative cluster `n`, marked changed.
    fn relative_mut(&mut self, n: u32) -> &mut [u8] {
        let absolute = n + self.alloc_offset;
        self.mark_dirty(absolute);
        self.cluster_mut(absolute)
    }

    /// The FAT cluster and offset holding relative cluster `n`'s entry,
    /// through the indirect FAT; `None` when a table lies outside the card.
    fn fat_slot(&self, n: u32) -> Option<(u32, usize)> {
        let per_cluster = (self.cluster_size / 4) as u32;
        let fat_index = n / per_cluster;
        let indirect = *self.ifc.get((fat_index / per_cluster) as usize)?;
        if !self.in_card(indirect) {
            return None;
        }
        let fat_cluster = le32(
            self.cluster(indirect),
            (fat_index % per_cluster) as usize * 4,
        );
        self.in_card(fat_cluster)
            .then_some((fat_cluster, (n % per_cluster) as usize * 4))
    }

    fn fat(&self, n: u32) -> Option<u32> {
        let (cluster, at) = self.fat_slot(n)?;
        Some(le32(self.cluster(cluster), at))
    }

    fn set_fat(&mut self, n: u32, value: u32) -> Result<()> {
        let (cluster, at) = self.fat_slot(n).context("the card's FAT is damaged")?;
        set_le32(self.cluster_mut(cluster), at, value);
        self.mark_dirty(cluster);
        Ok(())
    }

    fn is_free(&self, n: u32) -> bool {
        self.fat(n).is_some_and(|v| v & FAT_ALLOCATED == 0)
    }

    /// The chain from relative cluster `first`; `None` when it runs into a
    /// free cluster, leaves the card, or loops.
    fn chain(&self, first: u32) -> Option<Vec<u32>> {
        let mut order = Vec::new();
        let mut cluster = first;
        while (order.len() as u32) < self.alloc_end {
            if cluster >= self.alloc_end {
                return None;
            }
            order.push(cluster);
            let value = self.fat(cluster).filter(|v| v & FAT_ALLOCATED != 0)?;
            let next = value & !FAT_ALLOCATED;
            if next == FAT_CHAIN_END {
                return Some(order);
            }
            cluster = next;
        }
        None
    }

    /// `len` bytes of the chain from relative cluster `first`.
    fn read_chain(&self, first: u32, len: usize) -> Option<Vec<u8>> {
        let chain = self.chain(first)?;
        if chain.len() * self.cluster_size < len {
            return None;
        }
        let mut out = Vec::with_capacity(len);
        for &c in &chain {
            if out.len() >= len {
                break;
            }
            let absolute = c + self.alloc_offset;
            if !self.in_card(absolute) {
                return None;
            }
            let take = (len - out.len()).min(self.cluster_size);
            out.extend_from_slice(&self.cluster(absolute)[..take]);
        }
        Some(out)
    }

    /// The `count` entries of the directory starting at relative `first`.
    fn read_dir(&self, first: u32, count: u32) -> Option<Vec<Entry>> {
        let most = self.alloc_end as u64 * (self.cluster_size / ENTRY_SIZE) as u64;
        if count == 0 || count as u64 > most {
            return None;
        }
        let bytes = self.read_chain(first, count as usize * ENTRY_SIZE)?;
        Some(
            bytes
                .as_chunks::<ENTRY_SIZE>()
                .0
                .iter()
                .map(|e| Entry(*e))
                .collect(),
        )
    }

    fn root_count(&self) -> u32 {
        self.read_chain(self.rootdir_cluster, ENTRY_SIZE)
            .map_or(0, |b| le32(&b, ENTRY_LENGTH))
    }

    fn root(&self) -> Option<Vec<Entry>> {
        self.read_dir(self.rootdir_cluster, self.root_count())
    }

    /// Counts the first `span` clusters of the chain from `first` in
    /// `claims`, or with `check` says whether another chain reached one.
    fn claim_chain(&self, first: u32, span: u32, claims: &mut [u8], check: bool) -> bool {
        let chain = self.chain(first).unwrap_or_default();
        for &c in chain.iter().take(span as usize) {
            if check && claims[c as usize] > 1 {
                return true;
            }
            if !check {
                claims[c as usize] = claims[c as usize].saturating_add(1);
            }
        }
        false
    }

    fn claim_folder(&self, first: u32, count: u32, claims: &mut [u8], check: bool) -> bool {
        let Some(entries) = self.read_dir(first, count) else {
            return false;
        };
        let span = clusters_for(count as u64 * ENTRY_SIZE as u64, self.cluster_size);
        let mut shared = self.claim_chain(first, span, claims, check);
        for e in entries.iter().skip(2).filter(|e| e.is_file()) {
            if shared {
                break;
            }
            let need = clusters_for(e.length() as u64, self.cluster_size);
            shared = self.claim_chain(e.cluster(), need, claims, check);
        }
        shared
    }

    /// Whether every file of the folder reads.
    fn folder_reads(&self, first: u32, count: u32) -> bool {
        let Some(entries) = self.read_dir(first, count) else {
            return false;
        };
        entries.iter().skip(2).filter(|e| e.is_file()).all(|e| {
            let need = clusters_for(e.length() as u64, self.cluster_size) as usize;
            need == 0 || self.chain(e.cluster()).is_some_and(|c| c.len() >= need)
        })
    }

    /// The root's save folders, and apart those that don't read or share a
    /// cluster with another (deleting one would free the other's), which
    /// Sigil lists as corrupt.
    pub fn list(&self) -> Result<(Vec<Listed>, Vec<Listed>)> {
        let count = self.root_count();
        let root = self
            .read_dir(self.rootdir_cluster, count)
            .context("the card's root directory doesn't read")?;
        let mut claims = vec![0u8; self.alloc_end as usize];
        let span = clusters_for(count as u64 * ENTRY_SIZE as u64, self.cluster_size);
        self.claim_chain(self.rootdir_cluster, span, &mut claims, false);
        for e in root.iter().skip(2).filter(|e| e.is_dir()) {
            self.claim_folder(e.cluster(), e.length(), &mut claims, false);
        }
        let (mut saves, mut corrupt) = (Vec::new(), Vec::new());
        for e in root.iter().skip(2).filter(|e| e.is_dir()) {
            let listed = Listed {
                name: e.name().to_vec(),
                first_cluster: e.cluster(),
            };
            let plain = !listed.name.is_empty()
                && listed.name != b"."
                && listed.name != b".."
                && !listed.name.iter().any(|&b| b == b'/' || b == b'\\');
            if plain
                && self.folder_reads(e.cluster(), e.length())
                && !self.claim_folder(e.cluster(), e.length(), &mut claims, true)
            {
                saves.push(listed);
            } else {
                corrupt.push(listed);
            }
        }
        Ok((saves, corrupt))
    }

    /// The root entry pointing at the folder starting at `first`, and its index.
    fn root_entry_for(&self, first: u32) -> Option<(Entry, u32)> {
        let root = self.root()?;
        root.into_iter()
            .enumerate()
            .skip(2)
            .find(|(_, e)| e.is_dir() && e.cluster() == first)
            .map(|(i, e)| (e, i as u32))
    }

    /// Lift the folder starting at relative cluster `first` off the card.
    pub fn extract(&self, first: u32) -> Result<Folder> {
        let (entry, _) = self
            .root_entry_for(first)
            .context("no folder of the card's root starts there")?;
        let count = entry.length();
        let entries = (count >= 2)
            .then(|| self.read_dir(first, count))
            .flatten()
            .context("the save folder doesn't read")?;
        let mut files = Vec::new();
        for e in entries.iter().skip(2).filter(|e| e.exists()) {
            ensure!(e.is_file(), "the save folder holds a folder");
            let data = match e.length() {
                0 => Vec::new(),
                len => self
                    .read_chain(e.cluster(), len as usize)
                    .context("a file of the save doesn't read")?,
            };
            files.push((e.clone(), data));
        }
        Ok(Folder {
            entry,
            self_entry: entries[0].clone(),
            parent_entry: entries[1].clone(),
            files,
        })
    }

    fn entries_per_cluster(&self) -> u32 {
        (self.cluster_size / ENTRY_SIZE) as u32
    }

    /// Entry `index` of the directory whose chain is `chain`, marked changed.
    fn entry_mut(&mut self, chain: &[u32], index: u32) -> &mut [u8] {
        let per = self.entries_per_cluster();
        let at = (index % per) as usize * ENTRY_SIZE;
        &mut self.relative_mut(chain[(index / per) as usize])[at..at + ENTRY_SIZE]
    }

    fn free_clusters(&self) -> u32 {
        (0..self.alloc_end).filter(|&n| self.is_free(n)).count() as u32
    }

    /// The first root slot a deleted folder left free, else the end.
    fn slot_for_new(&self, root: &[Entry]) -> u32 {
        root.iter()
            .enumerate()
            .skip(2)
            .find(|(_, e)| !e.exists())
            .map_or(root.len() as u32, |(i, _)| i as u32)
    }

    /// Allocate the lowest free cluster at or after `*next`, linked after
    /// `after` when given.
    fn allocate(&mut self, next: &mut u32, after: Option<u32>) -> Result<u32> {
        while *next < self.alloc_end && !self.is_free(*next) {
            *next += 1;
        }
        ensure!(*next < self.alloc_end, "the memory card is full");
        let n = *next;
        *next += 1;
        self.set_fat(n, 0xFFFF_FFFF)?;
        if let Some(after) = after {
            self.set_fat(after, FAT_ALLOCATED | n)?;
        }
        Ok(n)
    }

    /// Grow `chain` by a cluster when `index` is the first entry of a new one.
    fn reserve_entry(&mut self, next: &mut u32, chain: &mut Vec<u32>, index: u32) -> Result<()> {
        let per = self.entries_per_cluster();
        if !index.is_multiple_of(per) || ((index / per) as usize) < chain.len() {
            return Ok(());
        }
        let n = self.allocate(next, chain.last().copied())?;
        self.relative_mut(n).fill(PAD);
        chain.push(n);
        Ok(())
    }

    /// Write `folder` into the root, taking the lowest free clusters in the
    /// order the PS2 writes them: the root's next cluster when it needs one,
    /// the folder's first, then per file its entry's cluster when needed and
    /// its data (`sigil_ps2_inject`). The card is unchanged on an error.
    pub fn inject(&mut self, folder: &Folder) -> Result<()> {
        let root = self
            .root()
            .context("the card's root directory doesn't read")?;
        let root_count = root.len() as u32;
        ensure!(root_count >= 2, "the card's root directory is damaged");
        ensure!(
            !root
                .iter()
                .skip(2)
                .any(|e| e.exists() && e.name_field() == folder.entry.name_field()),
            "the card already holds {}",
            String::from_utf8_lossy(folder.name())
        );
        let slot = self.slot_for_new(&root);
        let per = self.entries_per_cluster();
        let mut need = u32::from(slot == root_count && root_count.is_multiple_of(per));
        need += clusters_for(
            (folder.files.len() as u64 + 2) * ENTRY_SIZE as u64,
            self.cluster_size,
        );
        for (e, _) in &folder.files {
            need += clusters_for(e.length() as u64, self.cluster_size);
        }
        ensure!(need <= self.free_clusters(), "the memory card is full");

        let mut root_chain = self
            .chain(self.rootdir_cluster)
            .context("the card's root directory doesn't read")?;
        let mut next = 0;
        if slot == root_count {
            self.reserve_entry(&mut next, &mut root_chain, root_count)?;
        }
        let first = self.allocate(&mut next, None)?;
        self.relative_mut(first).fill(PAD);
        let mut dir_chain = vec![first];

        let file_count = folder.files.len() as u32;
        let e = self.entry_mut(&root_chain, slot);
        e.copy_from_slice(&folder.entry.0);
        set_le32(e, ENTRY_LENGTH, file_count + 2);
        set_le32(e, ENTRY_CLUSTER, first);
        set_le32(e, ENTRY_PARENT_INDEX, 0);
        let e = self.entry_mut(&dir_chain, 0);
        e.copy_from_slice(&folder.self_entry.0);
        set_le32(e, ENTRY_PARENT_INDEX, slot);
        self.entry_mut(&dir_chain, 1)
            .copy_from_slice(&folder.parent_entry.0);

        for (i, (entry, data)) in folder.files.iter().enumerate() {
            let index = i as u32 + 2;
            self.reserve_entry(&mut next, &mut dir_chain, index)?;
            let length = entry.length() as usize;
            let (mut start, mut prev) = (0, None);
            for (k, chunk) in data[..length].chunks(self.cluster_size).enumerate() {
                let n = self.allocate(&mut next, prev)?;
                if k == 0 {
                    start = n;
                }
                let c = self.relative_mut(n);
                c.fill(PAD);
                c[..chunk.len()].copy_from_slice(chunk);
                prev = Some(n);
            }
            let e = self.entry_mut(&dir_chain, index);
            e.copy_from_slice(&entry.0);
            set_le32(e, ENTRY_CLUSTER, if length > 0 { start } else { 0 });
        }

        let created = folder.self_entry.created();
        let root_self = self.entry_mut(&root_chain, 0);
        if slot == root_count {
            set_le32(root_self, ENTRY_LENGTH, root_count + 1);
        }
        let modified = Entry::from(root_self).modified();
        if tod_later(&created, &modified) {
            root_self[ENTRY_MODIFIED..ENTRY_MODIFIED + 8].copy_from_slice(&created);
        }
        Ok(())
    }

    /// Delete the folder starting at relative cluster `first`, freeing its
    /// clusters (`sigil_ps2_delete`).
    pub fn delete(&mut self, first: u32) -> Result<()> {
        let (_, index) = self
            .root_entry_for(first)
            .context("no folder of the card's root starts there")?;
        let folder = self.extract(first)?;
        let mut starts: Vec<u32> = folder
            .files
            .iter()
            .filter(|(e, _)| e.length() > 0)
            .map(|(e, _)| e.cluster())
            .collect();
        starts.push(first);
        for start in starts {
            for c in self.chain(start).unwrap_or_default() {
                self.set_fat(c, FAT_CHAIN_END)?;
            }
        }
        let root_chain = self.chain(self.rootdir_cluster).unwrap_or_default();
        if ((index / self.entries_per_cluster()) as usize) < root_chain.len() {
            let e = self.entry_mut(&root_chain, index);
            let mode = le16(e, ENTRY_MODE) & !MODE_EXISTS;
            set_le16(e, ENTRY_MODE, mode);
        }
        Ok(())
    }
}

/// The superblock page of an empty 8 MB card, over zeroed bytes.
fn superblock(sb: &mut [u8]) {
    sb[..MAGIC.len()].copy_from_slice(MAGIC);
    sb[MAGIC.len()..MAGIC.len() + 7].copy_from_slice(b"1.2.0.0");
    set_le16(sb, 0x28, PAGE_LEN as u16);
    set_le16(sb, 0x2A, PAGES_PER_CLUSTER as u16);
    set_le16(sb, 0x2C, PAGES_PER_BLOCK as u16);
    set_le16(sb, 0x2E, 0xFF00);
    set_le32(sb, 0x30, FORMAT_CLUSTERS);
    set_le32(sb, 0x34, FORMAT_ALLOC_OFFSET);
    set_le32(sb, 0x38, FORMAT_ALLOC_END);
    set_le32(sb, 0x3C, 0);
    set_le32(sb, 0x40, FORMAT_BACKUP1);
    set_le32(sb, 0x44, FORMAT_BACKUP2);
    set_le32(sb, 0x50, FORMAT_IFC);
    sb[0xD0..0x150].fill(0xFF);
    sb[0x150] = 2; // card type
    sb[0x151] = 0x2B; // card flags
}

fn write_dir_entry(e: &mut [u8], mode: u16, length: u32, cluster: u32, tod: &Tod, name: &[u8]) {
    e.fill(0);
    set_le16(e, ENTRY_MODE, mode);
    set_le32(e, ENTRY_LENGTH, length);
    e[ENTRY_CREATED..ENTRY_CREATED + 8].copy_from_slice(tod);
    set_le32(e, ENTRY_CLUSTER, cluster);
    e[ENTRY_MODIFIED..ENTRY_MODIFIED + 8].copy_from_slice(tod);
    e[ENTRY_NAME..ENTRY_NAME + name.len()].copy_from_slice(name);
}

/// Year, month, day, hour, minute, then second.
fn tod_later(a: &Tod, b: &Tod) -> bool {
    let key = |t: &Tod| [t[7], t[6], t[5], t[4], t[3], t[2], t[1]];
    key(a) > key(b)
}

/// Column parities of each byte value, as the PS2 ECC table has them: bits
/// 0-2 and 4-6 the even and odd halves of each bit pairing, bit 7 the byte's
/// own parity.
const COLUMN_BITS: [u8; 256] = {
    const GROUPS: [(u8, u8); 6] = [
        (0x55, 0),
        (0x33, 1),
        (0x0F, 2),
        (0xAA, 4),
        (0xCC, 5),
        (0xF0, 6),
    ];
    let mut table = [0u8; 256];
    let mut b = 0;
    while b < 256 {
        let mut out = 0u8;
        let mut g = 0;
        while g < GROUPS.len() {
            if (b as u8 & GROUPS[g].0).count_ones() % 2 == 1 {
                out |= 1 << GROUPS[g].1;
            }
            g += 1;
        }
        if (b as u8).count_ones() % 2 == 1 {
            out |= 0x80;
        }
        table[b] = out;
        b += 1;
    }
    table
};

/// The 3-byte ECC a card keeps for each 128-byte chunk of a page.
fn chunk_ecc(chunk: &[u8; 128]) -> [u8; 3] {
    let (mut column, mut line_even, mut line_odd) = (0u8, 0u8, 0u8);
    for (i, &b) in chunk.iter().enumerate() {
        let c = COLUMN_BITS[b as usize];
        column ^= c;
        if c & 0x80 != 0 {
            line_even ^= !(i as u8);
            line_odd ^= i as u8;
        }
    }
    [!column & 0x77, !line_even & 0x7F, !line_odd & 0x7F]
}

#[cfg(test)]
pub mod tests {
    use super::*;

    const TOD: Tod = [0, 1, 2, 3, 4, 5, 0xE9, 0x07];

    /// A save folder with these files, as a game would write one.
    pub fn folder(name: &str, files: &[(&str, Vec<u8>)]) -> Folder {
        let entry = |mode: u16, length: u32, name: &[u8]| {
            let mut e = [0u8; ENTRY_SIZE];
            write_dir_entry(&mut e, mode, length, 0, &TOD, name);
            Entry(e)
        };
        Folder {
            entry: entry(0x8427, 0, name.as_bytes()),
            self_entry: entry(0x8427, 0, b"."),
            parent_entry: entry(0xA426, 0, b".."),
            files: files
                .iter()
                .map(|(n, d)| (entry(0x8497, d.len() as u32, n.as_bytes()), d.clone()))
                .collect(),
        }
    }

    /// The folder as `extract` gives it back once on a card: the root entry
    /// carries its length and cluster, and so on.
    fn round_trip(card: &Card, name: &[u8]) -> Folder {
        let (saves, _) = card.list().unwrap();
        let listed = saves.iter().find(|s| s.name == name).unwrap();
        card.extract(listed.first_cluster).unwrap()
    }

    fn same_content(a: &Folder, b: &Folder) {
        assert_eq!(a.name(), b.name());
        assert_eq!(a.files.len(), b.files.len());
        for ((ea, da), (eb, db)) in a.files.iter().zip(&b.files) {
            assert_eq!(ea.name(), eb.name());
            assert_eq!(da, db);
        }
    }

    #[test]
    fn a_formatted_card_is_an_empty_ps2_card() {
        let card = Card::formatted(TOD);
        let file = card.to_file();
        assert_eq!(file.len(), 8_650_752);
        assert_eq!(&file[..MAGIC.len()], MAGIC);
        // The erased backup block keeps 0xFF data and spares.
        let erased = FORMAT_BACKUP2 as usize * PAGES_PER_BLOCK * (PAGE_LEN + SPARE_LEN);
        assert!(
            file[erased..erased + PAGE_LEN + SPARE_LEN]
                .iter()
                .all(|&b| b == 0xFF)
        );
        let loaded = Card::load(&file).unwrap();
        assert_eq!(loaded.list().unwrap(), (vec![], vec![]));
        assert_eq!(loaded.to_file(), file, "spares as read are written back");
        assert_eq!(loaded.free_clusters(), FORMAT_ALLOC_END - 1);
    }

    #[test]
    fn the_ecc_of_a_chunk() {
        // An all-zero chunk: every parity even.
        assert_eq!(chunk_ecc(&[0; 128]), [0x77, 0x7F, 0x7F]);
        let mut chunk = [0u8; 128];
        chunk[5] = 0x01;
        // Bit 0 of byte 5 flips masks 0x55, 0x33, 0x0F and the byte's own
        // parity: column bits 0-2, and the lines take !5 and 5.
        assert_eq!(chunk_ecc(&chunk), [0x70, 0x05, 0x7A]);
    }

    #[test]
    fn folders_round_trip_through_a_card() {
        let mut card = Card::formatted(TOD);
        let big: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let a = folder(
            "BASLUS-20152GAME",
            &[("icon.sys", vec![1; 964]), ("data", big)],
        );
        let b = folder("BESLES-12345SAVE", &[("empty", vec![])]);
        card.inject(&a).unwrap();
        card.inject(&b).unwrap();
        assert!(card.inject(&a).is_err(), "same name");

        let card = Card::load(&card.to_file()).unwrap();
        let (saves, corrupt) = card.list().unwrap();
        assert!(corrupt.is_empty());
        assert_eq!(saves.len(), 2);
        assert_eq!(saves[0].owner().as_deref(), Some("SLUS-20152"));
        same_content(&round_trip(&card, b"BASLUS-20152GAME"), &a);
        same_content(&round_trip(&card, b"BESLES-12345SAVE"), &b);
    }

    #[test]
    fn deleting_frees_the_folder_and_its_slot() {
        let mut card = Card::formatted(TOD);
        let a = folder("BASLUS-20152GAME", &[("data", vec![7; 3000])]);
        let b = folder("BASLUS-20153OTHER", &[("data", vec![8; 10])]);
        card.inject(&a).unwrap();
        card.inject(&b).unwrap();
        let free = card.free_clusters();
        let first = card.list().unwrap().0[0].first_cluster;
        card.delete(first).unwrap();
        assert_eq!(
            card.free_clusters(),
            free + 5,
            "two directory and three data clusters"
        );
        assert_eq!(card.list().unwrap().0.len(), 1);
        // A new folder takes the deleted one's root slot and lowest clusters.
        card.inject(&a).unwrap();
        let (saves, _) = card.list().unwrap();
        assert_eq!(saves[0].name, b"BASLUS-20152GAME");
        assert_eq!(saves[0].first_cluster, first);
    }

    #[test]
    fn a_full_card_is_refused_unchanged() {
        let mut card = Card::formatted(TOD);
        let huge = folder("BASLUS-20152GAME", &[("data", vec![0; 9 * 1024 * 1024])]);
        let before = card.to_file();
        assert!(card.inject(&huge).is_err());
        assert_eq!(card.to_file(), before);
    }

    #[test]
    fn owners_follow_the_product_code() {
        assert_eq!(owner(b"BASLUS-20152GAME").as_deref(), Some("SLUS-20152"));
        assert_eq!(owner(b"BISCPS-15001").as_deref(), Some("SCPS-15001"));
        assert_eq!(owner(b"BXSLUS-20152"), None);
        assert_eq!(owner(b"BASLUS-2015"), None);
        assert_eq!(owner(b"icon.sys"), None);
    }

    #[test]
    fn not_a_card() {
        assert!(Card::load(b"nope").is_none());
        let mut file = Card::formatted(TOD).to_file();
        file.pop();
        assert!(Card::load(&file).is_none(), "wrong size");
    }
}
