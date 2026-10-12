# rommd — Claude Code context

rommd is a local HTTP daemon (port 7497) that bridges RomM (a self-hosted game
library manager) with local emulators. It handles ROM download, emulator launch,
and bidirectional save/state sync with RomM.

## Architecture

```
RomM (remote) ←→ rommd daemon ←→ local emulators
```

- **`src/main.rs`** — CLI (`rommd`, `rommd service install|start|stop|restart|uninstall`), startup, emulator registry
- **`src/server/`** — axum HTTP handlers, OpenAPI spec (utoipa), request/response types
- **`src/process.rs`** — process tracking (a game is its whole process tree: Unix process group, Windows job object), stop, post-exit sync trigger
- **`src/emulator/`** — one file per emulator, all implement the `Emulator` trait
- **`src/romm/`** — RomM HTTP client (ROM download, save/state upload/download)
- **`src/config.rs`** — config file parsing, controller registry
- **`src/switch.rs`** — Switch NSP title ID extraction (ticket + NCA header decryption)
- **`src/n3ds.rs`** — 3DS title ID extraction (NCSD / NCCH headers, CIA TMD; no keys)
- **`src/sync/`** — save/state sync engine, three-way decision, persistent sync state
- **`src/save_archive.rs`** — zip/restore and RomM-compatible hashing of directory saves
- **`src/profile.rs`** — portable settings overlay (ProfileBlob)
- **`src/ini.rs`** — INI file read/write used by DuckStation and Dolphin
- **`src/update.rs`** — signed self-update from GitHub Releases, `GET/POST /update`
- **`src/service.rs`** — start at login: systemd user unit, launchd agent, Windows per-user logon task (XML, `--background`)
- **`install/`, `.github/workflows/`** — install scripts, CI, release pipeline (`dev/releasing.md`)
- **`contrib/rommd-button.user.js`** — temporary userscript adding a Launch button next to Play on RomM game pages (anchored on the buttons' aria-labels); keep it ASCII-only

## API access

rommd binds 127.0.0.1 only, but any page in the user's browser can still reach
it, so `server/security.rs` rejects requests whose `Origin` is not the RomM
server's (from `romm.base_url`) or listed in `server.allowed_origins`, limits
CORS to those origins, and requires a localhost `Host` header (DNS
rebinding). Clients without a browser send no `Origin` and pass.

## Updates and releases

Releases (tag `vX.Y.Z`, `release.yml`) publish one raw binary per target,
`rommd-<target>[.exe]`, each with a minisign signature whose trusted comment is
`rommd <version> <asset>`. The updater installs only binaries that verify
against the key `build.rs` embeds from `minisign.pub` *and* carry exactly that
comment (no relabelled old versions, no other platform's build); builds without
the key never install. `[updates] mode` = auto (default; daily check, install
while no game runs, then restart) / notify / off. Restart: with
`ROMMD_SUPERVISED` set (by the service definitions) rommd exits and the service
manager restarts it; otherwise it spawns the new binary first. The systemd unit
uses `KillMode=process` and the launchd agent `AbandonProcessGroup` so running
emulators survive a rommd restart. The Windows task is registered from XML
(a per-user logon trigger needs no elevation) to lift Task Scheduler's
battery, 3-day and below-normal-priority defaults; it runs `rommd
--background`, which logs to `<data dir>/rommd.log` and detaches from the
console window. It is not supervised: the updater spawns the new binary. Linux release builds are static musl;
reqwest uses rustls and zip only deflate, so there are no C dependencies
beyond `ring`.

## Emulator trait

Every emulator implements `src/emulator/mod.rs::Emulator`. Key methods:

- `save_layout` — async; where this game's saves and states live
  (`src/emulator/layout.rs`). `saves` is a `SaveSet`: `Dir` (a directory tree,
  one zip: Switch, Wii), `Bundle` (a set of files, one flat zip: GameCube
  `.gci`) or `Files` (one RomM entry per file). A `Dir` save may span several
  directories, each under its own zip root (`extra_roots`, used by 3DS). `states` is a `FileSet`. `None`
  means that kind is not synced. Called before launch and again after exit, so
  it may return more once the session created things (Ryujinx's slot,
  DuckStation's cache); after exit the fresh layout is merged over the launch one
- `FileSet` / `FileMatcher` — the files in one directory that belong to a
  game. `Stem` matches `<stem>` followed by `.`/`_` or nothing (never `Game
  Plus.sav` for "Game"); build it with `FileSet::stem`, which refuses an empty
  stem. `Gci` matches `.gci` files by the game ID in their header
- `save_family` — emulator IDs whose RomM saves this emulator can restore
  (Switch emulators share `switch::SAVE_COMPATIBLE_EMULATORS`)
- `game_id_for_rom` — sync; returns the emulator's stable game identifier.
  Used for save scoping. Must not use the ROM filename as a source — filenames
  can be wrong and would silently sync saves for the wrong game
- `snapshot` / `launch` — profile overlay and process spawn

Adding a new emulator: create `src/emulator/<name>.rs`, implement the trait,
add `pub mod <name>` to `mod.rs`, register in `main.rs`.

## Save sync (`src/sync/`)

Every save is a **unit** paired with at most one RomM entry: a single file, a
directory, or a bundle of files. `sync_layout` turns a `SaveLayout` into units
and runs the same `sync_unit` before launch and after exit.

**Three-way decision (`sync/decide.rs`):** compares the local content hash, the
RomM entry, and the `SyncBase` recorded at the last sync (`SyncStore`, persisted
to `~/.local/share/rommd/sync-state.json`). Equal hashes → in sync. Only local
changed → upload. Only RomM changed → download. Both changed, or they differ with
no history → conflict: the newer side wins (local mtime vs RomM `updated_at`),
the losing version is always kept in the backup dir, and the conflict is listed
at `GET /saves/conflicts`. A unit that did not exist locally at launch (first
boot, or a restore that failed offline) is a fresh start: it never wins a
conflict. Pre-launch sync returns the keys of the units present, and the
post-exit pass gets them back. A side that is missing is recreated from the other.
This replaces the old mtime rules and cannot ping-pong: a download records the
new hash as the base.

**Hashes** use RomM's `content_hash` scheme (verified against a live server):
MD5 of the file, or for zips MD5 over the sorted `"<entry name>:<md5>"` lines
joined by `\n` — the same as argosy-launcher, so
content uploaded by either side is recognised without history. States have no
`content_hash` in RomM; they fall back to comparing `updated_at` with the base,
and on first contact (no base) RomM's copy is downloaded and hashed, so an
identical state on a new device is in sync rather than a conflict.

**Uploads.** A RomM *slot* is a named save history: each slotted upload is a
new version (RomM tags the file name with the upload time) and the newest one
is the slot's current save. Archive saves POST a new version into `autosave`
with `autocleanup` (RomM keeps the newest 10 per ROM and slot, across
emulators), like argosy-launcher. RomM's cleanup is scoped by ROM and slot
only, not file name, so a single-file save gets the same history only while it
is the game's one save file and no other file sits in the slot
(`single_file_history_ok`); otherwise it updates in place (PUT the emulator's
own entry, else POST without a slot) so one file's versions can never delete
another's only copy. Games with several save files should be a `Bundle`. Uploads always send
`overwrite=true`, or RomM answers content already in the slot with that older
entry, and a revert would be undone by the next sync. **Downloads** back up the local
version to `save-backups/<emu>/<rom_id>/<time>/` next to the sync state (last 5 kept per game) and
write atomically (flat files via temp + rename).

**Archive saves (`Dir`, `Bundle`)** are one save history per game,
argosy-launcher compatible: file name `<rom stem>.zip`, slot `autosave`,
`emulator` = the real emulator id. The unit's remote is the slot's newest zip
from *any* emulator in `save_family`, so Switch saves move between Eden and Ryujinx (their raw
contents are identical). A `Dir` zip's entries sit under `archive_root/` (and
any `extra_roots`); a zip with any other root is refused, a root it lacks
leaves its directory untouched (empty folders are kept as zip directory
entries, which don't count towards `content_hash`); a `Bundle` zip is flat and every file must
pass the matcher (GCI header), so another game's save is never restored.

**Switch (Eden, Ryujinx):** only the active user is synced.

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

**Dolphin:** `config_dir` is the user dir (`~/.local/share/dolphin-emu` on
Linux). `Dolphin.ini` is read from `config_dir/Config/` (macOS, Windows,
portable) or `~/.config/dolphin-emu/` (Linux XDG split). The disc header magic
tells GameCube from Wii.
- **GameCube:** `Bundle` of the game's `.gci` files in `GC/<region>/Card A/`
  (region from the ID's 4th char: E→USA, J→JAP, else EUR; a card already
  holding the game's saves wins), or `GCIFolderAPath[Override]` when set. Slot
  A must be a GCI folder (`SlotA` = 8 or unset); a raw card (1) holds all
  games in one image and is not synced.
- **Wii:** `Dir` of `<NAND root>/title/00010000/<hex game code>/`, rooted at
  that folder name like argosy. It only counts as a save once `data/` has files
  (`required_subdir`), since Dolphin writes `content/title.tmd` on first boot.

**mGBA:** `<rom stem>.sav`. RomM's web player (EmulatorJS) uploads the same
raw battery save as `<rom stem> [<time>].srm` tagged `mgba`, so the `FileSet`
maps `.srm` to `.sav` (`remote_extensions`) and saves move between browser and
desktop. Other libretro cores need a per-emulator check before such a mapping:
N64's `.srm`, for one, combines several save types in one file.

**Azahar (3DS):** `config_dir` is the user dir (`~/.local/share/azahar-emu`);
qt-config.ini is in `config_dir/config/` (portable, macOS) or
`~/.config/azahar-emu/` (Linux). The SD card is `config_dir/sdmc` unless
`[Data%20Storage] use_custom_storage=true` points `sdmc_directory` elsewhere.
A save is two trees under `sdmc/Nintendo 3DS/<ID0>/<ID1>/` (all-zero IDs on
Azahar): `title/<high>/<low>/data/` (the save archive plus its
`00000001.metadata`) and `extdata/00000000/<low >> 8>/` — some games keep
progress only in extdata. One zip with roots `data/` and `extdata/`, as
argosy does; those roots don't name the title, so only RomM's ROM link guards
against a wrong zip. Title IDs come from the ROM header; update and DLC IDs
map to the base title. States are `states/<TITLE ID>.<NN>.cst` (00 is the
quicksave). Save family: Citra forks (citra, citra_mmj, lime3ds, borked3ds).

**DuckStation:** states are `<serial>_N.sav`. A game's memory cards (`_1`,
`_2`) are one `Bundle`, so they stay consistent and keep a history. Cards in the default
per-game-title mode are named by the *save title* from DuckStation's game
database (`cache/gamedb.cache`: length-prefixed serial, title, sort title,
localized title, save title), falling back to the gamelist cache title.

**Single files:** RomM stores names with an upload timestamp
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

Deferred work (Ryujinx slot creation, RomM device registration, state hashes,
named save histories) is tracked in `dev/backlog.md`.

## RomM API

Base URL and credentials are in config. The client is `src/romm/mod.rs`.
Endpoints used: `GET /api/roms/{id}`, `GET /api/roms/{id}/content/{name}`,
`GET /api/saves`, `POST /api/saves`, `PUT /api/saves/{id}`, `GET /api/saves/{id}/content`,
`GET /api/states`, `POST /api/states`, `GET /api/states/{id}/content`.

RomM does not store Switch title IDs in the ROM response — extraction from the
file is the only option.

## Tests

`cargo test` runs everything offline; nothing touches a real RomM or the user's
data dir.

- **`src/testing/fake_romm.rs`** — in-process fake of RomM 5.2.0's save and
  state API (slotted uploads tagged and versioned, `overwrite`, `autocleanup`
  per ROM and slot, PUT in place, `content_hash` scheme). Its behaviour is
  taken from RomM's `endpoints/saves.py` / `states.py`; re-check them when
  targeting a newer RomM. A virtual clock starts an hour in the past and ticks
  per upload.
- **`src/sync/scenarios.rs`** — end-to-end sync scenarios: simulated devices
  (a temp save dir + `SyncStore` each) sharing one fake RomM. Backups go next
  to the store (`SyncStore::backups_dir`), so tests stay in temp dirs.
- **`src/testing/mod.rs`** — `TestEmulator` (identity and save family only),
  `TempDir`, file helpers.
- Emulator layouts are tested against fixture dirs in each emulator's module.

A new sync rule gets a scenario; a new emulator gets a layout test.

## Conventions

- `cargo test` before committing
- No comments unless the why is non-obvious
- No filename-based game ID inference (see game_id_for_rom note above)
- `SaveLayout.states` being `None` is the correct signal for "no states"
- The daemon is intentionally RomM-only for now. A `LibraryService` trait
  abstraction is the right future path but should not be introduced until there
  is a second concrete implementation to validate the boundary against
