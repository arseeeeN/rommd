# Backlog

Deferred work with enough context to pick it up later.

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


## Key the local ROM cache on the platform slug

The local ROM cache is `roms/<platform_fs_slug>/`, but `fs_slug` is the
platform's folder name on the RomM server, which can be renamed (e.g. `ngc` to
`gc`, with a `config.yml` platform binding keeping the slug `ngc`). A rename
re-downloads that platform's games into a new folder and orphans the old one.
Keying the cache on the canonical `slug` avoids that, at the cost of moving
existing caches once. Check how RomM's custom folder structure support (the
release after 5.2) reports `slug` and `fs_slug` before deciding. Emulator
choice already uses `slug` (`Emulator::platforms`).

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
- `search_games(query)` — `games::find_games` (`GET /games`), which already
  returns title, platform, `rom_id` and the emulator a launch would use.
- `launch_game(rom_id, emulator?)` — `POST /launch`, which picks the
  platform's emulator when none is given.
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

Find installed emulators and fill in `[emulators.*]`, so a new install works
without hand-written paths.

**Done:** the macOS scan (`src/detect.rs`) and `rommd detect`, which prints
what it finds next to the config, for Dolphin, DuckStation, Ryujinx, mGBA and
Azahar (`Emulator::mac_app`). Not yet detected on macOS: Eden (bundle ID
`com.yuzu-emu.yuzu`, yuzu's, so also require the executable `eden`; data in
`~/.local/share/eden`, XDG paths even on macOS). Next: the config editor,
`rommd detect --save`, and filling in detected emulators on first run.

**How far to look** (decided): known install locations; the OS's own index
where it identifies apps (macOS Spotlight by bundle ID, ~30 ms a query); a
path the user names (`rommd detect <path>`, the same hint-and-verify check as
the config API); never a full-disk scan (slow, wakes external drives, privacy
prompts). Every candidate, however found, passes the same file check. Looking
outside the usual install folders stays on the CLI or a user-triggered UI: on
macOS a background daemon reading `~/Desktop` or `~/Downloads` makes the
system ask the user for access out of nowhere. An app found outside the
install folders gets a hint to move it there, since it is easily deleted or
moved and the config then breaks. Windows and Linux have no index by
identity: look shallowly in Desktop, Downloads and Documents, plus `PATH`,
Flatpak and the registry's install entries.

**Parts, one code path for all uses:**
- A detection scan returning suggestions (`binary`, `config_dir`, how the
  install was found), with no side effects.
- A config editor that adds an `[emulators.<id>]` table with `toml_edit`
  (already pulled in by `toml`), which keeps every comment, blank line and the
  order of everything it doesn't touch. New tables go after the last real
  `[emulators.*]` table, else at the end of the file. Placing them "after the
  emulators comment block" would mean matching comment text, which is fragile.
- First run: generate the template for this OS only (other platforms'
  examples are noise there; `dev/config.example.toml` keeps them all), then
  add what was detected through the same editor and drop the commented
  example of each emulator that was found.
- `rommd detect` prints suggestions; `rommd detect --save` adds them.
- Never edit an existing config in the background: the user may have it open,
  and a silent write is how their work gets lost.

**Writing the file:** re-read it right before editing and refuse if it doesn't
parse (that is the user's to fix, not rommd's to "repair"); write via temp file
and rename; never overwrite an existing emulator entry (suggest instead when its
paths no longer exist); hot reload picks up rommd's own write. Write home paths
as `~/…`, and Windows paths as single-quoted literal strings, since
`toml_edit` escapes every backslash in a basic string.

**Detection pitfalls:**
- A wrong `config_dir` is the dangerous failure: saves sync from or into the
  wrong place, silently, while a wrong `binary` fails loudly. Derive the data
  dir from the install that was found (Flatpak: `~/.var/app/<app id>/…`,
  native: `~/.local/share/…`, portable: next to the program, e.g. Ryujinx's
  `portable/`, Dolphin's `portable.txt`) and check it with
  `config_dir_markers`. An emulator that never ran has no data dir yet: write
  the expected path and say so.
- Several installs of one emulator (Flatpak and AppImage, Dolphin stable and
  dev) share one ID: pick by a fixed rule (native before Flatpak) and list the
  others.
- Eden and Ryujinx both found: config order would silently decide Switch, so
  write both plus a commented `# [platforms] switch = "…"` hint.
- Flatpak has no single binary; the exported launcher
  (`/var/lib/flatpak/exports/bin/<app id>`, `~/.local/share/flatpak/exports/bin`)
  should work as `binary`. **Untested:** rommd has never run a Flatpak
  emulator; process-group tracking and the post-exit sync through Flatpak's
  sandbox launcher need checking (planned on a Steam Deck).

**Where to look:** macOS `/Applications/*.app` and `~/Applications`,
identified by the bundle identifier in `Contents/Info.plist`; Linux packages
(`/usr/bin`), Flatpak exports, AppImages (`~/Applications`); Windows Program
Files, `%LOCALAPPDATA%`, Scoop, winget.

## Editing the config through the API (postponed)

Endpoints to add or change emulators would let frontends (RomM, a launcher)
offer a settings UI. Postponed for security: choosing `binary` means choosing
which program rommd runs, so anything with script access to an allowed origin
(an XSS bug in RomM, say) could make rommd run any program on the machine.

**Way forward, built on auto-detection:** the request never sets a path to
run. It can only hint ("there might be a Dolphin install at this path"), and
rommd adds the entry only when its own checks find an actual install of that
emulator there, through the same detection code and config editor. The checks
must be static (bundle identifier, file layout, data dir markers) and must
never execute the candidate: "run it with `--version` to see what it is" would
run any file a page points at. Changing other settings through the API (ports,
origins, update mode) has its own risks and is a separate decision; ROM and
data paths would need similar checks.

