# Adding an emulator

The steps in the order that avoids rework. PCSX2 (`src/emulator/pcsx2.rs`)
is a compact recent example; DuckStation is the closest full one (saves,
states, profiles).

## 1. Find out what argosy-launcher does

rommd keeps saves compatible with [argosy-launcher](https://github.com/rommapp/argosy-launcher),
RomM's Android client, so a save made on one moves to the other. Decide
nothing about a system's saves before reading how argosy stores them; its
choice is the format RomM already holds for that system.

```sh
git clone https://github.com/rommapp/argosy-launcher ../argosy-launcher
git -C ../argosy-launcher submodule update --init sigil   # not checked out by default
```

argosy's save rules live in **Sigil** (`sigil/`, a C library argosy links
in), not in its Kotlin code. Read, for the system:

- `sigil/docs/platforms/<system>.md`: the game ID it reads from the ROM,
  the emulator layouts it knows, and the unit that goes to RomM.
- `sigil/docs/sync.md`: unit shapes per system, and the older shapes a
  restore must still accept because argosy uploaded them before Sigil.
- `sigil/docs/save-units.md`: artifact (file) names and the hash.
- `docs/save-id-to-path.md` (argosy itself): how the Kotlin side places
  saves, per system.
- `app/src/main/kotlin/com/nendo/argosy/data/emulator/SavePathRegistry.kt`:
  argosy's emulator IDs. Use the same ID (it is RomM's `emulator` field).

Then compare with rommd's `SaveSet` kinds (`src/emulator/layout.rs`):
files (`Files`), a set of files zipped flat (`Bundle`) or directory trees
zipped under named roots (`Dir`). If argosy's unit is something else, e.g.
one game's saves taken out of a shared memory card (PS1, PS2, GameCube raw
cards), the sync engine needs a new kind first: ship the emulator without
those saves and add a `dev/backlog.md` entry, as PCSX2 did.

## 2. Read the emulator's source, then run it

Look up each fact in the source of a released version (link the tag, and
name the file in a doc comment as the existing modules do). You need:

- **Data folder** per OS and what moves it: portable markers next to the
  program, environment variables (XDG), a setting.
- **Settings file** and the keys that move the save, state and cache
  folders; relative values are usually inside the data folder.
- **File names** of saves and states, and anything else written beside them
  that must *not* sync (PCSX2's `.p2s.backup`).
- **Command line**: how to boot a file, the end-of-options marker (`--`),
  and whether the emulator quits when the game stops. The post-game sync
  runs when the process exits; PCSX2 needs `-batch`, Eden never quits by
  itself.
- **Identity for `rommd detect`**: macOS bundle ID (`Info.plist`); Linux
  Flatpak app ID, package executable, and the AppImage's root `.desktop`
  file or icon name (from its packaging script); Windows `ProductName` from
  the `.rc` file, or a URL inside the binary when there is no version
  resource.

Then install it, boot a game, make an in-game save and a save state, and
look at what appeared in the data folder.

## 3. Game ID

`game_id_for_rom` must read the ID from the ROM's contents, never its file
name. Existing readers: `ps_disc.rs` (PS1/PS2 serial, PS2 boot program
CRC), `switch.rs`, `n3ds.rs`, `dolphin.rs` (`read_disc`). Compressed images
(CHD, CSO) can't be read yet; an emulator's own game list cache can fill in
(DuckStation, PCSX2). Check the cache's version and give up on any other,
since its layout changes between releases.

## 4. Write the module

`src/emulator/<name>.rs`, implementing `Emulator` (`src/emulator/mod.rs`):
`id`, `display_name`, `platforms` (RomM's canonical slug), `command`,
`snapshot` (`bail!` until profiles are supported), `game_id_for_rom`,
`save_layout`, `save_family` (IDs whose saves this emulator can restore),
`config_dir_markers` and `config_dir_hint`, `mac_app`, `linux_app`,
`windows_app`. Detection types are in `src/detect/`; extend them when an
emulator's rules don't fit (PCSX2 added `AppData::Documents`).

Register it: `pub mod <name>` in `src/emulator/mod.rs`, and append it to
`emulator::all()`. Append, since that order is the order of a new config file.

For an OS you can't run, write detection from the source and packaging
scripts. Note where each fact came from, and say in the PR which OSes were
only checked against source.

## 5. Everything else that names emulators

- `src/config.rs`: `Os::examples` (macOS, Windows, Linux) and, for Windows
  portable installs, `examples_note`.
- `dev/config.example.toml`.
- `README.md`: the supported emulators table and its "Things to know".
- `CLAUDE.md`: a paragraph under Save sync on where saves and states live.
- `dev/backlog.md`: whatever is deferred.

## 6. Test

- A layout test against a fixture tree (`crate::testing::{TempDir, write}`):
  the folders, which files match and which don't.
- A `command` test (arguments, `--`), and tests for any binary format you
  parse (built in the test, as `ps_disc.rs` does).
- A new sync rule gets a scenario in `src/sync/scenarios.rs`.
- Against real data: `cargo run -- detect <path to the app or program>`,
  and a throwaway test that runs `game_id_for_rom` and `save_layout` on your
  own ROMs and data folder (delete it afterwards). Then launch a game
  through rommd and check the sync in the log.

CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`
and `cargo test`; run all three before committing.
