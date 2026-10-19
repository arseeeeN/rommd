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


## Search and launch games from the CLI

A fallback to the RomM userscript, and handy for testing: find a game and start
it from a terminal.

```sh
rommd search xenoblade platform:switch
rommd play "Mario Odyssey"              # one match: launch; several: pick one
rommd play --first mario platform:switch
rommd play 199                          # a bare number is a ROM ID
```

- **Thin client.** The CLI calls the running daemon: `play` goes through
  `POST /launch`, so launching, process tracking and the post-exit sync stay
  in one place. Search is a new daemon endpoint (e.g. `GET /games?q=`) using
  rommd's RomM client and credentials, shared with the MCP `search_games` tool
  below and usable by the userscript.
- **Ambiguity.** `play` launches only when exactly one game matches. With
  several, it lists them numbered and asks on a terminal, otherwise exits with
  the list; `--first` takes the top hit. Not the default: a vague query must
  not start the wrong game. Not `rommd start`, which reads like starting the
  daemon (`rommd service start`).
- **Query language.** Free text plus `key:value` filters (booru/GitHub style),
  quoting values with spaces:
  - `platform:<slug>` — RomM's platform slug or fs slug (`switch`, `ngc`, …).
  - `emulator:<id>` — which emulator to launch with when a platform has
    several (Eden and Ryujinx).
  - `library:romm` — the game library to search. Not "provider", which in
    RomM means metadata providers (IGDB etc.). Only `romm` is accepted until a
    second library (Gaseous) exists; see the `LibraryService` note in
    CLAUDE.md.
- **Needs a platform → emulator mapping** in the daemon, the same one the MCP
  `launch_game` needs (today it only exists in the userscript). When several
  configured emulators handle a platform (Eden and Ryujinx), the first of
  these decides:
  1. The request names one: `emulator:` in the query, the userscript, later
     an emulator choice in RomM's UI.
  2. An optional per-platform default in the config, `[platforms] switch =
     "eden"`.
  3. The first one in the config file. `Config::emulators` is a `HashMap`
     today, so it must become an `IndexMap` to keep the file's order.

  `rommd play` says which emulator it picked and why ("using ryujinx, first
  configured for switch; set `[platforms] switch` to change"), since config
  order alone is invisible. Key the mapping on RomM's canonical platform
  `slug` (`ngc`), never on `fs_slug` (the server folder name, which can be
  renamed, e.g. to `gc` with a `config.yml` platform binding) or a custom
  display name. `platform:` in queries may match all three, resolved through
  `GET /api/platforms`.
- **Related:** the local ROM cache is `roms/<fs_slug>/`, so renaming a
  platform folder on the server re-downloads its games into a new folder and
  orphans the old one. Keying it on `slug` would avoid that, at the cost of
  moving existing caches once. Check how RomM's custom folder structure
  support (next release) reports `slug` and `fs_slug` before deciding.
- **To check:** RomM's search parameters (`GET /api/roms` with a search term
  and platform IDs; platform slugs resolved through `GET /api/platforms`).

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
