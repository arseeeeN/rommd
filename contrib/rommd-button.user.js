// ==UserScript==
// @name         rommd: Launch button
// @namespace    https://github.com/arseeeeN/rommd
// @version      0.3.0
// @description  Adds a Launch button to the action bar of RomM game pages that starts the game on this PC through the local rommd daemon.
// @match        https://romm.example.com/*
// @grant        none
// @run-at       document-idle
// ==/UserScript==

// A stopgap until RomM can launch games through local apps itself.
//
// Change @match above to your RomM address. rommd only accepts browser
// requests from the RomM server in its config (romm.base_url) or from
// server.allowed_origins. The button is placed next to the game page's
// "Play" button (or "Copy download link" when the platform has no browser
// player), found by their aria-labels; older RomM versions append the game
// name to those labels, and both forms are recognised.

(function () {
  "use strict";

  const ROMMD = "http://127.0.0.1:7497";

  // RomM platform slug -> rommd emulator id. Only emulators configured in
  // rommd's config.toml can launch.
  const EMULATORS = {
    gb: "mgba",
    gbc: "mgba",
    gba: "mgba",
    ngc: "dolphin",
    wii: "dolphin",
    "3ds": "azahar",
    psx: "duckstation",
    switch: "ryujinx", // or "eden"
  };

  const ICON = "mdi-rocket-launch";
  const MARK = "data-rommd-launch";

  console.info("rommd: Launch button script loaded");

  function currentRomId() {
    const m = location.pathname.match(/\/rom\/(\d+)\/?$/);
    return m ? Number(m[1]) : null;
  }

  async function errorText(res) {
    try {
      return (await res.json()).error || res.statusText;
    } catch (_) {
      return res.statusText || "HTTP " + res.status;
    }
  }

  async function launch(romId) {
    const romRes = await fetch("/api/roms/" + romId, { credentials: "include" });
    if (!romRes.ok) throw new Error("RomM: " + (await errorText(romRes)));
    const rom = await romRes.json();
    const emulator = EMULATORS[rom.platform_slug];
    if (!emulator) {
      throw new Error('no emulator is set for platform "' + rom.platform_slug + '" in the script');
    }
    let res;
    try {
      res = await fetch(ROMMD + "/launch", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ rom_id: romId, emulator: emulator }),
      });
    } catch (_) {
      throw new Error("rommd is not reachable at " + ROMMD + ". Is it running?");
    }
    if (!res.ok) throw new Error(await errorText(res));
  }

  // A button whose aria-label is `label`, alone or followed by the game name.
  function findButton(label) {
    return [...document.querySelectorAll("button[aria-label]")].find((b) => {
      const l = b.getAttribute("aria-label");
      return l === label || l.startsWith(label + " ");
    });
  }

  // The new button is a copy of the page's Play button (or copy-link
  // button), so it takes RomM's look and size; only the label, the icon and
  // the click differ.
  function makeButton(template, romId) {
    const button = template.cloneNode(true);
    button.setAttribute(MARK, "");
    button.removeAttribute("id");
    button.removeAttribute("aria-describedby");
    button.removeAttribute("disabled");
    button.classList.remove("v-btn--disabled");
    button.setAttribute("aria-label", "Launch");
    button.title = "Launch on this PC";
    const walker = document.createTreeWalker(button, NodeFilter.SHOW_TEXT);
    for (let n = walker.nextNode(); n; n = walker.nextNode()) {
      if (/^\s*(Play|Copy download link)\s*$/.test(n.nodeValue)) {
        n.nodeValue = n.nodeValue.replace(/Play|Copy download link/, "Launch");
      }
    }
    const icon = button.querySelector(".v-icon, .mdi");
    const setIcon = (mdi, spin) => {
      if (!icon) return;
      icon.className = icon.className
        .split(" ")
        .filter((c) => !c.startsWith("mdi-"))
        .concat(mdi, spin ? "mdi-spin" : [])
        .join(" ");
    };
    setIcon(ICON);
    button.addEventListener("click", async (event) => {
      event.preventDefault();
      event.stopPropagation();
      if (button.dataset.busy) return;
      button.dataset.busy = "1";
      setIcon("mdi-loading", true);
      try {
        await launch(romId);
        setIcon("mdi-check");
      } catch (e) {
        console.error("rommd:", e);
        setIcon("mdi-alert-circle-outline");
        alert("rommd: " + e.message);
      } finally {
        delete button.dataset.busy;
        setTimeout(() => setIcon(ICON), 3000);
      }
    });
    return button;
  }

  function ensureButton() {
    const romId = currentRomId();
    const existing = document.querySelector("[" + MARK + "]");
    if (romId === null) {
      if (existing) existing.remove();
      return;
    }
    if (existing && existing.dataset.romId === String(romId)) return;
    const anchor = findButton("Play") || findButton("Copy download link");
    if (!anchor) return;
    if (existing) existing.remove();
    const button = makeButton(anchor, romId);
    button.dataset.romId = String(romId);
    anchor.after(button);
  }

  // RomM is a single-page app that re-renders the bar as you navigate, so
  // keep checking that the button is there and belongs to this game.
  setInterval(ensureButton, 500);
})();
