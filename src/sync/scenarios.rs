//! End-to-end sync scenarios against the fake RomM (`testing::fake_romm`).
//! Each test drives one or more simulated devices — a save directory and a
//! sync-state store each — through `sync_layout`, as launch and exit would.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;
use crate::emulator::{FileSet, GameSaveDir, SaveLayout, SaveSet};
use crate::testing::fake_romm::{FakeRomm, content_hash};
use crate::testing::{TempDir, TestEmulator, age, write, zip_bytes};

const ROM: i64 = 7;
const TITLE: &str = "0100000000010000";
const SWITCH: &[&str] = &["eden", "ryujinx"];
const EDEN: TestEmulator = TestEmulator { id: "eden", family: SWITCH };
const RYUJINX: TestEmulator = TestEmulator { id: "ryujinx", family: SWITCH };
const MGBA: TestEmulator = TestEmulator { id: "mgba", family: &[] };
const DOLPHIN: TestEmulator = TestEmulator { id: "dolphin", family: &[] };

struct Device {
    dir: TempDir,
    store: SyncStore,
}

impl Device {
    fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let store = SyncStore::load(dir.path().join("sync-state.json"));
        Self { dir, store }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    async fn sync(
        &self,
        fake: &FakeRomm,
        emulator: &dyn Emulator,
        layout: &SaveLayout,
        present_at_launch: Option<&HashSet<String>>,
    ) -> HashSet<String> {
        let romm = fake.client();
        let ctx = SyncCtx { romm: &romm, store: &self.store, rom_id: ROM, emulator };
        sync_layout(&ctx, layout, Path::new("Game.rom"), present_at_launch).await
    }

    /// Every file under this device's backup dir.
    fn backups(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![self.store.backups_dir()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                if e.path().is_dir() { stack.push(e.path()) } else { out.push(e.path()) }
            }
        }
        out
    }

    fn switch(&self) -> SaveLayout {
        SaveLayout {
            saves: Some(SaveSet::Dir(GameSaveDir {
                dir: self.path("save"),
                archive_root: TITLE.into(),
                extra_roots: vec![],
                mirrors: vec![],
                required_subdir: None,
            })),
            states: None,
        }
    }

    fn gba(&self) -> SaveLayout {
        SaveLayout {
            saves: FileSet::stem(self.path("saves"), "Game")
                .map(|s| SaveSet::Files(s.with_remote_extensions(&[("srm", "sav")]))),
            states: FileSet::stem(self.path("states"), "Game"),
        }
    }

    fn gamecube(&self) -> SaveLayout {
        SaveLayout { saves: Some(SaveSet::Bundle(FileSet::gci(self.path("card"), "GALE01"))), states: None }
    }
}

fn read(path: PathBuf) -> Vec<u8> {
    std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {}", path.display(), e))
}

fn zip_names(data: &[u8]) -> Vec<String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data)).unwrap();
    let mut names: Vec<String> = (0..zip.len()).map(|i| zip.by_index(i).unwrap().name().to_owned()).collect();
    names.sort();
    names
}

// ── Archive saves ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn archive_save_moves_between_devices_and_emulators() {
    let fake = FakeRomm::start().await;
    let (a, b) = (Device::new("a"), Device::new("b"));
    write(&a.path("save/Common.bin"), b"common v1");
    write(&a.path("save/slot/File1.bin"), b"file v1");

    a.sync(&fake, &EDEN, &a.switch(), None).await;
    let saves = fake.saves(ROM);
    assert_eq!(saves.len(), 1);
    let up = &saves[0];
    assert_eq!(up.slot.as_deref(), Some("autosave"));
    assert_eq!(up.emulator.as_deref(), Some("eden"));
    assert!(up.file_name.starts_with("Game [") && up.file_name.ends_with("].zip"), "{}", up.file_name);
    assert_eq!(zip_names(&up.data), [format!("{TITLE}/Common.bin"), format!("{TITLE}/slot/File1.bin")]);
    let local = save_archive::entries_hash(&save_archive::dir_entries(&a.path("save"), TITLE).unwrap()).unwrap();
    assert_eq!(local.as_deref(), Some(up.content_hash.as_str()), "local hash must follow RomM's scheme");

    // Another device, another emulator of the family, restores it.
    b.sync(&fake, &RYUJINX, &b.switch(), None).await;
    assert_eq!(read(b.path("save/slot/File1.bin")), b"file v1");

    // Nothing changed: no further uploads from either side.
    let writes = fake.writes().len();
    a.sync(&fake, &EDEN, &a.switch(), None).await;
    b.sync(&fake, &RYUJINX, &b.switch(), None).await;
    assert_eq!(fake.writes().len(), writes);

    // B plays on; A picks it up and keeps its old version as a backup.
    write(&b.path("save/Common.bin"), b"common v2");
    b.sync(&fake, &RYUJINX, &b.switch(), None).await;
    assert_eq!(fake.saves(ROM).len(), 2, "each upload is a new version");
    a.sync(&fake, &EDEN, &a.switch(), None).await;
    assert_eq!(read(a.path("save/Common.bin")), b"common v2");
    assert!(a.backups().iter().any(|p| p.ends_with("Common.bin") && read(p.clone()) == b"common v1"));
}

#[tokio::test]
async fn emulators_outside_the_family_ignore_the_save() {
    let fake = FakeRomm::start().await;
    fake.seed_save(ROM, "Game.zip", "ryujinx", Some("autosave"), &zip_bytes(&[(&format!("{TITLE}/a.bin"), b"a")]));
    let d = Device::new("other");
    d.sync(&fake, &DOLPHIN, &d.switch(), None).await;
    assert!(!d.path("save").exists());
}

#[tokio::test]
async fn history_keeps_the_last_ten_versions() {
    let fake = FakeRomm::start().await;
    let d = Device::new("hist");
    for v in 1..=12 {
        write(&d.path("save/data.bin"), format!("v{v}").as_bytes());
        d.sync(&fake, &EDEN, &d.switch(), None).await;
    }
    let saves = fake.saves(ROM);
    assert_eq!(saves.len(), 10);
    let newest = saves.last().unwrap();
    assert_eq!(newest.content_hash, content_hash(&zip_bytes(&[(&format!("{TITLE}/data.bin"), b"v12")])));
}

#[tokio::test]
async fn reverting_to_an_older_version_is_uploaded_not_undone() {
    let fake = FakeRomm::start().await;
    let d = Device::new("revert");
    for v in ["v1", "v2", "v1"] {
        write(&d.path("save/data.bin"), v.as_bytes());
        d.sync(&fake, &EDEN, &d.switch(), None).await;
    }
    assert_eq!(fake.saves(ROM).len(), 3, "the revert is a new version, not the old entry");
    d.sync(&fake, &EDEN, &d.switch(), None).await;
    assert_eq!(read(d.path("save/data.bin")), b"v1", "the next sync must not restore v2");
}

#[tokio::test]
async fn refuses_an_archive_of_another_game() {
    let fake = FakeRomm::start().await;
    fake.seed_save(ROM, "Game.zip", "eden", Some("autosave"), &zip_bytes(&[("0100AAAA00000000/a.bin", b"other game")]));
    let d = Device::new("foreign");
    let present = d.sync(&fake, &EDEN, &d.switch(), None).await;
    assert!(present.is_empty());
    assert!(save_archive::newest_mtime(&d.path("save")).is_none(), "nothing may be restored");
}

#[tokio::test]
async fn a_folder_without_its_required_data_is_not_a_save() {
    let fake = FakeRomm::start().await;
    let d = Device::new("wii");
    write(&d.path("title/content/title.tmd"), b"tmd");
    let layout = SaveLayout {
        saves: Some(SaveSet::Dir(GameSaveDir {
            dir: d.path("title"),
            archive_root: "524d3845".into(),
            extra_roots: vec![],
            mirrors: vec![],
            required_subdir: Some("data"),
        })),
        states: None,
    };
    d.sync(&fake, &DOLPHIN, &layout, None).await;
    assert!(fake.writes().is_empty());
    write(&d.path("title/data/save.bin"), b"progress");
    d.sync(&fake, &DOLPHIN, &layout, None).await;
    assert_eq!(zip_names(&fake.saves(ROM)[0].data), ["524d3845/content/title.tmd", "524d3845/data/save.bin"]);
}

// ── GameCube bundles ──────────────────────────────────────────────────────────

#[tokio::test]
async fn gamecube_bundle_holds_only_this_games_files() {
    let fake = FakeRomm::start().await;
    let (a, b) = (Device::new("gc-a"), Device::new("gc-b"));
    write(&a.path("card/01-GALE-SuperSmashBros.gci"), b"GALE01 melee");
    write(&a.path("card/01-GMSE-Sunshine.gci"), b"GMSE01 sunshine");
    a.sync(&fake, &DOLPHIN, &a.gamecube(), None).await;
    assert_eq!(zip_names(&fake.saves(ROM)[0].data), ["01-GALE-SuperSmashBros.gci"]);

    write(&b.path("card/01-GPOE-Other.gci"), b"GPOE01 other game");
    b.sync(&fake, &DOLPHIN, &b.gamecube(), None).await;
    assert_eq!(read(b.path("card/01-GALE-SuperSmashBros.gci")), b"GALE01 melee");
    assert_eq!(read(b.path("card/01-GPOE-Other.gci")), b"GPOE01 other game", "other games' saves stay");
}

#[tokio::test]
async fn refuses_a_bundle_with_another_games_save() {
    let fake = FakeRomm::start().await;
    fake.seed_save(ROM, "Game.zip", "dolphin", Some("autosave"), &zip_bytes(&[("01-GALE-renamed.gci", b"GMSE01 not melee")]));
    let d = Device::new("gc-foreign");
    d.sync(&fake, &DOLPHIN, &d.gamecube(), None).await;
    assert!(!d.path("card/01-GALE-renamed.gci").exists());
}

// ── Conflicts ─────────────────────────────────────────────────────────────────

/// Two devices that both changed the save since they last synced.
async fn diverged(fake: &FakeRomm) -> (Device, Device) {
    let (a, b) = (Device::new("ca"), Device::new("cb"));
    write(&a.path("save/data.bin"), b"base");
    a.sync(fake, &EDEN, &a.switch(), None).await;
    b.sync(fake, &EDEN, &b.switch(), None).await;
    write(&b.path("save/data.bin"), b"from b");
    b.sync(fake, &EDEN, &b.switch(), None).await;
    write(&a.path("save/data.bin"), b"from a");
    (a, b)
}

#[tokio::test]
async fn conflict_keeps_the_newer_local_save_and_the_romm_version() {
    let fake = FakeRomm::start().await;
    let (a, _b) = diverged(&fake).await;
    a.sync(&fake, &EDEN, &a.switch(), None).await;

    assert_eq!(read(a.path("save/data.bin")), b"from a");
    let newest = fake.saves(ROM).pop().unwrap();
    assert_eq!(newest.content_hash, content_hash(&zip_bytes(&[(&format!("{TITLE}/data.bin"), b"from a")])));
    let conflicts = a.store.conflicts().await;
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].kept, "local");
    let loser = PathBuf::from(conflicts[0].loser_backup.clone().unwrap());
    assert_eq!(zip_names(&read(loser)), [format!("{TITLE}/data.bin")]);
}

#[tokio::test]
async fn conflict_takes_a_newer_romm_save_and_backs_up_the_local_one() {
    let fake = FakeRomm::start().await;
    let (a, _b) = diverged(&fake).await;
    age(&a.path("save/data.bin"), Duration::from_secs(4 * 3600));
    a.sync(&fake, &EDEN, &a.switch(), None).await;

    assert_eq!(read(a.path("save/data.bin")), b"from b");
    assert_eq!(a.store.conflicts().await[0].kept, "remote");
    assert!(a.backups().iter().any(|p| read(p.clone()) == b"from a"));
}

#[tokio::test]
async fn a_save_created_this_session_never_wins_a_conflict() {
    let fake = FakeRomm::start().await;
    fake.seed_save(ROM, "Game.zip", "eden", Some("autosave"), &zip_bytes(&[(&format!("{TITLE}/data.bin"), b"real progress")]));
    let d = Device::new("fresh");

    // Pre-launch restore fails (offline), so the game starts a new save.
    fake.set_fail_listing(true);
    let present = d.sync(&fake, &EDEN, &d.switch(), None).await;
    assert!(present.is_empty());
    fake.set_fail_listing(false);
    write(&d.path("save/data.bin"), b"new game");

    d.sync(&fake, &EDEN, &d.switch(), Some(&present)).await;
    assert_eq!(read(d.path("save/data.bin")), b"real progress");
    assert_eq!(d.store.conflicts().await[0].kept, "remote");
    assert!(d.backups().iter().any(|p| read(p.clone()) == b"new game"));
}

#[tokio::test]
async fn a_listing_failure_changes_nothing_but_records_presence() {
    let fake = FakeRomm::start().await;
    let d = Device::new("offline");
    write(&d.path("save/data.bin"), b"local");
    fake.set_fail_listing(true);
    let present = d.sync(&fake, &EDEN, &d.switch(), None).await;
    assert_eq!(present.len(), 1, "an existing save must not look fresh after exit");
    assert!(fake.writes().is_empty());
}

// ── Single files ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn web_player_srm_restores_as_sav_and_comes_back() {
    let fake = FakeRomm::start().await;
    fake.seed_save(ROM, "Game [2026-05-05 13-07-23-697].srm", "mgba", None, b"browser sram");
    let d = Device::new("srm");
    d.sync(&fake, &MGBA, &d.gba(), None).await;
    assert_eq!(read(d.path("saves/Game.sav")), b"browser sram");
    assert!(!d.path("saves/Game.srm").exists());
    assert!(fake.writes().is_empty());

    write(&d.path("saves/Game.sav"), b"desktop sram");
    d.sync(&fake, &MGBA, &d.gba(), None).await;
    let newest = fake.saves(ROM).pop().unwrap();
    assert!(newest.file_name.starts_with("Game [") && newest.file_name.ends_with("].sav"), "{}", newest.file_name);
    assert_eq!(newest.slot.as_deref(), Some("autosave"));
    assert_eq!(newest.data, b"desktop sram");
}

#[tokio::test]
async fn a_single_file_updates_in_place_when_the_slot_holds_other_files() {
    let fake = FakeRomm::start().await;
    fake.seed_save(ROM, "Game.zip", "other", Some("autosave"), b"another emulator's archive");
    let d = Device::new("shared-slot");
    write(&d.path("saves/Game.sav"), b"v1");
    d.sync(&fake, &MGBA, &d.gba(), None).await;
    write(&d.path("saves/Game.sav"), b"v2");
    d.sync(&fake, &MGBA, &d.gba(), None).await;

    let writes = fake.writes();
    assert_eq!(writes[0], "POST save Game.sav slot=None");
    assert!(writes[1].starts_with("PUT save "), "{:?}", writes);
    assert_eq!(fake.saves(ROM).len(), 2, "the other file is never cleaned up");
}

#[tokio::test]
async fn several_files_of_one_game_never_share_a_slot_history() {
    let fake = FakeRomm::start().await;
    let d = Device::new("two-files");
    write(&d.path("saves/Game_1.mcd"), b"card 1");
    write(&d.path("saves/Game_2.mcd"), b"card 2");
    d.sync(&fake, &MGBA, &d.gba(), None).await;
    assert!(fake.saves(ROM).iter().all(|s| s.slot.is_none()), "{:?}", fake.writes());
}

#[tokio::test]
async fn romm_duplicates_resolve_to_the_newest() {
    let fake = FakeRomm::start().await;
    fake.seed_save(ROM, "Game [2026-01-01_00-00-01].sav", "mgba", None, b"old");
    fake.seed_save(ROM, "Game [2026-01-01_00-00-02].sav", "mgba", None, b"new");
    let d = Device::new("dupes");
    d.sync(&fake, &MGBA, &d.gba(), None).await;
    assert_eq!(read(d.path("saves/Game.sav")), b"new");
}

#[tokio::test]
async fn states_update_in_place() {
    let fake = FakeRomm::start().await;
    let d = Device::new("states");
    write(&d.path("states/Game.ss1"), b"state 1");
    d.sync(&fake, &MGBA, &d.gba(), None).await;
    write(&d.path("states/Game.ss1"), b"state 2");
    d.sync(&fake, &MGBA, &d.gba(), None).await;

    let states = fake.states(ROM);
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].data, b"state 2");
    assert!(fake.writes()[1].starts_with("PUT state "));
}

#[tokio::test]
async fn a_3ds_save_carries_title_data_and_extdata_separately() {
    let fake = FakeRomm::start().await;
    let (a, b) = (Device::new("3ds-a"), Device::new("3ds-b"));
    let layout = |d: &Device| SaveLayout {
        saves: Some(SaveSet::Dir(GameSaveDir {
            dir: d.path("title/data"),
            archive_root: "data".into(),
            extra_roots: vec![("extdata".into(), d.path("extdata/00001744"))],
            mirrors: vec![],
            required_subdir: None,
        })),
        states: None,
    };
    // Progress kept only in extdata still counts as a save.
    write(&a.path("extdata/00001744/user/progress.bin"), b"ext progress");
    a.sync(&fake, &EDEN, &layout(&a), None).await;
    assert_eq!(zip_names(&fake.saves(ROM)[0].data), ["extdata/user/progress.bin"]);

    write(&b.path("title/data/00000001/local.bin"), b"b's own title data");
    age(&b.path("title/data/00000001/local.bin"), Duration::from_secs(4 * 3600));
    b.sync(&fake, &EDEN, &layout(&b), None).await;
    assert_eq!(read(b.path("extdata/00001744/user/progress.bin")), b"ext progress");
    assert_eq!(read(b.path("title/data/00000001/local.bin")), b"b's own title data", "a root the zip lacks stays");
}
