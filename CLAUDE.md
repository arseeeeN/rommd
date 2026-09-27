# rommd — Claude Code context

rommd is a local HTTP daemon (port 7497) that bridges RomM (a self-hosted game
library manager) with local emulators. It handles ROM download, emulator launch,
and bidirectional save/state sync with RomM.

## Architecture

```
RomM (remote) ←→ rommd daemon ←→ local emulators
```

- **`src/main.rs`** — startup, emulator registry, config loading
- **`src/server/`** — axum HTTP handlers, OpenAPI spec (utoipa), request/response types
- **`src/process.rs`** — process tracking, post-exit sync trigger
- **`src/emulator/`** — one file per emulator, all implement the `Emulator` trait
- **`src/romm/`** — RomM HTTP client (ROM download, save/state upload/download)
- **`src/config.rs`** — config file parsing, controller registry
- **`src/switch.rs`** — Switch NSP title ID extraction (ticket + NCA header decryption)
- **`src/sync/`** — save/state sync engine, three-way decision, persistent sync state
- **`src/save_archive.rs`** — zip/restore and RomM-compatible hashing of directory saves
- **`src/profile.rs`** — portable settings overlay (ProfileBlob)
- **`src/ini.rs`** — INI file read/write used by DuckStation and Dolphin

## Emulator trait

Every emulator implements `src/emulator/mod.rs::Emulator`. Key methods:

- `saves_dir` / `states_dir` — async; `states_dir` returns `Option<PathBuf>` where
  `None` means the emulator has no save states (Ryujinx, Eden)
- `game_saves_dir` — async; returns `Some(GameSaveDir)` for emulators whose
  per-game save is a directory (Switch), `None` to use flat-file sync of `saves_dir`
- `save_family` — emulator IDs whose RomM saves this emulator can restore
  (Switch emulators share `switch::SAVE_COMPATIBLE_EMULATORS`)
- `game_id_for_rom` — sync; returns the emulator's stable game identifier.
  Used for save scoping. Must not use the ROM filename as a source — filenames
  can be wrong and would silently sync saves for the wrong game
- `save_filter` — filename prefix used to scope flat-file save sync. Ryujinx
  returns an empty `SaveFilter` because it uses `game_saves_dir` instead
- `snapshot` / `launch` — profile overlay and process spawn

Adding a new emulator: create `src/emulator/<name>.rs`, implement the trait,
add `pub mod <name>` to `mod.rs`, register in `main.rs`.

## Save sync (`src/sync/`)

Every save is a **unit**: one local file (flat-file emulators) or one directory
(Switch), paired with at most one RomM entry. The same `sync_unit` runs before
launch and after exit; the path is chosen by whether `game_saves_dir` returns
`Some`.

**Three-way decision (`sync/decide.rs`):** compares the local content hash, the
RomM entry, and the `SyncBase` recorded at the last sync (`SyncStore`, persisted
to `~/.local/share/rommd/sync-state.json`). Equal hashes → in sync. Only local
changed → upload. Only RomM changed → download. Both changed, or they differ with
no history → conflict: the newer side wins (local mtime vs RomM `updated_at`),
the losing version is always kept in the backup dir, and the conflict is listed
at `GET /saves/conflicts`. A side that is missing is recreated from the other.
This replaces the old mtime rules and cannot ping-pong: a download records the
new hash as the base.

**Hashes** use RomM's `content_hash` scheme (verified against a live server):
MD5 of the file, or for zips / directory saves MD5 over the sorted
`"<root>/<rel>:<md5>"` lines joined by `\n` — the same as argosy-launcher, so
content uploaded by either side is recognised without history. States have no
`content_hash` in RomM; they fall back to comparing `updated_at` with the base.

**Uploads** PUT the emulator's own entry when one exists and POST otherwise.
Other emulators' entries are never overwritten. **Downloads** back up the local
version to `save-backups/<emu>/<rom_id>/<time>/` (last 5 kept per game) and
write atomically (flat files via temp + rename).

**Directory path (Switch: Eden, Ryujinx):** the save directory is one RomM save,
argosy-launcher compatible: entries under a `<TITLE_ID>/` root, file name
`<rom stem>.zip`, slot `autosave`, `emulator` = the real emulator id. The unit's
remote is the newest zip from *any* emulator in `save_family`, so saves move
between Eden and Ryujinx (their raw contents are identical). A zip whose root
folder is not the title ID is refused. Only the active user is synced.

A game's save is either an **Account** save (per user) or a **Device** save
(one per console, e.g. Animal Crossing). The type found on disk wins, Account
first; with no local save, `switch::DEVICE_SAVE_TITLES` decides.

- **Eden:** `<nand>/user/save/0000000000000000/<USER_ID>/<TITLE_ID>/`. `<nand>`
  from `[Data%20Storage] nand_directory` in qt-config.ini (default
  `config_dir/nand`). `<USER_ID>` = profiles.dat UUID at `0x10 + current_user *
  0xC8`, bytes reversed, uppercase hex; Device saves use the all-zero user ID.
  rommd may create this dir itself. Launched with `-g <rom>`; Eden stays open
  after the game stops, so post-exit sync runs when Eden itself quits.
- **Ryujinx:** slot `bis/user/save/<slot>/` found via `ExtraData0` (save type
  u8 at 0x20: 1 = Account, 3 = Device). `0/` is the
  committed save and `1/` the working copy of the two-phase commit — **not user
  profiles**. `0/` is read; restores write both. A slot exists only after
  Ryujinx has booted the game once (it must be in `imkvdb`), so a fresh install
  cannot receive a save until then. Post-exit re-resolves the slot; a save
  created that session is a fresh start and never wins a conflict, so an
  existing RomM save is restored into the new slot (the fresh one is backed up).

**Flat-file path (everyone else):** files in `saves_dir` / `states_dir` whose
name starts with the `save_filter` prefix; a kind with an empty prefix is
skipped. RomM stores names with an upload timestamp
(`anguna [2026-05-05 13-07-23-697].srm`); `local_file_name` strips it so RomM
entries map back to the emulator's file name, and duplicates resolve to the
newest entry.

## Switch title ID extraction (`src/switch.rs`)

`switch::title_id_for_rom`, shared by Eden and Ryujinx. Update NSPs (`…800`)
map to the base title, since saves belong to the base application. Tiers:

1. **Ticket** — Rights ID in plaintext in the `.tik` file inside the NSP PFS0.
   No keys needed. Fails for scene NSPs with zeroed Rights IDs.
2. **NCA header decryption** — AES-XTS decrypt sector 1 of the `.cnmt.nca` header.
   Requires `header_key` (64 hex chars) in prod.keys (Ryujinx `system/`, Eden `keys/`).
   Nintendo uses non-standard XTS: sector number goes **big-endian into bytes 8–15**
   of the 128-bit tweak (not little-endian into bytes 0–7 like IEEE P1619).
There is deliberately no filename fallback (e.g. `Game[0100000000010000].nsp`):
game IDs must never depend on file names. NSPs where both tiers fail get no
save sync.

## Controllers

Three-layer model: game profile (RomM) → controller profile (RomM) → physical
device (local config). Physical device details stay in `config.toml` under
`[controllers.<id>]` and are never sent to RomM. Controller profiles (button
layouts) are deferred pending RomM API extensions.

`LaunchRequest` accepts `controller_slots: [{slot, controller_id}]` — parsed but
not yet wired to emulator config files. `GET /controllers` lists configured
devices. Write endpoints and `/controllers/scan` are explicitly deferred.

See `dev/controller-design.md` for the full design rationale.

## RomM API

Base URL and credentials are in config. The client is `src/romm/mod.rs`.
Endpoints used: `GET /api/roms/{id}`, `GET /api/roms/{id}/content/{name}`,
`GET /api/saves`, `POST /api/saves`, `PUT /api/saves/{id}`, `GET /api/saves/{id}/content`,
`GET /api/states`, `POST /api/states`, `GET /api/states/{id}/content`.

RomM does not store Switch title IDs in the ROM response — extraction from the
file is the only option.

## Conventions

- `cargo test` before committing
- No comments unless the why is non-obvious
- No filename-based game ID inference (see game_id_for_rom note above)
- `states_dir` returning `None` is the correct signal for "no states", not
  aliasing `saves_dir`
- The daemon is intentionally RomM-only for now. A `LibraryService` trait
  abstraction is the right future path but should not be introduced until there
  is a second concrete implementation to validate the boundary against
