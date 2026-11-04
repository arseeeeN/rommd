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

## PS2 memory cards: what's left (PCSX2)

File cards sync (`ps2_card`, ported from Sigil's `card_ps2.c`): the unit is
`<rom stem>.ps2`, a fresh 8 MB card with ECC holding the game's save folders,
and restore merges them back into the shared cards. Still missing:

- **Folder cards** (a slot whose card is a directory of save folders plus
  `_pcsx2_superblock` and per-folder `_pcsx2_index`): skipped with a warning.
  Sigil's `sync_folder_cards.c` packs a folder card into a card in memory and
  unpacks a unit into folders; restore must write a formatted
  `_pcsx2_superblock` (byte 0x16 = 0x6F) when it is missing, else PCSX2
  ignores the card.
- **argosy's older shape**: a zip of save folders, `<folder>/<file>`, or the
  same under `<card>.ps2/`, uploaded before Sigil for folder cards. Restoring
  it needs the `_pcsx2_index` times, as `sigil_ps2_pack` reads them.
- **`memcardFilters`**: some games read saves under other serials (PCSX2's
  `GameIndex.yaml`); Sigil takes those as the game's ids too. rommd uses the
  disc serial alone.
- **Per-game card overrides** in PCSX2's game settings are not read; nor are
  the multitap slots.
- **Byte identity with Sigil** is by porting, not tested: building Sigil's
  CLI (or a small harness over `sigil_collect`) as a test oracle would check
  the unit and its `content_hash` for the same fixture cards. Open question
  for the RomM devs: are Sigil's unit formats a stable, versioned spec rommd
  can follow, and is that oracle in CI welcome?

The same `SaveSet` machinery would cover Dolphin's raw GameCube cards (one
`.gci` or a zip of them, per Sigil's `gamecube` layout), which rommd skips
today.

## N64 saves rommd doesn't find yet

- RMG's AppImage: its `mupen64plus.ini` is inside the image, so a save RMG
  hasn't created on this computer can't be named before the first launch
  (it is restored after the game first saves). Reading the image (squashfs)
  or RMG's ROM cache would fix it.
- mupen64plus's `SaveFilenameFormat = 0` (header name only) and the pre-2.6
  `<GoodName>.<ext>` files it still reads: not synced.
- gopher64's `.img` (SC64 SD card) and `.romsave` (homebrew ROM writes), and
  64DD disk saves: not in argosy's unit, not synced.
- ROMs inside `.zip`/`.7z` given as the launch file: rommd hashes only raw
  `.z64`/`.v64`/`.n64` files (its own zip downloads are extracted).

## PCSX2 per-game settings

PCSX2 keeps per-game settings in `gamesettings/<SERIAL>_<CRC>.ini` (CRC as
in the save state names, 8 uppercase hex digits). That is where profile
support would hook in.


## Key the local ROM cache on the platform slug

The local ROM cache is `roms/<platform_fs_slug>/`, but `fs_slug` is the
platform's folder name on the RomM server, which can be renamed (e.g. `ngc` to
`gc`, with a `config.yml` platform binding keeping the slug `ngc`). A rename
re-downloads that platform's games into a new folder and orphans the old one.
Keying the cache on the canonical `slug` avoids that, at the cost of moving
existing caches once. Check how RomM's custom folder structure support (the
release after 5.2) reports `slug` and `fs_slug` before deciding. Emulator
choice already uses `slug` (`Emulator::platforms`). Until then, `[platforms]
<slug> folder` pins a platform's folder, so a rename on the server doesn't
move it. Detecting EmuDeck / ES-DE folder names (`gc`, `n3ds`) on their own
when `roms_dir` is such a folder is a later idea.

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

## Being visible without a UI of its own

rommd has no window, which holds it back more than any missing feature.
Rather than a launcher app, rommd should show up where people already are:
Steam, the system tray, a browser tab, RomM. In order:

**1. Steam shortcuts (in progress).** One non-Steam shortcut per game,
running `rommd play --attach id:N` (Game Mode's process tree, see the Steam
Deck entry), with RomM's cover as its portrait capsule. Which games:
- every game rommd has downloaded, by default (no setup);
- plus every game in the RomM collections `[steam] collections` names,
  downloaded or not: the launch downloads it, so a collection is how a game
  reaches Game Mode without a trip to Desktop Mode. A collection adds games;
  it doesn't filter, which could only ever show games already fetched.
- `[steam] downloaded = false` leaves only the collections.

`rommd steam sync` writes them; `rommd steam remove` takes them all out.
rommd only ever touches shortcuts it made (its own program, `play --attach
id:`), never the user's others. Facts to keep in mind:
- `userdata/<account>/config/shortcuts.vdf` is binary VDF (maps, strings,
  int32s); Steam rewrites it from memory when it exits, so rommd edits it
  only while Steam is closed, and Steam sees changes after a restart.
- Artwork goes to `userdata/<account>/config/grid/` named after the
  shortcut's app ID: `<id>p` portrait (RomM's cover), `<id>` wide, `<id>_hero`,
  `<id>_logo`. RomM's `path_cover_large` is named `big.png` but can hold a
  JPEG, so the extension follows the bytes. SteamGridDB (which RomM can
  query) could fill the other three later.
- The app ID is `crc32(exe + name) | 0x80000000`, as Steam ROM Manager
  computes it; two games of one name get their platform in the name.
- Flatpak Steam can't run a host program without `flatpak-spawn --host`.
- To verify on a Deck: that Game Mode picks the shortcuts and their art up
  after a restart, and that launching one downloads and starts the game.

**2. A local page served by rommd** (`http://localhost:7497/`): status,
running game, downloads in progress, recent games, save conflicts with their
backups, and the update. No native UI code; the tray's "Open rommd" opens it.
It is an origin of its own, so it may only use the actions the API already
has, never the config.

**3. A tray item, `rommd tray`** (done on Linux and macOS; Windows: see
"The tray on Windows"): a separate process and a client of the
daemon over HTTP, like `rommd play`, started at login by its own autostart
entry; `[tray] enabled = false` (or no autostart entry) leaves the daemon on
its own. Never part of the daemon: the daemon runs without a display (systemd
user unit), and the Linux release is a static musl build, so a tray crate
needing GTK can't go in. On Linux `ksni` (StatusNotifierItem over D-Bus, pure
Rust; KDE and the Deck's desktop, GNOME needs an extension); Windows and macOS
have native APIs. Menu: status, recent games, "Update available, install",
"Update Steam shortcuts", "Open rommd" (the page), "Open log", quit.

**Further ideas:**
- The Launch button in RomM itself: a small RomM PR for a configurable
  external launcher on the game page would make RomM the UI and retire the
  userscript. To ask the RomM devs.
- Desktop notifications: save synced, download finished, save conflict.
- App-menu entries (Linux `.desktop`, Windows Start menu) from the same games
  and covers as the Steam shortcuts, so a desktop launcher's search finds
  them.
- Discord Rich Presence: "Playing Super Mario 64 via RomM".

## The tray on Windows

`rommd tray` runs on Linux (`src/tray/linux.rs`, `ksni`) and macOS
(`src/tray/macos.rs`, `tray-icon`). The user is writing the Windows version
on that machine. `macos.rs` is the model: the same crate, the same split of
threads. What is shared and what Windows needs (the macOS notes are kept as
a record of what was done):

**Shared already, keep it so.** `src/tray/mod.rs` decides everything: what
the menu shows (`menu(&View) -> Vec<Entry>`, unit-tested), the tooltip, what
an `Action` does (`perform`, over `client::Daemon`), and the loop (`serve`:
the 10 s refresh, the refresh when the menu opens, `Action::Refresh`). A
platform file only turns `Entry`s into native items, sends chosen `Action`s
down the channel, and replaces the view on `show`. Its `Tray` has `show(View)`
and `shutdown()`; `run()` grows one `cfg` per OS. Until Windows has its tray,
clippy fails there on dead code (`serve`'s helpers, `client::Daemon`'s
methods), as it did on macOS before.

**Crate.** `tray-icon` + `muda` (Tauri's) for both; native on Windows
(Shell_NotifyIcon) and macOS (NSStatusItem), no C libraries. Gated with
`[target.'cfg(any(windows, target_os = "macos"))'.dependencies]` (done, with
`png` to decode the icon): its Linux
backend needs GTK, which the static musl build can't link, so Linux stays
on `ksni`. Writing Shell_NotifyIcon by hand with `windows-sys` (already a
dependency) is the fallback if `tray-icon` pulls in too much.

**Event loop: the main difference from Linux.** `ksni` runs on tokio;
`tray-icon` needs the OS loop on the thread that created the icon, and on
macOS that must be the *main* thread (AppKit). `tray::run()` is sync; on
macOS (done) `macos::run` keeps AppKit on main and runs `serve` on tokio in
a spawned thread. Chosen items reach tokio through `MenuEvent`'s handler
(called on main), views come back through a std channel plus
`DispatchQueue::main().exec_async`, and the AppKit objects live in a
main-thread `thread_local`. On Windows any one thread with a message pump
works, but the same split keeps the code alike: the pump instead of
`NSApplication::run`, a posted message instead of the dispatch queue.

**Icon.** `assets/rommd-tray.svg` is a single-colour 24×24 shape (fill
`currentColor`); neither OS takes SVG in the tray. Pre-render PNGs into
`assets/` (e.g. `rsvg-convert -w 32`/`-w 64`) and `include_bytes!` them; no
SVG renderer at runtime.
- macOS (done): `assets/rommd-tray-mac.png`, black on transparent, 44 px
  (tray-icon caps the height at 22 pt; Retina), set with
  `with_icon_templated` so the menu bar recolours it.
- Windows: 16 and 32 px (more at high DPI). The tray doesn't recolour, so
  white for the default dark taskbar, or follow the
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize`
  `SystemUsesLightTheme` value (black on light) and re-read it on
  `WM_SETTINGCHANGE`.

**Starting at login** (`apply_autostart(enabled)`, called by `rommd service
install`/`uninstall` and `rommd setup`; must also remove what it made):
- Windows: an `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` value
  `rommd tray` → `"<exe>" tray`, or a second logon task like the daemon's
  (`service.rs`). The exe is a console-subsystem binary, so the tray must
  detach from its console as `--background` does for the daemon (or the
  user sees a window at login); log to `<data dir>/rommd-tray.log` then.
- macOS (done): `service::tray_agent`, a second LaunchAgent
  (`io.github.rommd.tray`, `RunAtLoad`, `LimitLoadToSessionType` Aqua, no
  `KeepAlive`, log `~/Library/Logs/rommd-tray.log`). Like Linux it only
  writes or removes the file; `start_now()` starts the tray this time.

**Other bits that are Unix-specific in `mod.rs` today:**
- `supported()` is Linux and macOS; `desktop_session()` checks
  `DISPLAY`/`WAYLAND_DISPLAY` on Linux and the absence of `SSH_CONNECTION`
  on macOS; on Windows a logged-in user always has a desktop.
- `other_tray_running()` uses `pgrep -f "rommd tray$"`: fine on macOS; on
  Windows use a named mutex (`CreateMutexW` `Local\rommd-tray`,
  `ERROR_ALREADY_EXISTS` = another runs).
- `start_now()` spawns `rommd tray` detached via a Unix process group; on
  Windows `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` (and no console).
- `open()` already uses `open` / `explorer`.
- `Action::Quit` must end the OS loop too, not only the tokio side
  (`Tray::shutdown`; macOS terminates `NSApplication`).

**Menu behaviour to keep:** refresh when the menu opens (macOS, done:
tray-icon sends the click event before it opens the menu, so the handler
sends `Action::Refresh` and waits up to 300 ms for the view; Windows: on the
click / `WM_CONTEXTMENU`, the same way), and
"Working…" while an action runs. Submenus (`Entry::Submenu`, the recent
games' fullscreen / windowed) map to `muda::Submenu`. muda reads `&` as an
access key mark, so labels double it (`macos::escape`). A menu replaced
while it is open stays on screen, so its items' actions are kept until the
next change (`State::previous`).

## Emulators that stay open after the game

rommd's session (post-exit sync, the tray's "Playing", the updater's wait)
ends when the emulator's process exits. PCSX2 (`-batch`), DuckStation
(`-batch`), Dolphin (`-b`), RMG (`--quit-after-emulation`) and gopher64 quit
with the game. These have no such option (checked 2026-11-04 against
their default branches) and go back to their game list instead, so the
session lasts until the user closes the emulator:

- mGBA (Qt): `Window::gameStopped` shows the logo; only closing the window
  quits. The SDL binary `mgba` ends with the game.
- Ryujinx: `AppHost_AppExit` brings back the game list; only closing the
  main window quits.
- Eden: `OnEmulationStopped` shows the game list; `yuzu-cmd` (SDL) ends with
  the game.
- Azahar: `ShutdownGame` shows the game list; no SDL frontend any more.

Options: an upstream flag in each (small, the others' implementations are
there to copy), or rommd noticing the game stopped another way (the
emulator's log, its window title).

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

## Steam Deck: Game Mode launches and downloads that survive sleep

The Deck is the hardest target: Game Mode has no browser or terminal for the
userscript, it suspends constantly, and its storage is small. In order:

**1. Resumable, per-file ROM downloads (done).** Verified against RomM 5.2: the
per-file endpoint `GET /api/roms/{file id}/files/content/{name}` answers range
requests (`206`), while the bundled `GET /api/roms/{id}/content/{name}?file_ids=…`
(zipped on the fly) ignores them. Downloading a multi-file ROM file by file
makes every download resumable and stops needing twice the space (today a
15 GB game arrives as a 15 GB zip that is then extracted). The bundle's
exact `Content-Length` suggests it is streamed by nginx's `mod_zip`, which can
serve ranges only when every file's CRC-32 is known; RomM leaves `crc_hash`
empty for large files (Xenoblade's), so a RomM PR adding them might make the
bundle resumable (unconfirmed). Per-file downloads are still better here: no
extraction, no wait on RomM. What the bundle adds is RomM's generated `.m3u`;
rommd needs one only for multi-disc games (write it from the disc files) and
picks the base game among several files by each file's `category` (`game`,
`update`, `dlc`); `file_path` gives each file's place in the folder. Keep the `.part`
on network errors (delete it only on cancel), retry with backoff (the network
needs seconds after waking), continue from the `.part` size, and check the
size against RomM's `file_size_bytes`. A daemon restart mid-download then
resumes on the next launch too.

**2. No idle sleep while downloading (done, `src/awake.rs`; verified on
macOS, Linux and Windows untested).** Hold an idle-only inhibitor while a
launch fetches its ROM: `systemd-inhibit --what=idle`, `caffeinate -i` on
macOS, `SetThreadExecutionState` on Windows. Idle only: a deliberate suspend
(power button) must still work. To verify on a Deck: whether Game Mode's own
auto-suspend timer respects logind's idle inhibitors, and whether polkit lets
rommd's user service (not part of the login session) take one; desktops that
manage idle sleep themselves (GNOME, KDE) may also need the session's
`org.freedesktop.ScreenSaver.Inhibit`.

**3. Attached launches for gamescope (done: `rommd play --attach`; verified on
macOS, including the CLI being killed outright; untested in Game Mode).** To
check on a Deck: that gamescope shows and focuses the emulator, that Steam
Input applies, what "Exit game" sends (SIGTERM is forwarded as a stop; with
SIGKILL the emulator, in its own process group, keeps running, tracked by the
daemon, so whether Steam's reaper kills the whole tree matters), and that
Flatpak emulators behave the same. As far as known, gamescope shows and
focuses the windows of the process tree Steam launched; an emulator started
by rommd's background service is outside it (likely hidden behind Steam's UI,
without controller focus) and misses Steam's environment (Steam Input's
controller mapping, gamescope's display and audio). To verify on a Deck. The
fix: `rommd play --attach id:N` as a Steam shortcut (by full path; Game Mode's
`PATH` may lack `~/.local/bin`). The daemon downloads and syncs as now but
hands back the emulator command (program, arguments, environment) instead of
spawning it; the CLI starts it as its own child, waits for it to exit, and
tells the daemon, which runs the post-exit sync. Steam shows "Playing" until
then. When Steam's "Exit game" kills the CLI and the emulator together, the
daemon sees the session drop, confirms the emulator is gone, and syncs
anyway. `Emulator::launch` becomes "build the command" so either side can run
it.

**Launchers.** A browser or PWA can't start processes, so in Game Mode it is
stuck with the daemon starting the emulator. An Electron launcher can run
`rommd play --attach` as a child (or use the same prepare-and-hand-back API);
if it is the Steam shortcut itself, it must stay alive (hidden) while the
game runs. Cheapest in Game Mode is no launcher process at all: per-game Steam
shortcuts that call rommd directly, created by rommd or by a Steam ROM Manager
parser listing the RomM library (an idea, not decided).

## Emulator auto-detection

Find installed emulators and fill in `[emulators.*]`, so a new install works
without hand-written paths.

**Done:** the macOS scan (`src/detect.rs`) and `rommd detect [path]`, which
prints what it finds next to the config, for Dolphin, DuckStation, Ryujinx,
mGBA and Azahar (`Emulator::mac_app`): install folders, then Spotlight, or a
path the user names, with a hint to move apps found outside Applications. Not yet detected on macOS: Eden (bundle ID
`com.yuzu-emu.yuzu`, yuzu's, so also require the executable `eden`; data in
`~/.local/share/eden`, XDG paths even on macOS). The config editor
(`src/config_edit.rs`), `rommd detect --save`, and the first-run config (this
OS's examples only, with the emulators found in the install folders filled in)
are done too.

**Linux (done, `src/detect/linux.rs`, verified on CachyOS):** packages (root-owned
executables on `PATH`), Flatpak installations, AppImages in `~/Applications`,
`~/AppImages`, `~/.local/bin`, `~/bin` or launched by a desktop entry in
`~/.local/share/applications` (AppImageLauncher, Gear Lever, EmuDeck); the CLI
also looks in Desktop, Downloads, Documents and the user's own `PATH` dirs.
AppImages are identified by the app ID their squashfs root declares
(`<id>.desktop` or icon; gzip and zstd), uruntime's DwarFS images (Eden, which
packs its names) by `.upd_info`. A scan of a typical home takes well under
100 ms. Two emulators found for one platform (Eden and Ryujinx) get a
`[platforms]` hint in the first-run config and in `rommd detect`.

Not covered yet, roughly by likely demand:
- Eden's *stable* AppImages: identified only if they carry `.upd_info`
  pointing at `eden-emu.dev/` (the nightlies do); unchecked. A DwarFS image
  without it can't be identified, not even by `rommd detect <path>`.
- Ryujinx's tarball (`publish/Ryujinx`, used by older EmuDeck installs): no
  file in it names the app.
- RetroDECK: one Flatpak (`net.retrodeck.retrodeck`) running every emulator
  inside, data in `~/retrodeck`; the config's `binary` can't express
  `flatpak run --command=…`.
- Snaps, Dolphin's legacy `~/.dolphin-emu`, portable installs next to an
  extracted binary, xz/lzo squashfs.

**Windows (done, `src/detect/windows.rs`, verified on Windows 10):** programs
with the emulator's file name in the folders the uninstall entries name
(`HKCU`/`HKLM`, both registry views), `Program Files`,
`%LOCALAPPDATA%\Programs` (two levels down), winget packages, Scoop's
`apps\*\current`, and the targets of `.lnk` shortcuts in the Start Menu and on
the Desktop; the CLI and setup also look in Desktop, Downloads and Documents
(three levels down, for zips extracted into a folder named after the zip).
Everything except Desktop, Downloads, temp and the Recycle Bin counts as
installed, as suggested: most Windows emulators ship as zips with no standard
place. A scan takes about 0.25 s on a machine with ~190 uninstall entries.

What the real builds turned out to carry (checked October 2026):
- Version resource (`ProductName`): mGBA (`mGBA`, Inno Setup installer and
  7z), DuckStation (`DuckStation`; its updater and uninstaller say so too, so
  the file name `duckstation-qt*.exe` matters). **Dolphin, Azahar and Eden
  ship none**, so the planned `ProductName` check alone would miss them; they
  are identified by their project's web address inside the program
  (`dolphin-emu.org`, `azahar-emu.org`, `eden-emu.dev`; yuzu builds don't
  carry Eden's). No embedded manifest either.
- Data folders, from each emulator's source: Dolphin
  (`UICommon::SetUserDirectory`): `portable.txt` or registry
  `LocalUserConfig` → `User\` next to the exe, else registry `UserConfigPath`
  (Dolphin writes it, with mixed slashes and a trailing slash), else
  `Documents\Dolphin Emulator` if it exists, else `%APPDATA%\Dolphin
  Emulator`. DuckStation: `portable.txt` or `settings.ini` next to the exe,
  else `Documents\DuckStation` if it exists, else
  `%LOCALAPPDATA%\DuckStation`. Eden and Azahar: a `user\` folder next to the
  exe, else `%APPDATA%\eden` / `%APPDATA%\Azahar`. mGBA: `portable.ini` next
  to the exe, else `%APPDATA%\mGBA`.
- winget: DuckStation is a portable zip package
  (`%LOCALAPPDATA%\Microsoft\WinGet\Packages\Stenzek.DuckStation_…`, an
  `HKCU` uninstall entry, a link in `WinGet\Links`); mGBA and Azahar are their
  installers; Dolphin is only the 2016 5.0 installer; Ryujinx and Eden aren't
  there. Dolphin's own download is a 7z that people extract by hand (no
  uninstall entry, no shortcut).

Not covered yet:
- **Ryujinx:** its servers (`update.ryujinx.app`, `git.ryujinx.app`) were
  down, so no build could be inspected and it has no `windows_app`. Not on
  winget. Scoop's `games` bucket has `ryujinx` (Ryubing) and
  `ryujinx-canary`, downloading from the same server; its manifest confirms
  the zip layout (`extract_dir = publish`, so `publish\Ryujinx.exe`) and that
  Scoop runs it portable (`persist = portable`, moving an existing
  `%APPDATA%\Ryujinx` into `apps\ryujinx\current\portable`). Left to check:
  what proves the program is Ryujinx (the .NET apphost's version resource,
  `ProductName` probably `Ryujinx`), then add `windows_app` with
  `Portable::Folder("portable")` and `AppData::Roaming("Ryujinx")`.
- Real installs not tried: Scoop packages, DuckStation's own installer,
  Dolphin's 5.0 installer from winget, Dolphin's `LocalUserConfig` set.
- EmuDeck for Windows' layout.
- `install.ps1` on Windows 10 with `rommd setup` adding what it finds: the
  test that matters, still to do.

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
  write both plus a comment naming the one used and the `[platforms]` line
  that picks the other (done: `detect::shared_platforms`, in the first-run
  config and `rommd detect`; `--save` appends without it). Detection lists
  emulators in `emulator::all()` order, so Ryujinx is the default.
- Flatpak has no single binary; the exported launcher
  (`/var/lib/flatpak/exports/bin/<app id>`, `~/.local/share/flatpak/exports/bin`)
  is used as `binary`. Checked with a non-emulator Flatpak: `flatpak run` execs
  `bwrap`, so the PID rommd spawns lives as long as the sandbox, the sandboxed
  processes stay in rommd's process group, and stopping the group ends them.
  **Untested:** an actual emulator Flatpak through launch and post-exit sync
  (planned on a Steam Deck), and ROM access: the sandbox only sees what its
  `filesystems` grant, so `rommd detect` prints the `flatpak override` that
  opens `roms_dir`. A launch could check the same (`Sandbox::can_read`) and
  say why the emulator found no ROM.

**Where to look:** macOS `/Applications/*.app` and `~/Applications`,
identified by the bundle identifier in `Contents/Info.plist`; Linux packages
(`/usr/bin`), Flatpak exports, AppImages (`~/Applications`); Windows as above.

## Guided setup: `rommd setup`, run by the install scripts

**Done** as planned below, except: emulators are confirmed all at once, not
one by one. Windows detection now exists too (see "Emulator auto-detection"),
so setup adds emulators there as well. The install scripts are untested on a
real install (Windows PowerShell 5 especially).

Today the installer drops the binary, installs the service, and the daemon
writes a default config on its first start; the user then has to find
`config.toml` and fill in the RomM URL and credentials by hand, and nothing
works until they do. Instead, an interactive `rommd setup` that the install
scripts call before `service install`.

**In the binary, not the scripts.** One implementation instead of one in
`install.sh` and one in `install.ps1`; it reuses `default_config`,
`config_edit` and detection; it can check answers against RomM; package
manager users (brew, AUR) and anyone changing their mind later run it
themselves.

**Flow** (like rustup: "proceed with the standard setup / customize / cancel"):
- Quick: RomM URL and API token (hidden input), checked against the server
  before anything is written ("connected to RomM 5.2.0 as …", or why not:
  not a RomM server, token refused, unreachable). Then detected emulators,
  each confirmed. Interactive, so detection may look everywhere (Spotlight,
  Downloads, `detect(…, true)`), unlike the daemon's unattended first start.
  The URL also matters for the userscript, whose origin rommd checks against
  `romm.base_url`.
- Customize: also the ROM directory and the update mode (auto / notify /
  off); later `[platforms]` when two emulators share one. Port stays a file
  edit.
- An existing config is not overwritten: setup offers to change its RomM
  settings and add emulators through `config_edit`. The file holds the token,
  so it is written owner-only (0600 on Unix).

**Install scripts:**
- `curl … | sh` feeds the script on stdin, so prompts read from `/dev/tty`;
  with no terminal (CI, provisioning) setup is skipped and the install works
  as today. `install.ps1` (`irm | iex`) can use `Read-Host`.
- Unattended installs: `-y`, or `ROMMD_URL` / `ROMMD_TOKEN` in the
  environment.
- Re-running the installer to upgrade doesn't ask again when a config exists
  (at most "reconfigure? [y/N]").
- Steam Deck: installed from Desktop Mode in Konsole, so interactive works.

**Later: RomM's device registration / pairing** instead of pasting a token,
if RomM's flow suits a local daemon (not looked into yet). Related:
"Register rommd as a RomM device" above.

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

