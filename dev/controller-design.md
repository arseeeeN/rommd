# Controller System Design

Captured from design discussion. Covers the model, the reasoning behind key decisions,
and what is explicitly deferred.

---

## The Three-Layer Model

```
Game Profile (RomM)
    └── slot assignments: slot N → controller profile name
            │
            ▼
Controller Profile (RomM)
    └── button layout for an emulator's button namespace
        no device expression — portable across machines
            │
            ▼
Physical Device (local rommd config)
    └── the actual hardware: device expression, identifier, friendly name
        machine-specific, never stored in RomM
```

### Why three layers and not two

We started with a simpler two-layer model (controller profile = device expression + button
mappings bundled together). This was rejected because of the Kings Field case: you want a
"Kings Field attack layout" profile that any friend can apply to whatever controller they
own, without needing a separate profile per physical device. The button layout has to be
separable from the physical device.

### Why the physical device layer stays local

Two reasons that point in the same direction:

1. **Device expressions are emulator-specific.** Dolphin uses `Bluetooth/0/Nintendo RVL-CNT-01`,
   DuckStation uses `SDL-0/Button 0`, RetroArch has its own system. There is no unified
   cross-emulator device syntax, and there never will be. A "physical device" entity in RomM
   would need to know about every emulator's syntax to be useful.

2. **Device expressions are partially machine-local.** SDL indices (`SDL/0/...`, `SDL/1/...`)
   depend on connection order and change between sessions. MAC addresses and VID/PID are
   stable identifiers, but translating them to an emulator's live device expression requires
   querying the OS at launch time.

These two constraints together mean physical device config belongs in rommd's local config,
not in RomM.

---

## Physical Controller Identification

Three categories with different identifier schemes:

### Generic Bluetooth controllers
- Identified by **MAC address** (stable, unique across connections)
- Works with any emulator via the SDL/HID layer
- The emulator config needs an SDL device expression (`SDL/0/DualSense`)
  which is connection-order dependent — rommd must enumerate connected SDL devices
  at launch time and match by name or MAC
- Store both MAC (identity) and expected SDL device name (matching) in local config

### USB controllers
- Identified by **VID + PID + Serial** (Serial disambiguates multiple identical devices)
- Same SDL translation challenge as Bluetooth

### Emulator-specific controllers
- **Real Wiimotes**: Dolphin manages its own Bluetooth stack, bypassing the OS.
  Device expression: `Bluetooth/0/Nintendo RVL-CNT-01-TR`. Universal — same on any machine
  where Dolphin is set up. Dolphin speaks directly to the Wiimote, not via OS BT devices.
- **DSU protocol devices** (phones as Wiimotes, Steam Deck gyro, etc.):
  Device expression: `DSUClient/0/0` (logical slot number). Universal — slot 0 is slot 0
  on any machine. The user configures DSU in Dolphin/the DSU server once.
- **Wii U Gamepad**: supported via DSU/Cemuhook in Dolphin, same slot-based addressing.

---

## Local Config Structure

```toml
[controllers.dualshock_mine]
name       = "My DualShock 4"
type       = "bluetooth"
mac        = "AA:BB:CC:DD:EE:FF"
sdl_name   = "DualSense Wireless Controller"   # used for live matching at launch

[controllers.xbox_living_room]
name       = "Living Room Xbox"
type       = "usb"
vid        = "045E"
pid        = "02EA"
serial     = "..."

[controllers.wiimote_left]
name       = "Wiimote Left"
type       = "dolphin_wiimote"
device_expr = "Bluetooth/0/Nintendo RVL-CNT-01-TR"

[controllers.phone_arseen]
name       = "Arseen's Phone (DSU)"
type       = "dsu"
device_expr = "DSUClient/0/0"
```

---

## Where Things Live

| Entity | Storage | Reason |
|---|---|---|
| Controller profile (button layout) | RomM | Portable, shareable, no machine-specific data |
| Game profile (slot assignments + overlay) | RomM | Per-game, portable |
| Physical controller registry | Local rommd config | Machine-specific device expressions and identifiers |

---

## The `/controllers` Endpoint

`GET /controllers` returns the locally configured controller list so the frontend can build
slot-assignment UI without knowing what's in the local config file.

Two important states per controller:
- **Configured** — exists in local config
- **Connected** — physically present right now (plugged in / paired)

The frontend should show both: full configured list, with connected/disconnected indication.
This lets you build a game session even when not all controllers are currently attached,
and greyed-out entries make the state obvious.

---

## Game Session Flow

1. User opens game in frontend, picks controller assignments: "slot 1 → `wiimote_left`,
   slot 2 → `phone_arseen`"
2. Frontend sends launch request with slot assignments + selected controller profiles
3. rommd resolves controller profiles from RomM (button layouts)
4. rommd resolves physical devices from local config (device expressions)
5. For generic SDL controllers, rommd enumerates connected devices at launch time
   and matches by MAC/name to get the live SDL index
6. rommd composes the emulator-specific config files (`WiimoteNew.ini`, `GCPadNew.ini`,
   DuckStation input config, etc.) and writes them to the temp config dir
7. Emulator launches with correct controllers, no manual setup required

---

## RetroArch Core Naming

Separate topic, captured here for completeness.

API field: `"emulator": "retroarch:melonds"` — the emulator field encodes both the
frontend and the core. The handler splits on `:`, looks up `retroarch` in the emulators
map, passes the core name as a separate argument.

Per-platform core defaults in rommd config avoid requiring explicit core every call:
```toml
[emulators.retroarch.cores]
nds = "melonds"
gba = "mgba"
snes = "snes9x"
```

Explicit core in the API always wins over the default.

---

## UX Path for Non-Technical Users

The configuration overhead is real but solvable:
- Auto-detect newly connected controllers and prompt to name and register them
- Tray context menu for quick access to controller setup
- Frontend forms for creating and editing controller profiles in RomM
- RomM already has a controller debug interface (added in a recent release) that could
  be adapted for creating/editing profiles in the browser

Initial setup is one-time per device. After that, launching with the right controllers
is just picking from a list.

---

## What Is Explicitly Deferred

- Controller discovery / auto-registration (enumerate connected devices)
- MAC-to-SDL live matching at launch time
- The `/controllers` endpoint implementation
- Controller profile storage in RomM (pending RomM API extensions — owner plans to add
  game profile + controller profile endpoints to their RomM fork)
- Full controller system implementation for Dolphin

**Current plan:** implement Dolphin basic support first (launch, save sync, per-game INI
overlay) with manual/stubbed controller config. Layer the controller system on top once
Dolphin is working and we have seen how it consumes the config in practice.
