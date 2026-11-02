# Emulator detection (`src/detect/`)

`rommd detect [path]` finds installed emulators on macOS, Linux and Windows.
Each is verified from its files only, never by running it, so the same check
can vet a path a client suggests. The daemon's first start looks only in the
install folders (it is unattended); the CLI and `rommd setup` also look in
Desktop, Downloads and Documents, and Spotlight on macOS.

Emulators are listed in `emulator::all()` order (so a new config's file
order, which picks between Ryujinx and Eden, is fixed); of several installs of
one emulator, one in an install folder wins, then one whose data dir is in use.

## macOS (`mac.rs`)

`.app` bundles in the install folders, then Spotlight (`mdfind` by bundle ID).
The bundle's `Info.plist` gives the identity (`Emulator::mac_app`: bundle ID,
plus the executable name for a fork that kept its parent's ID).

## Linux (`linux.rs`, `appimage.rs`)

- Root-owned executables on `PATH` (packages).
- Flatpak installations: `exports/bin/<app id>` as binary, data in
  `~/.var/app/<id>`, and the sandbox's `filesystems` grants, so `detect` can
  warn when `roms_dir` is out of reach.
- AppImages in `~/Applications`, `~/AppImages`, `~/.local/bin`, `~/bin` or
  launched by a desktop entry, recognised by the app ID their image declares
  (`appimage.rs` reads the squashfs root: `<id>.desktop` or icon) or, for
  DwarFS images, their `.upd_info`.
- Or a path the user names.

## Windows (`windows.rs`)

Programs with the emulator's file name in the folders uninstall entries name
(`HKCU`/`HKLM`), `Program Files` and `%LOCALAPPDATA%\Programs` (two levels
down), winget packages, Scoop's `apps\*\current`, and the targets of Start Menu
and Desktop `.lnk` shortcuts.

A program is identified by `ProductName` in its version resource (mGBA,
DuckStation, PCSX2) or, since Dolphin, Azahar and Eden ship none, by its
project's web address inside the file. Its data dir follows the emulator's own
rules (`WindowsDataDir`: portable marker next to the program, Dolphin's
registry values, a legacy Documents folder, then AppData, or Documents for
PCSX2). Everything but Desktop, Downloads and temp counts as installed, since
most Windows emulators ship as zips with no standard place.
