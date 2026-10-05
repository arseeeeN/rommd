# rommd

rommd is a small daemon that connects [RomM](https://github.com/rommapp/romm),
the self-hosted game library manager, with the emulators on your computer. A
frontend (such as RomM's web UI) asks rommd to launch a game. rommd downloads
the ROM, restores your latest save from RomM, starts the emulator, and uploads
your progress when you quit.

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

| System | Emulator | Saves | Save states |
|---|---|---|---|
| Nintendo Switch | Eden, Ryujinx | ✓ (per user and per console) | — |
| Nintendo 3DS | Azahar | ✓ (save data and extdata) | ✓ |
| GameCube / Wii | Dolphin | ✓ | ✓ |
| Game Boy Advance | mGBA | ✓ | ✓ |
| PlayStation | DuckStation | ✓ (memory cards) | ✓ |

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

Prefer to do it by hand? Download the binary for your platform from the
[releases page](https://github.com/arseeeeN/rommd/releases), optionally verify
it with [minisign](https://jedisct1.github.io/minisign/)
(`minisign -Vm rommd-<target> -p minisign.pub`), and run
`rommd service install`.

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

`config_dir` is the folder where the emulator keeps its saves. Where an
emulator splits its settings into a separate folder (Dolphin, Eden and Azahar
on Linux), rommd finds them on its own. See `dev/config.example.toml` for more
examples. Restart rommd after editing the config.

## Running

`rommd service install` starts rommd at login and restarts it if it stops: a
systemd user service on Linux, a launch agent on macOS, a logon task on
Windows. `rommd service uninstall` removes it. Running `rommd` with no command
runs it in the foreground.

Logs: `journalctl --user -u rommd -f` on Linux, `~/Library/Logs/rommd.log` on
macOS.

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

**Security**: rommd only accepts connections from your own computer. Web pages
may only call it from your RomM server's address (`romm.base_url`). Allow other
frontends with `[server] allowed_origins = ["https://…"]`.

## Your data

rommd keeps its sync history and backups next to each other:

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
