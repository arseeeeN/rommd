//! End-to-end sync scenarios against the fake RomM (`testing::fake_romm`).
//! Each test drives one or more simulated devices — a save directory and a
//! sync-state store each — through `sync_layout`, as launch and exit would.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::*;
use crate::emulator::{CreateDir, FileSet, GameSaveDir, N64Files, Ps1Cards, SaveLayout, SaveSet};
use crate::testing::fake_romm::{FakeRomm, content_hash};
use crate::testing::{TempDir, TestEmulator, age, write, zip_bytes};

const ROM: i64 = 7;
const TITLE: &str = "0100000000010000";
const SWITCH: &[&str] = &["eden", "ryujinx"];
const EDEN: TestEmulator = TestEmulator {
    id: "eden",
    family: SWITCH,
};
const RYUJINX: TestEmulator = TestEmulator {
    id: "ryujinx",
    family: SWITCH,
};
const MGBA: TestEmulator = TestEmulator {
    id: "mgba",
    family: &[],
};
const DOLPHIN: TestEmulator = TestEmulator {
    id: "dolphin",
    family: &[],
};
const DUCKSTATION: TestEmulator = TestEmulator {
    id: "duckstation",
    family: &["swanstation"],
};
const RMG: TestEmulator = TestEmulator {
    id: "rmg",
    family: crate::n64::SAVE_COMPATIBLE_EMULATORS,
};
const GOPHER64: TestEmulator = TestEmulator {
    id: "gopher64",
    family: crate::n64::SAVE_COMPATIBLE_EMULATORS,
};

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
        let ctx = SyncCtx {
            romm: &romm,
            store: &self.store,
            rom_id: ROM,
            emulator,
        };
        sync_layout(&ctx, layout, Path::new("Game.rom"), present_at_launch).await
    }

    /// Every file under this device's backup dir.
    fn backups(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![self.store.backups_dir()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                if e.path().is_dir() {
                    stack.push(e.path())
                } else {
                    out.push(e.path())
                }
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
                alternatives: vec![],
                ignored: vec![],
                create: None,
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

    /// An N64 game's files, `<name>-12345678.<ext>` in `n64/`.
    fn n64(&self, word_swapped: bool, new_stem: Option<&str>) -> SaveLayout {
        SaveLayout {
            saves: FileSet::tagged(self.path("n64"), "-12345678", &["eep", "sra", "fla", "mpk"])
                .map(|set| {
                    SaveSet::N64(N64Files {
                        set,
                        new_stem: new_stem.map(str::to_owned),
                        word_swapped,
                    })
                }),
            states: None,
        }
    }

    fn gamecube(&self) -> SaveLayout {
        SaveLayout {
            saves: Some(SaveSet::Bundle(FileSet::gci(self.path("card"), "GALE01"))),
            states: None,
        }
    }
}

fn read(path: PathBuf) -> Vec<u8> {
    std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {}", path.display(), e))
}

fn zip_names(data: &[u8]) -> Vec<String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data)).unwrap();
    let mut names: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().name().to_owned())
        .collect();
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
    assert!(
        up.file_name.starts_with("Game [") && up.file_name.ends_with("].zip"),
        "{}",
        up.file_name
    );
    assert_eq!(
        zip_names(&up.data),
        [
            format!("{TITLE}/Common.bin"),
            format!("{TITLE}/slot/File1.bin")
        ]
    );
    let local =
        save_archive::entries_hash(&save_archive::dir_entries(&a.path("save"), TITLE).unwrap())
            .unwrap();
    assert_eq!(
        local.as_deref(),
        Some(up.content_hash.as_str()),
        "local hash must follow RomM's scheme"
    );

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
    assert!(
        a.backups()
            .iter()
            .any(|p| p.ends_with("Common.bin") && read(p.clone()) == b"common v1")
    );
}

#[tokio::test]
async fn emulators_outside_the_family_ignore_the_save() {
    let fake = FakeRomm::start().await;
    fake.seed_save(
        ROM,
        "Game.zip",
        "ryujinx",
        Some("autosave"),
        &zip_bytes(&[(&format!("{TITLE}/a.bin"), b"a")]),
    );
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
    assert_eq!(
        newest.content_hash,
        content_hash(&zip_bytes(&[(&format!("{TITLE}/data.bin"), b"v12")]))
    );
}

#[tokio::test]
async fn reverting_to_an_older_version_is_uploaded_not_undone() {
    let fake = FakeRomm::start().await;
    let d = Device::new("revert");
    for v in ["v1", "v2", "v1"] {
        write(&d.path("save/data.bin"), v.as_bytes());
        d.sync(&fake, &EDEN, &d.switch(), None).await;
    }
    assert_eq!(
        fake.saves(ROM).len(),
        3,
        "the revert is a new version, not the old entry"
    );
    d.sync(&fake, &EDEN, &d.switch(), None).await;
    assert_eq!(
        read(d.path("save/data.bin")),
        b"v1",
        "the next sync must not restore v2"
    );
}

#[tokio::test]
async fn refuses_an_archive_of_another_game() {
    let fake = FakeRomm::start().await;
    fake.seed_save(
        ROM,
        "Game.zip",
        "eden",
        Some("autosave"),
        &zip_bytes(&[("0100AAAA00000000/a.bin", b"other game")]),
    );
    let d = Device::new("foreign");
    let present = d.sync(&fake, &EDEN, &d.switch(), None).await;
    assert!(present.is_empty());
    assert!(
        save_archive::newest_mtime(&d.path("save")).is_none(),
        "nothing may be restored"
    );
}

fn wii(d: &Device) -> SaveLayout {
    SaveLayout {
        saves: Some(SaveSet::Dir(GameSaveDir {
            dir: d.path("title/data"),
            archive_root: "524d3845/data".into(),
            extra_roots: vec![],
            mirrors: vec![],
            alternatives: vec![],
            ignored: vec!["524d3845/content".into()],
            create: None,
        })),
        states: None,
    }
}

#[tokio::test]
async fn a_wii_save_is_its_data_folder_whichever_shape_romm_holds() {
    let fake = FakeRomm::start().await;
    // argosy's older upload carries the title's content/ too.
    fake.seed_save(
        ROM,
        "Game.zip",
        "dolphin",
        Some("autosave"),
        &zip_bytes(&[
            ("524d3845/content/title.tmd", b"their tmd"),
            ("524d3845/data/save.bin", b"progress"),
        ]),
    );
    let d = Device::new("wii");
    write(&d.path("title/content/title.tmd"), b"our tmd");
    d.sync(&fake, &DOLPHIN, &wii(&d), None).await;
    assert_eq!(read(d.path("title/data/save.bin")), b"progress");
    assert_eq!(read(d.path("title/content/title.tmd")), b"our tmd");

    write(&d.path("title/data/save.bin"), b"more progress");
    d.sync(&fake, &DOLPHIN, &wii(&d), None).await;
    assert_eq!(
        zip_names(&fake.saves(ROM).pop().unwrap().data),
        ["524d3845/data/save.bin"],
        "Sigil's unit: no content/"
    );
}

#[tokio::test]
async fn a_switch_device_save_restores_from_sigils_device_area() {
    let fake = FakeRomm::start().await;
    fake.seed_save(
        ROM,
        "Game.zip",
        "eden",
        Some("autosave"),
        &zip_bytes(&[
            (&format!("{TITLE}/account.bin"), b"account"),
            (&format!("device/{TITLE}/island.bin"), b"island"),
        ]),
    );
    let layout = |d: &Device, device: bool| {
        let (alternatives, ignored) = crate::switch::sigil_areas(TITLE, device);
        SaveLayout {
            saves: Some(SaveSet::Dir(GameSaveDir {
                dir: d.path("save"),
                archive_root: TITLE.into(),
                extra_roots: vec![],
                mirrors: vec![],
                alternatives,
                ignored,
                create: None,
            })),
            states: None,
        }
    };
    let device = Device::new("switch-device");
    device
        .sync(&fake, &EDEN, &layout(&device, true), None)
        .await;
    assert_eq!(read(device.path("save/island.bin")), b"island");
    assert!(!device.path("save/account.bin").exists());

    let account = Device::new("switch-account");
    account
        .sync(&fake, &RYUJINX, &layout(&account, false), None)
        .await;
    assert_eq!(read(account.path("save/account.bin")), b"account");
    assert!(!account.path("save/island.bin").exists());
}

/// A Ryujinx-like layout whose save folder has to be created by a hook, which
/// counts its calls and fails when `refuse` is set.
fn slot_layout(d: &Device, calls: &Arc<AtomicUsize>, refuse: bool) -> SaveLayout {
    let (slot, calls) = (d.path("slot"), calls.clone());
    let create = CreateDir(Arc::new(move || {
        calls.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(!refuse, "Ryujinx is running");
        std::fs::create_dir_all(slot.join("0"))?;
        std::fs::create_dir_all(slot.join("1"))?;
        Ok(())
    }));
    SaveLayout {
        saves: Some(SaveSet::Dir(GameSaveDir {
            dir: d.path("slot/0"),
            archive_root: TITLE.into(),
            extra_roots: vec![],
            mirrors: vec![d.path("slot/1")],
            alternatives: vec![],
            ignored: vec![],
            create: Some(create),
        })),
        states: None,
    }
}

#[tokio::test]
async fn a_missing_save_folder_is_created_only_to_restore_into() {
    let fake = FakeRomm::start().await;
    let calls = Arc::new(AtomicUsize::new(0));

    let fresh = Device::new("slot-none");
    fresh
        .sync(&fake, &RYUJINX, &slot_layout(&fresh, &calls, false), None)
        .await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "nothing to restore, nothing created"
    );
    assert!(!fresh.path("slot").exists());

    let eden = Device::new("slot-eden");
    write(&eden.path("save/File1.bin"), b"from eden");
    eden.sync(&fake, &EDEN, &eden.switch(), None).await;

    let refused = Device::new("slot-refused");
    let present = refused
        .sync(&fake, &RYUJINX, &slot_layout(&refused, &calls, true), None)
        .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(present.is_empty());
    assert!(
        !refused.path("slot").exists(),
        "a refused slot leaves nothing behind"
    );

    let ryujinx = Device::new("slot-ryujinx");
    let layout = slot_layout(&ryujinx, &calls, false);
    let present = ryujinx.sync(&fake, &RYUJINX, &layout, None).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        present.len(),
        1,
        "the restored save counts as present at launch"
    );
    assert_eq!(read(ryujinx.path("slot/0/File1.bin")), b"from eden");
    assert_eq!(read(ryujinx.path("slot/1/File1.bin")), b"from eden");

    ryujinx.sync(&fake, &RYUJINX, &layout, Some(&present)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "an existing folder is not created again"
    );
}

// ── GameCube bundles ──────────────────────────────────────────────────────────

/// A `.gci`: game and maker code, then the save's file name at 0x08.
fn gci(id: &str, file_name: &str, body: &[u8]) -> Vec<u8> {
    let mut data = vec![0u8; 0x40];
    data[..6].copy_from_slice(id.as_bytes());
    data[0x08..0x08 + file_name.len()].copy_from_slice(file_name.as_bytes());
    data.extend_from_slice(body);
    data
}

#[tokio::test]
async fn gamecube_bundle_holds_only_this_games_files() {
    let fake = FakeRomm::start().await;
    let (a, b) = (Device::new("gc-a"), Device::new("gc-b"));
    let melee = gci("GALE01", "SuperSmashBros", b"melee");
    write(&a.path("card/01-GALE-SuperSmashBros.gci"), &melee);
    write(
        &a.path("card/01-GMSE-Sunshine.gci"),
        &gci("GMSE01", "Sunshine", b"s"),
    );

    // A game's only save travels bare, as argosy uploads it.
    a.sync(&fake, &DOLPHIN, &a.gamecube(), None).await;
    let up = fake.saves(ROM).pop().unwrap();
    assert!(up.file_name.ends_with("].gci"), "{}", up.file_name);
    assert_eq!(up.data, melee);
    assert_eq!(up.content_hash, content_hash(&melee));

    let other = gci("GPOE01", "Other", b"other game");
    write(&b.path("card/01-GPOE-Other.gci"), &other);
    b.sync(&fake, &DOLPHIN, &b.gamecube(), None).await;
    assert_eq!(read(b.path("card/01-GALE-SuperSmashBros.gci")), melee);
    assert_eq!(
        read(b.path("card/01-GPOE-Other.gci")),
        other,
        "other games' saves stay"
    );

    // Two saves of the game go up as a zip of both.
    write(
        &b.path("card/8P-GALE-Replay.gci"),
        &gci("GALE8P", "Replay", b"r"),
    );
    b.sync(&fake, &DOLPHIN, &b.gamecube(), None).await;
    assert_eq!(
        zip_names(&fake.saves(ROM).pop().unwrap().data),
        ["01-GALE-SuperSmashBros.gci", "8P-GALE-Replay.gci"]
    );
    a.sync(&fake, &DOLPHIN, &a.gamecube(), None).await;
    assert!(a.path("card/8P-GALE-Replay.gci").is_file());
}

#[tokio::test]
async fn refuses_a_bundle_with_another_games_save() {
    let fake = FakeRomm::start().await;
    fake.seed_save(
        ROM,
        "Game.zip",
        "dolphin",
        Some("autosave"),
        &zip_bytes(&[("01-GALE-renamed.gci", &gci("GMSE01", "Sunshine", b"s"))]),
    );
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
    assert_eq!(
        newest.content_hash,
        content_hash(&zip_bytes(&[(&format!("{TITLE}/data.bin"), b"from a")]))
    );
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
    fake.seed_save(
        ROM,
        "Game.zip",
        "eden",
        Some("autosave"),
        &zip_bytes(&[(&format!("{TITLE}/data.bin"), b"real progress")]),
    );
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
    assert_eq!(
        present.len(),
        1,
        "an existing save must not look fresh after exit"
    );
    assert!(fake.writes().is_empty());
}

// ── Single files ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn web_player_srm_restores_as_sav_and_comes_back() {
    let fake = FakeRomm::start().await;
    fake.seed_save(
        ROM,
        "Game [2026-05-05 13-07-23-697].srm",
        "mgba",
        None,
        b"browser sram",
    );
    let d = Device::new("srm");
    d.sync(&fake, &MGBA, &d.gba(), None).await;
    assert_eq!(read(d.path("saves/Game.sav")), b"browser sram");
    assert!(!d.path("saves/Game.srm").exists());
    assert!(fake.writes().is_empty());

    write(&d.path("saves/Game.sav"), b"desktop sram");
    d.sync(&fake, &MGBA, &d.gba(), None).await;
    let newest = fake.saves(ROM).pop().unwrap();
    assert!(
        newest.file_name.starts_with("Game [") && newest.file_name.ends_with("].sav"),
        "{}",
        newest.file_name
    );
    assert_eq!(newest.slot.as_deref(), Some("autosave"));
    assert_eq!(newest.data, b"desktop sram");
}

#[tokio::test]
async fn a_single_file_updates_in_place_when_the_slot_holds_other_files() {
    let fake = FakeRomm::start().await;
    fake.seed_save(
        ROM,
        "Game.zip",
        "other",
        Some("autosave"),
        b"another emulator's archive",
    );
    let d = Device::new("shared-slot");
    write(&d.path("saves/Game.sav"), b"v1");
    d.sync(&fake, &MGBA, &d.gba(), None).await;
    write(&d.path("saves/Game.sav"), b"v2");
    d.sync(&fake, &MGBA, &d.gba(), None).await;

    let writes = fake.writes();
    assert_eq!(writes[0], "POST save Game.sav slot=None");
    assert!(writes[1].starts_with("PUT save "), "{:?}", writes);
    assert_eq!(
        fake.saves(ROM).len(),
        2,
        "the other file is never cleaned up"
    );
}

#[tokio::test]
async fn several_files_of_one_game_never_share_a_slot_history() {
    let fake = FakeRomm::start().await;
    let d = Device::new("two-files");
    write(&d.path("saves/Game_1.mcd"), b"card 1");
    write(&d.path("saves/Game_2.mcd"), b"card 2");
    d.sync(&fake, &MGBA, &d.gba(), None).await;
    assert!(
        fake.saves(ROM).iter().all(|s| s.slot.is_none()),
        "{:?}",
        fake.writes()
    );
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
            alternatives: vec![],
            ignored: vec![],
            create: None,
        })),
        states: None,
    };
    // Progress kept only in extdata still counts as a save.
    write(
        &a.path("extdata/00001744/user/progress.bin"),
        b"ext progress",
    );
    a.sync(&fake, &EDEN, &layout(&a), None).await;
    assert_eq!(
        zip_names(&fake.saves(ROM)[0].data),
        ["extdata/user/progress.bin"]
    );

    write(
        &b.path("title/data/00000001/local.bin"),
        b"b's own title data",
    );
    age(
        &b.path("title/data/00000001/local.bin"),
        Duration::from_secs(4 * 3600),
    );
    b.sync(&fake, &EDEN, &layout(&b), None).await;
    assert_eq!(
        read(b.path("extdata/00001744/user/progress.bin")),
        b"ext progress"
    );
    assert_eq!(
        read(b.path("title/data/00000001/local.bin")),
        b"b's own title data",
        "a root the zip lacks stays"
    );
}

#[tokio::test]
async fn a_3ds_save_restores_from_sigils_unit() {
    let fake = FakeRomm::start().await;
    fake.seed_save(
        ROM,
        "Game.zip",
        "eden",
        Some("autosave"),
        &zip_bytes(&[
            ("00040000/00174400/data/00000001/save.bin", b"save"),
            ("00040000/00174400/extdata/user/progress.bin", b"ext"),
        ]),
    );
    let d = Device::new("3ds-sigil");
    let layout = SaveLayout {
        saves: Some(SaveSet::Dir(GameSaveDir {
            dir: d.path("title/data"),
            archive_root: "data".into(),
            extra_roots: vec![("extdata".into(), d.path("extdata/00001744"))],
            mirrors: vec![],
            alternatives: vec![
                ("data".into(), "00040000/00174400/data".into()),
                ("extdata".into(), "00040000/00174400/extdata".into()),
            ],
            ignored: vec![],
            create: None,
        })),
        states: None,
    };
    d.sync(&fake, &EDEN, &layout, None).await;
    assert_eq!(read(d.path("title/data/00000001/save.bin")), b"save");
    assert_eq!(read(d.path("extdata/00001744/user/progress.bin")), b"ext");
}

/// RomM answers uploads with `updated_at` in UTC and lists it in its own time
/// zone (the fake: +01:00). The same instant written differently is no change:
/// an untouched state is left alone, and one changed only here is uploaded
/// without a conflict.
#[tokio::test]
async fn states_are_unchanged_when_romm_writes_the_time_differently() {
    let fake = FakeRomm::start().await;
    let d = Device::new("states-tz");
    write(&d.path("states/Game.ss1"), b"state 1");
    write(&d.path("states/Game.ss2"), b"untouched");
    d.sync(&fake, &MGBA, &d.gba(), None).await;
    write(&d.path("states/Game.ss1"), b"state 1, later");
    d.sync(&fake, &MGBA, &d.gba(), None).await;

    let conflicts = d.store.conflicts().await;
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(d.backups(), Vec::<PathBuf>::new(), "nothing was downloaded");
    let writes = fake.writes();
    assert_eq!(writes.len(), 3, "{writes:?}");
    assert!(writes[2].starts_with("PUT state "), "{writes:?}");
}

#[tokio::test]
async fn states_without_history_are_compared_by_content() {
    // RomM has no content hash for states, so a device meeting RomM's states
    // for the first time must compare the bytes instead of calling a conflict.
    let fake = FakeRomm::start().await;
    let (a, b) = (Device::new("st-a"), Device::new("st-b"));
    write(&a.path("states/Game.ss1"), b"same state");
    write(&a.path("states/Game.ss2"), b"a's state");
    a.sync(&fake, &MGBA, &a.gba(), None).await;

    write(&b.path("states/Game.ss1"), b"same state");
    write(&b.path("states/Game.ss2"), b"b's state");
    b.sync(&fake, &MGBA, &b.gba(), None).await;
    let conflicts = b.store.conflicts().await;
    assert_eq!(
        conflicts.len(),
        1,
        "only the state that differs: {:?}",
        conflicts
    );
    assert!(conflicts[0].local_path.ends_with("Game.ss2"));
}

#[tokio::test]
async fn empty_folders_in_a_save_survive_the_round_trip() {
    // A 3DS game formats its save archive (an empty 00000001/ next to its
    // metadata) before writing anything to it.
    let fake = FakeRomm::start().await;
    let (a, b) = (Device::new("empty-a"), Device::new("empty-b"));
    write(&a.path("save/00000001.metadata"), b"format info");
    std::fs::create_dir_all(a.path("save/00000001")).unwrap();
    a.sync(&fake, &EDEN, &a.switch(), None).await;
    let hash_without_dirs = content_hash(&zip_bytes(&[(
        &format!("{TITLE}/00000001.metadata"),
        b"format info",
    )]));
    assert_eq!(
        fake.saves(ROM)[0].content_hash,
        hash_without_dirs,
        "folders don't change the hash"
    );

    b.sync(&fake, &EDEN, &b.switch(), None).await;
    assert!(b.path("save/00000001").is_dir());
    assert_eq!(read(b.path("save/00000001.metadata")), b"format info");
}

// ── N64 ───────────────────────────────────────────────────────────────────────

/// 32 KiB of SRAM in bus order, and as mupen64plus stores it.
fn n64_sram(fill: u8) -> (Vec<u8>, Vec<u8>) {
    let mut bus = vec![0xFF; 0x8000];
    bus[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, fill]);
    let mut swapped = bus.clone();
    swapped
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .for_each(|w| w.reverse());
    (bus, swapped)
}

#[tokio::test]
async fn n64_saves_move_between_rmg_and_gopher64() {
    let fake = FakeRomm::start().await;
    let (a, b) = (Device::new("n64-rmg"), Device::new("n64-gopher64"));
    let (bus, swapped) = n64_sram(1);
    write(&a.path("n64/GOOD NAME-12345678.sra"), &swapped);

    a.sync(&fake, &RMG, &a.n64(true, None), None).await;
    let up = fake.saves(ROM).pop().unwrap();
    assert_eq!(up.slot.as_deref(), Some("autosave"));
    assert!(up.file_name.ends_with("].srm"), "{}", up.file_name);
    assert_eq!(up.data.len(), 0x48800, "a libretro .srm");
    assert_eq!(&up.data[0x20800..0x28800], &swapped[..]);

    b.sync(&fake, &GOPHER64, &b.n64(false, Some("GAME-12345678")), None)
        .await;
    assert_eq!(read(b.path("n64/GAME-12345678.sra")), bus);

    let writes = fake.writes().len();
    a.sync(&fake, &RMG, &a.n64(true, None), None).await;
    b.sync(&fake, &GOPHER64, &b.n64(false, Some("GAME-12345678")), None)
        .await;
    assert_eq!(fake.writes().len(), writes, "the same save on both sides");

    let (bus2, _) = n64_sram(2);
    write(&b.path("n64/GAME-12345678.sra"), &bus2);
    b.sync(&fake, &GOPHER64, &b.n64(false, Some("GAME-12345678")), None)
        .await;
    a.sync(&fake, &RMG, &a.n64(true, None), None).await;
    let (_, swapped2) = n64_sram(2);
    assert_eq!(read(a.path("n64/GOOD NAME-12345678.sra")), swapped2);
}

#[tokio::test]
async fn n64_restores_sigils_unit() {
    let fake = FakeRomm::start().await;
    let (bus, _) = n64_sram(1);
    fake.seed_save(
        ROM,
        "Game.zip",
        "mupen64plus_fz",
        Some("autosave"),
        &zip_bytes(&[("sram", &bus)]),
    );
    let d = Device::new("n64-sigil");
    d.sync(&fake, &GOPHER64, &d.n64(false, Some("GAME-12345678")), None)
        .await;
    assert_eq!(read(d.path("n64/GAME-12345678.sra")), bus);
    d.sync(&fake, &GOPHER64, &d.n64(false, Some("GAME-12345678")), None)
        .await;
    assert!(fake.writes().is_empty(), "the base covers the unit's shape");
}

#[tokio::test]
async fn n64_restores_the_web_players_srm() {
    let fake = FakeRomm::start().await;
    let (bus, swapped) = n64_sram(1);
    let mut srm = vec![0xFF; 0x48800];
    srm[0x20800..0x28800].copy_from_slice(&swapped);
    fake.seed_save(
        ROM,
        "Game [2026-05-05 13-07-23-697].srm",
        "mupen64plus_next",
        None,
        &srm,
    );
    let d = Device::new("n64-srm");
    d.sync(&fake, &GOPHER64, &d.n64(false, Some("GAME-12345678")), None)
        .await;
    assert_eq!(read(d.path("n64/GAME-12345678.sra")), bus);
    d.sync(&fake, &GOPHER64, &d.n64(false, Some("GAME-12345678")), None)
        .await;
    assert!(fake.writes().is_empty());
}

#[tokio::test]
async fn an_n64_save_rommd_cant_name_is_restored_after_the_first_session() {
    let fake = FakeRomm::start().await;
    let (_, swapped) = n64_sram(1);
    let a = Device::new("n64-first");
    write(&a.path("n64/GOOD NAME-12345678.sra"), &swapped);
    a.sync(&fake, &RMG, &a.n64(true, None), None).await;

    // No database to spell the name: nothing to restore into before launch.
    let d = Device::new("n64-unnamed");
    let present = d.sync(&fake, &RMG, &d.n64(true, None), None).await;
    assert!(present.is_empty());
    assert!(!d.path("n64").exists());

    // The game saves under its own name; RomM's save wins over the new one.
    let (_, fresh) = n64_sram(9);
    write(&d.path("n64/GOOD NAME-12345678.sra"), &fresh);
    d.sync(&fake, &RMG, &d.n64(true, None), Some(&present))
        .await;
    assert_eq!(read(d.path("n64/GOOD NAME-12345678.sra")), swapped);
    assert_eq!(d.store.conflicts().await[0].kept, "remote");
}

// ── PS1 ───────────────────────────────────────────────────────────────────────

fn ps1(d: &Device) -> SaveLayout {
    SaveLayout {
        saves: Some(SaveSet::Ps1Cards(Ps1Cards {
            cards: [d.path("memcards/Game_1.mcd"), d.path("memcards/Game_2.mcd")],
        })),
        states: None,
    }
}

#[tokio::test]
async fn ps1_saves_travel_as_one_card_both_ways() {
    use crate::ps1_card::tests::{card, save};
    let fake = FakeRomm::start().await;
    let (a, b) = (Device::new("ps1-a"), Device::new("ps1-b"));
    let (one, two) = (save("BASLUS-00594AAA", 1, 1), save("BASLUS-00594BBB", 2, 2));
    write(
        &a.path("memcards/Game_1.mcd"),
        &card(std::slice::from_ref(&one)),
    );
    write(
        &a.path("memcards/Game_2.mcd"),
        &card(std::slice::from_ref(&two)),
    );

    a.sync(&fake, &DUCKSTATION, &ps1(&a), None).await;
    let up = fake.saves(ROM).pop().unwrap();
    assert!(up.file_name.ends_with("].mcd"), "{}", up.file_name);
    assert_eq!(up.data, card(&[one.clone(), two.clone()]), "Sigil's unit");
    assert_eq!(up.content_hash, content_hash(&up.data));

    // A libretro core's raw card (argosy, RomM's web player) restores too.
    let three = save("BASLUS-00594CCC", 1, 3);
    fake.seed_save(
        ROM,
        "Game.srm",
        "swanstation",
        None,
        &card(std::slice::from_ref(&three)),
    );
    b.sync(&fake, &DUCKSTATION, &ps1(&b), None).await;
    assert_eq!(read(b.path("memcards/Game_1.mcd")), card(&[three]));
    assert!(!b.path("memcards/Game_2.mcd").exists());
    let writes = fake.writes().len();
    b.sync(&fake, &DUCKSTATION, &ps1(&b), None).await;
    assert_eq!(fake.writes().len(), writes, "in sync after the restore");
}
