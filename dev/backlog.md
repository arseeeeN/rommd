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

**Proposed: create the slot the way Ryujinx does.**
- Allocate a new save data ID and create `bis/user/save/<id>/0/` and `1/`.
- Write `ExtraData0` / `ExtraData1` (SaveDataAttribute: program ID u64 at 0x00,
  user ID at 0x08, type u8 at 0x20 — 1 Account, 3 Device — plus owner ID, sizes).
- Add the entry to the save index `bis/system/save/8000000000000000/0/imkvdb.arc`
  (LibHac KVDB: `IMKV` header, `IMEN` entries keyed by the 0x40-byte
  SaveDataAttribute, value = SaveDataIndexerValue with the save data ID).
- Find out how the last published save data ID is persisted so new IDs don't
  collide.
- Pick the user ID from Ryujinx's active profile (`system/Profiles.json`) for
  Account saves.

**Needs.** Real Ryujinx files to build and verify against, e.g. a copy of
`bis/system/save/8000000000000000/` and one `bis/user/save/<slot>/` from an
existing install. Back up `imkvdb.arc` before every write, and refuse to write
if its format isn't the one we recognise. Keep the post-exit fallback.

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

