# What rommd would like from RomM

Changes to [RomM](https://github.com/rommapp/romm) that would make rommd (and
other clients, argosy-launcher among them) better. Each stands alone; none
blocks rommd, which works around all of them today. Checked against RomM
master `58e2fdb` (2026-11-03).

## 1. Steam artwork through RomM's SteamGridDB key

**Today.** RomM talks to SteamGridDB with a key set on the server
(`STEAMGRIDDB_API_KEY`), but exposes only `GET /api/search/cover?search_term=`,
which feeds its own cover picker: grids in cover sizes (600x900, GOG tiles,
squares; `backend/handler/metadata/sgdb_handler.py`, `_get_game_covers`).
Heroes, logos, icons and the wide 460x215 / 920x430 grids are never fetched,
and the key never leaves the server, as it shouldn't.

**So** every rommd user who wants Steam shortcuts with proper art (banner,
hero, logo, icon) needs a SteamGridDB key of their own in rommd's config.

**Proposed.** Artwork for a ROM by kind, through the server's key, for the
game RomM already matched (`rom.sgdb_id`):

```
GET /api/roms/{id}/sgdb/{kind}     kind: grid_tall | grid_wide | hero | logo | icon
  -> [{ "url": "...", "width": 920, "height": 430, "mime": "image/png" }, ...]
```

Filters as Steam ROM Manager uses by default: static only, no NSFW, no humor,
locked images (DMCA placeholders) skipped, which `_get_game_covers` already
does. The grids build on `iter_grids_for_game`
(`backend/adapters/services/steamgriddb.py`); heroes, logos and icons need
new calls there, since the service only knows grids today (SteamGridDB API:
`/heroes/game/{id}`, `/logos/game/{id}`, `/icons/game/{id}`, same filters).
`404` when the ROM has no `sgdb_id` or SteamGridDB is disabled. rommd would then ask RomM first and use its own key
only as a fallback.

## 2. A lighter ROM listing

**Today.** `GET /api/roms` returns each ROM with its full metadata (IGDB,
ScreenScraper, LaunchBox, HLTB, RetroAchievements blocks, …): 6.5 MB for a
345-ROM library on a small server, tens of MB for thousands of ROMs. rommd
needs a handful of fields per ROM to build its Steam shortcuts and tell what
is downloaded.

**Proposed.** Either a `fields=` parameter limiting the response to the named
fields, or a slim schema (`?schema=simple`) with: `id`, `name`, `fs_name`,
`fs_size_bytes`, `has_multiple_files`, `platform_id`, `platform_slug`,
`platform_fs_slug`, `platform_display_name`, `path_cover_large`,
`merged_screenshots`, `sgdb_id`, `updated_at`.

## 3. A Launch button on the game page

**Today.** rommd ships a userscript (`contrib/rommd-button.user.js`) that adds
a Launch button next to Play on RomM's game pages and calls rommd's local API
(`POST http://localhost:7497/launch`). RomM's own UI has no notion of an
external launcher.

**Proposed.** A user setting for an external launcher URL (default unset);
when set, the game page shows a Launch button that POSTs `{"rom_id": N}` to
it and follows the `202` launch (`GET /launches/{id}`: progress, cancel), as
the userscript does. rommd already accepts requests from the RomM server's
origin (`server/security.rs`). The userscript retires.

## Notes for the conversation

- rommd: <https://github.com/arseeeeN/rommd>. Its sync is compatible with
  argosy-launcher's (Sigil units and argosy's older shapes).
- RomM 5.5's snapshots API is where argosy's saves go on new servers; rommd
  supporting it is its own project, not a request to RomM.
