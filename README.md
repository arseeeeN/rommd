# rommd

rommd is a small daemon that connects [RomM](https://github.com/rommapp/romm),
the self-hosted game library manager, with the emulators on your computer. A
frontend asks rommd to launch a game through its local [HTTP API](#api). rommd
downloads the ROM, restores your latest save from RomM, starts the emulator,
and uploads your progress when you quit.

> **Status:** early and not yet released. rommd has no user interface of its
> own, and RomM's web UI doesn't call it yet, so for now games are launched
> through the API (for example with `curl`).

- **Launch from your library**: ROMs are downloaded on demand and cached.
- **Save sync that doesn't lose progress**: every save is compared against
  what was last synced, so rommd can tell "changed here" from "changed
  elsewhere" from "changed on both". Real conflicts keep the newer version and
  back up the other one.
- **Version history**: RomM keeps the last 10 versions of each game's save.
- **Cross-emulator saves**: a Switch save made in Ryujinx restores into Eden and
  the other way round; a GBA save made in RomM's in-browser player continues in
  desktop mGBA.
- **Compatible with [argosy-launcher](https://github.com/rommapp/argosy-launcher)**:
  saves use the same formats, so they move between desktop and Android.
- **Updates itself**: signed releases, installed while no game is running.

## Supported emulators

| System | Emulator | ID | Saves | Save states |
|---|---|---|---|---|
| Nintendo Switch | [Eden](https://eden-emu.dev) | `eden` | ✓ (per user and per console) | — |
| Nintendo Switch | [Ryujinx](https://ryujinx.app) | `ryujinx` | ✓ (per user and per console) | — |
| Nintendo 3DS | [Azahar](https://azahar-emu.org) | `azahar` | ✓ (save data and extdata) | ✓ |
| GameCube / Wii | [Dolphin](https://dolphin-emu.org) | `dolphin` | ✓ | ✓ |
| Game Boy Advance | [mGBA](https://mgba.io) | `mgba` | ✓ | ✓ |
| PlayStation | [DuckStation](https://www.duckstation.org) | `duckstation` | ✓ (memory cards) | ✓ |

The ID is the emulator's name in the config file (`[emulators.<id>]`) and in
launch requests. Things to know:

- **Switch:** only the active user's saves are synced. rommd identifies a game
  by its title ID, read from the NSP. Most NSPs work as they are; ones without
  a ticket need `prod.keys` in the emulator's keys folder. Ryujinx only creates
  a game's save folder the first time it boots the game, so on a fresh install
  your RomM save is restored from the second launch on.
- **GameCube:** memory card slot A must be a GCI folder (Dolphin's default).
  A raw memory card image holds every game's saves in one file and is not
  synced.

## Install

**Linux and macOS**

```sh
curl -fsSL https://github.com/arseeeeN/rommd/releases/latest/download/install.sh | sh
```

**Windows** (PowerShell)

```powershell
irm https://github.com/arseeeeN/rommd/releases/latest/download/install.ps1 | iex
```

The script installs the latest release (to `~/.local/bin` or
`%LOCALAPPDATA%\Programs\rommd`), checks it against the release checksums,
and registers rommd to start when you log in. Run it again to reinstall.

Prefer to do it by hand? Download `rommd-<target>` (`.exe` on Windows) for
your platform from the [releases page](https://github.com/arseeeeN/rommd/releases)
and optionally verify it with [minisign](https://jedisct1.github.io/minisign/)
(`minisign -Vm rommd-<target> -p minisign.pub`). Rename it to `rommd`
(`rommd.exe`; on Linux and macOS also `chmod +x` it) and move it to a folder
it can stay in, such as `~/.local/bin` or `%LOCALAPPDATA%\Programs\rommd`.
Then run `rommd service install`. It registers the binary where it is, so
moving or deleting it later stops rommd from starting at login.

**From source:** install [Rust](https://rustup.rs), then

```sh
cargo install --git https://github.com/arseeeeN/rommd
rommd service install
```

Builds from source embed the release signing key too, so they update
themselves to the latest release like an installed binary. To keep running
your own build, set `mode = "off"` under `[updates]`.

## Configure

rommd creates a commented config file on first start:

| Platform | Config file |
|---|---|
| Linux | `~/.config/rommd/config.toml` |
| macOS | `~/Library/Application Support/rommd/config.toml` |
| Windows | `%APPDATA%\rommd\config.toml` |

A minimal setup:

```toml
[romm]
base_url = "https://romm.example.com"
api_token = "..."            # RomM → Settings → API Tokens

[emulators.dolphin]
binary     = "/usr/bin/dolphin-emu"
config_dir = "~/.local/share/dolphin-emu"   # the emulator's user/data folder

[emulators.eden]
binary     = "~/Applications/Eden.AppImage"
config_dir = "~/.local/share/eden"
```

On Windows, write paths in single quotes, since a backslash in double quotes
starts an escape sequence:

```toml
[emulators.dolphin]
binary     = 'C:\Program Files\Dolphin\Dolphin.exe'
config_dir = '%APPDATA%\Dolphin Emulator'
```

`config_dir` is the folder where the emulator keeps its saves. Where an
emulator splits its settings into a separate folder (Dolphin, Eden and Azahar
on Linux), rommd finds them on its own. At startup rommd warns in its log when
a `config_dir` does not look like that emulator's folder and says where it
usually is. The generated config file has examples for each platform.
Restart rommd after editing the config.

## Running

`rommd service install` starts rommd at login: a systemd user service on
Linux and a launch agent on macOS (both restart it if it stops), a logon task
on Windows (no administrator rights needed; it runs without a console window).
`rommd service uninstall` removes it. Running `rommd` with no command runs it
in the foreground.

Logs: `journalctl --user -u rommd -f` on Linux, `~/Library/Logs/rommd.log` on
macOS, `%LOCALAPPDATA%\rommd\rommd.log` on Windows.

## Updates

```toml
[updates]
mode = "auto"    # auto | notify | off
```

- `auto` (default): checks daily and installs new releases while no game is
  running, then restarts.
- `notify`: checks daily and reports new versions in the log and API.
- `off`: no background checks.

Every release binary is signed. rommd only installs an update whose signature
matches the key built into it, for exactly that version and platform.

## API

rommd listens on `http://localhost:7497`. The full API is described by
`GET /openapi.json`. The main endpoints:

| Endpoint | |
|---|---|
| `POST /launch` | Download (if needed), sync, and launch `{rom_id, emulator}` |
| `GET /status` | Running games |
| `POST /stop` | Stop a game (`{pid}`) or all games |
| `GET /emulators` | Known emulators and whether they are configured |
| `GET /saves/conflicts` | Recent save conflicts and where the other version was kept |
| `GET /update`, `POST /update` | Update status (`?check=true` checks now); install the latest release |

To launch a game by hand, take its ID from the game's URL in RomM
(`…/rom/123`):

```sh
curl -X POST localhost:7497/launch -H 'Content-Type: application/json' \
  -d '{"rom_id": 123, "emulator": "dolphin"}'
```

**Security**: rommd only accepts connections from your own computer. Web pages
may only call it from your RomM server's address (`romm.base_url`). Allow other
frontends with `[server] allowed_origins = ["https://…"]`.

## Your data

rommd keeps downloaded ROMs (`roms/`, or `roms_dir` in the config), its sync
history and save backups in one folder:

| Platform | Folder |
|---|---|
| Linux | `~/.local/share/rommd/` |
| macOS | `~/Library/Application Support/rommd/` |
| Windows | `%LOCALAPPDATA%\rommd\` |

Before rommd replaces a save on your computer, it copies the old one to
`save-backups/` (the last 5 per game are kept).

## Development

```sh
cargo test     # runs offline against a built-in fake RomM
cargo run      # runs the daemon in the foreground
```

`CLAUDE.md` describes the architecture and the reasoning behind the save sync.
`dev/releasing.md` covers cutting a release.
