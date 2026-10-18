# Backlog

Deferred work with enough context to pick it up later.

## Create Ryujinx save slots so a fresh install can restore before first boot

**Problem.** A Ryujinx save slot (`bis/user/save/<id>/`) only exists after
Ryujinx has booted the game once, because the slot must also be registered in
its save index. On a fresh install rommd has nowhere to restore a RomM save
before launch, so the first session starts from a new game. The fallback today:
post-exit re-resolves the slot, and since a save created that session never wins
a conflict, the RomM save is restored into it for the *next* launch.

**Rejected: boot, watch, kill, relaunch.** The user would see the game boot
twice (10–60 s with shader compilation). Killing Ryujinx right after the slot
appears can interrupt its write to `imkvdb.arc`, which indexes every game's
saves. And some games only create their save at the title screen or after a
prompt, so the watch could wait indefinitely.

**Proposed: create the slot the way Ryujinx does.** The formats below were
read from a real install (2026-10-18): a slot Ryujinx had just created and its
save index, before and after a launch.

- **Save index**: `bis/system/save/8000000000000000/{0,1}/imkvdb.arc` (itself
  a two-phase system save; both copies were identical). Header: `IMKV`, u32
  reserved (0), u32 entry count. Then per entry: `IMEN`, u32 key size (0x40),
  u32 value size (0x40), the key, the value — 140 bytes each.
  - Key (SaveDataAttribute): program ID u64 at 0x00, user ID at 0x08 (two
    u64, the first 16 hex digits of the profile ID first), system save ID u64
    at 0x18, type u8 at 0x20 (0 System, 1 Account, 2 BCAT, 3 Device).
  - Value: save data ID u64 at 0x00 (the slot folder name), space u8 at 0x18
    (1 = user). Everything else was zero.
  - Entries were in slot order, which also matches LibHac's attribute order
    (program, type, user). Insert new entries at their sorted position so both
    readings hold.
- **Next ID**: `lastPublishedId` next to `imkvdb.arc` (in `0/` and `1/`), a
  u64 holding the last slot ID handed out. New slot = value + 1; write it back.
- **Slot**: `bis/user/save/<id:016x>/` with `0/`, `1/`, an empty `.lock`, and
  `ExtraData0` = `ExtraData1` (0x200 bytes): the 0x40-byte key, then owner ID
  u64 at 0x40 (the program ID), timestamp u64 at 0x48 (0), flags u32 at 0x50
  (0), data size i64 at 0x58, journal size i64 at 0x60, a random commit ID u64
  at 0x68, zeros after.
- **User ID**: `last_opened` in `system/Profiles.json`.
- **Only while Ryujinx is closed.** Ryujinx rewrites the index whenever it
  starts, so a write while any Ryujinx instance runs (even another
  game) would be lost or clobber it.

**Test reference: Super Mario Odyssey** (`0100000000010000`) from that install.
Index entry for its Account save, slot 1, default profile
`00000000000000010000000000000000` (all bytes not listed are zero):
- key: `0x02`=01, `0x07`=01 (program ID), `0x08`=01 (user), `0x20`=01 (Account)
- value: `0x00`=01 (slot 1), `0x18`=01 (user space)

Its `ExtraData0`: the same key, owner `0x0100000000010000`, data size
`0x1000000` (16 MiB), journal size `0x300000` (3 MiB), commit ID
`0x4FFE9263665134B4`. The writer's output must match these bytes except the
commit ID. (Slot 2 of the same game is its BCAT save, type 2; rommd only
creates Account and Device saves.)

**Save sizes: tested, Ryujinx ignores them** (Ryujinx 1.3.3, 2026-10-18). On
a scratch copy of the data dir (`Ryujinx --root-data-dir <dir> <rom>`), Super
Mario Odyssey's slot was replaced by a hand-made one (new ID from
`lastPublishedId`, sorted index entry, data and journal size 0, the real save
files copied in). The game loaded the save and saved into it; Ryujinx rewrote
both `ExtraData` files on that commit (new commit ID) but left the sizes at 0,
and kept the index entry. So writing 0 works. The real sizes are the game's
NACP (`control.nacp` in the control NCA's RomFS), which would need title-key
and section decryption on top of today's header decryption (ticket title key
or key area, AES-CTR, RomFS lookup). Write 0 for now and read the NACP if a
Ryujinx version starts checking the sizes; until then, only create slots on
Ryujinx versions tested this way.

Ryujinx also rewrites `imkvdb.arc` (same content) every time it starts, even
without a game, which is why slots may only be created while it is closed.

**Safety.** Back up `imkvdb.arc` and `lastPublishedId` before every write,
refuse to write if the header, key/value sizes or file length don't match the
format above, write the working copy (`1/`) before the committed one (`0/`),
and keep the post-exit fallback.

## Register rommd as a RomM device

RomM (5.x) tracks per-device sync state (`/api/devices`, `device_id` on save
uploads and downloads, `device_syncs` on saves) and offers `POST
/api/sync/negotiate`, which argosy-launcher uses. rommd keeps its own three-way
sync state instead, because negotiate expects the client to track the
last-synced hash anyway, matches saves on `emulator` (at odds with the Switch
save family), and covers saves but not states. Registering a device and passing
`device_id` would still let RomM's own tracking and UI see rommd.

## Content hashes for save states

RomM returns no `content_hash` for states, so state sync falls back to comparing
`updated_at` with the last synced value, and on first contact downloads RomM's
copy to compare (states can be tens of MB, so that costs one download per state
on a new device). Revisit if RomM adds hashes for states.

## Named save histories (slots)

A RomM slot is a named save history: every upload into it is a new version, the
newest is the slot's current save, and `autocleanup` keeps the last N (see
CLAUDE.md, Save sync). rommd always plays and syncs `autosave`, the same
default history argosy-launcher uses. Separate histories would let a player
keep, say, a speedrun or a 100% run apart from their main playthrough.

- Add an optional `slot` to `POST /launch`, so a game syncs against that
  history instead of `autosave`: pick the slot's newest entry as the unit's
  remote, upload new versions into it, and key the sync state by slot as well.
- The sync state and the conflict rule already work per unit, so switching
  histories is a restore, like switching emulators.
- Single-file saves have no slot today (see CLAUDE.md for why); named histories
  for them need per-file slot names or a RomM cleanup scoped by file name.
- In RomM's UI, an editable dropdown on the game page to choose which history
  to play and to create new ones would pass the chosen slot to the launcher.
  That is a RomM feature and would need upstreaming there.

## Title IDs from RomM's ROM metadata

Newer RomM releases extract Switch and 3DS title IDs on the server, so they
will appear in the ROM metadata. rommd reads them from the ROM file today
(`switch.rs`, `n3ds.rs`), which needs the file present locally and, for some
Switch NSPs, prod.keys. Once the field is in the API: prefer RomM's value,
keep local extraction as the fallback for older servers and local-path
launches, and warn when both exist but differ — the file is then not the game
RomM thinks it is, and syncing its saves would attach them to the wrong ROM.

## Azahar per-game settings

Azahar keeps per-game settings in `~/.config/azahar-emu/custom/<TITLE ID>.ini`
(Linux; `config_dir/config/custom/` elsewhere). That is where profile support
(`launch` overlay, `snapshot`) would hook in, as Dolphin's GameSettings do.


## MCP server for assistants

Let an AI assistant find and start games ("play Mario Kart", "continue my
Zelda save", "why did my save conflict"). Starting a game is already one click
in RomM, so the payoff is in the tasks around a launch, not the launch itself.

**The OpenAPI spec is not enough.** Generic OpenAPI-to-MCP bridges would expose
the routes one-to-one, but:
- `POST /launch` needs a `rom_id` and an `emulator` id, and nothing in rommd
  resolves a title to either. Search lives in RomM, so the assistant would need
  RomM's API and credentials as well, plus the platform → emulator mapping.
- `profile`, `local_path` (launches any file on disk) and `snapshot` are not
  for a model to choose, and `/update` must stay a user action.

**Proposed: a small set of tools written for the job.**
- `search_games(query, platform?)` — through rommd's RomM client, returning
  title, platform and `rom_id`.
- `launch_game(rom_id, emulator?)` — without `emulator`, rommd picks a
  configured one for the ROM's platform (worth having in the daemon anyway).
- `status`, `stop_game`, `list_save_conflicts` — map onto existing routes.

**Transport.** A `rommd mcp` stdio subcommand that calls the running daemon on
localhost: the client spawns it, nothing new is exposed on the network, and
`server/security.rs` is unaffected. A streamable-HTTP endpoint in the daemon
would also work, but MCP clients send no RomM `Origin`, so it would need its
own place in the access rules.

## A status surface for the background daemon

Windows users expect a background app to have a notification-area icon
(status, open config or log, check for updates, quit). A tray icon means
platform-specific UI code on three platforms, so it is deferred. The
preferred direction is to configure and watch rommd from RomM instead, which
depends on what RomM's API can carry. Until then the log file
(`<data dir>/rommd.log` under the logon task) and the HTTP API are the
interface.

## User documentation beyond the README

The README carries install, configuration and per-emulator notes, and keeps
growing (manual installs, Windows path quoting, where each emulator keeps its
folder on each OS, portable installs). Move the detail into a wiki or docs
site, one page per emulator and per platform, and keep the README to a quick
start with links.

## Sync saves while the game runs

Newer RomM versions sync saves from the browser player while the game runs,
by watching its save files. rommd syncs only before launch and after exit,
so a shutdown, crash or power loss during play loses the upload until the
next launch, and another device can't pick up progress mid-session.

**Idea.** Watch the launch layout's save locations (`notify` crate:
inotify, FSEvents, ReadDirectoryChangesW) and run the same `sync_unit`
on a change, upload-only while the game runs.

**To work out first.**
- How RomM's browser sync behaves (endpoints, slot, how often it uploads),
  so both sides agree on what a mid-session version is.
- Debounce: emulators write saves in several steps (Ryujinx's two-phase
  commit, Dolphin rewriting GCI files, multi-file bundles), so wait until
  the files are quiet and consistent before hashing.
- History: archive saves go into `autosave`, where RomM keeps the newest
  10 per ROM and slot. An upload per in-game save would push out older
  versions within one session; coalesce, or upload mid-session versions
  somewhere that does not rotate the history.
- Base and conflicts: each mid-session upload becomes the new base, so the
  post-exit pass stays a no-op when nothing changed after the last upload.
- Restores never happen mid-session; a newer RomM save that appears while
  the game runs is a conflict for the post-exit pass, as today.

## Tag save states with the emulator version

Save states are snapshots of the emulator's internals and often only load in
the version that made them. Seen with Dolphin: a state made by Dolphin 2606
(laptop) synced to a PC running 2609, and 2609 refused it as incompatible.
States update in place in RomM, so two devices on different versions keep
replacing each other's state with one the other can't load (the replaced
local state is backed up, nothing is lost). Saves are not affected.

**Idea.** Record the emulator version with each uploaded state and only
restore a state made by the version installed here; keep others in RomM
untouched instead of overwriting them.

**What we know.**
- Dolphin writes its version into every state header: 6-byte game ID
  (padded to 16 bytes), f64 time, a u32 magic `0xBAADBB7D`, then a u32
  length and the version string (`Dolphin 2606`).
- `Dolphin.exe --version` prints the installed version (`Dolphin 2609`),
  also on Windows when its output is captured.

**To decide.** Where the tag lives: RomM's state entries have no version
field, so it would go into the file name (tags like `[Dolphin 2606]` would
need to survive `local_file_name`) or the `emulator` field, or RomM gains a
field. Then check the other emulators: DuckStation, mGBA and Azahar states
may each have their own version rules.

## Emulator auto-detection

Find installed emulators and fill in `[emulators.*]` on first start, and
offer the same scan as an endpoint returning suggestions. Look where each
platform installs them: Linux packages (`/usr/bin`), Flatpak (exports and
`~/.var/app/<app id>/` for the data dir), AppImages (`~/Applications`); macOS
`/Applications/*.app`; Windows Program Files, `%LOCALAPPDATA%`, Scoop, winget.
Each emulator's `config_dir` has to be found the same way, and differs for
Flatpak installs (e.g. `~/.var/app/org.DolphinEmu.dolphin-emu/data/dolphin-emu`).

## Editing the config through the API (postponed)

Endpoints to add or change emulators (`PUT /config/emulators/{id}` with
`binary` and `config_dir`) would let frontends offer a settings UI, writing
`config.toml` with `toml_edit` so comments survive and hot reload applies it.
Postponed for security: choosing `binary` means choosing which program rommd
runs, so anything that gains script access to an allowed origin (an XSS bug in
RomM, say) could make rommd run any program on the machine. If revisited,
allow such writes only from local tools (no browser `Origin`), or from a
browser only after an explicit opt-in such as
`server.allow_config_from_browser = true`. Auto-detection covers most of the
need without this risk.
