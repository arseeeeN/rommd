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
- **`src/process.rs`** — process tracking, save sync logic (pre-launch + post-exit)
- **`src/emulator/`** — one file per emulator, all implement the `Emulator` trait
- **`src/romm/`** — RomM HTTP client (ROM download, save/state upload/download)
- **`src/config.rs`** — config file parsing, controller registry
- **`src/switch.rs`** — Switch NSP title ID extraction (ticket + NCA header decryption)
- **`src/profile.rs`** — portable settings overlay (ProfileBlob)
- **`src/ini.rs`** — INI file read/write used by DuckStation and Dolphin

## Emulator trait

Every emulator implements `src/emulator/mod.rs::Emulator`. Key methods:

- `saves_dir` / `states_dir` — async; `states_dir` returns `Option<PathBuf>` where
  `None` means the emulator has no save states (Ryujinx, Eden)
- `game_saves_dir` — async; returns `None` to use `saves_dir`, or a specific
  path for emulators with opaque save directories (Ryujinx scans ExtraData0)
- `game_id_for_rom` — sync; returns the emulator's stable game identifier.
  Used for save scoping. Must not use the ROM filename as a source — filenames
  can be wrong and would silently sync saves for the wrong game
- `save_filter` — filename prefix used to scope flat-file save sync. Ryujinx
  returns an empty `SaveFilter` because it uses `game_saves_dir` instead
- `snapshot` / `launch` — profile overlay and process spawn

Adding a new emulator: create `src/emulator/<name>.rs`, implement the trait,
add `pub mod <name>` to `mod.rs`, register in `main.rs`.

## Save sync

Two sync paths, chosen at launch time based on whether `game_saves_dir` returns
a `Some`:

**Profile-dir path (Ryujinx):** `slot_dir` = `bis/user/save/<slot_id>/`. Each
immediate subdir (`0/`, `1/`) is a user profile. Pre-launch syncs bidirectionally
per profile, using the profile name as the RomM `slot` parameter. Post-exit
uploads files modified since session start.

**Flat-file path (everyone else):** syncs files in `saves_dir` / `states_dir`
filtered by `save_filter` prefix.

**Pre-launch upload rule:** only upload files not yet in RomM (bootstrap).
Files already in RomM are downloaded if RomM is newer, otherwise left alone.
The post-exit sync handles uploading files actually modified during the session.
This prevents a ping-pong where downloading a file advances its local mtime,
triggering a re-upload next launch, and so on.

**Ryujinx backup filtering:** Ryujinx writes timestamped backup copies alongside
canonical saves (`Common [2026-09-26_18-16-21].bin` next to `Common.bin`). These
are skipped during sync — only files whose names do not contain `[` are synced.

## Switch title ID extraction (`src/switch.rs`)

Three tiers, tried in order by `ryujinx.rs::game_id_for_rom`:

1. **Ticket** — Rights ID in plaintext in the `.tik` file inside the NSP PFS0.
   No keys needed. Fails for scene NSPs with zeroed Rights IDs.
2. **NCA header decryption** — AES-XTS decrypt sector 1 of the `.cnmt.nca` header.
   Requires `header_key` (64 hex chars) in `config_dir/system/prod.keys`.
   Nintendo uses non-standard XTS: sector number goes **big-endian into bytes 8–15**
   of the 128-bit tweak (not little-endian into bytes 0–7 like IEEE P1619).
3. **Filename bracket notation** — `Game[0100000000010000][V0].nsp` — last resort,
   validated to 16 hex chars with top byte 0x01.

The filename fallback was explicitly added after tiers 1 and 2 both failed for
scene NSPs. Do not remove it without a better replacement.

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
`GET /api/saves`, `POST /api/saves`, `GET /api/saves/{id}/content`,
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
