// ==UserScript==
// @name         rommd: Launch button
// @namespace    https://github.com/arseeeeN/rommd
// @version      0.4.0
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

  // Which emulator starts a game is rommd's choice: the one configured for
  // the platform, or [platforms] in its config.toml when there are several.

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

  // A refused origin looks like a network error to the page, since rommd's
  // refusal carries no CORS headers. A no-cors request still completes when
  // rommd is running, which tells the two apart.
  async function whyUnreachable() {
    try {
      await fetch(ROMMD + "/health", { mode: "no-cors" });
    } catch (_) {
      return (
        "rommd is not reachable at " + ROMMD + ". Is it running? Opening " +
        ROMMD + "/health in a tab should show its status. In Chrome and " +
        "Edge, also check that this site may access your local network."
      );
    }
    return (
      "rommd is running but refused this page. Set romm.base_url in rommd's " +
      "config.toml to " + location.origin + " (or add it to " +
      "server.allowed_origins), then restart rommd."
    );
  }

  async function rommd(path, options) {
    try {
      return await fetch(ROMMD + path, options);
    } catch (_) {
      throw new Error(await whyUnreachable());
    }
  }

  // Starts the game. Resolves to null once it runs, or to a launch ID while
  // rommd downloads the ROM first (it then starts the game by itself).
  async function launch(romId) {
    const res = await rommd("/launch", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ rom_id: romId }),
    });
    if (!res.ok) throw new Error(await errorText(res));
    return res.status === 202 ? (await res.json()).launch_id : null;
  }

  async function launchState(id) {
    const res = await rommd("/launches/" + id);
    if (!res.ok) throw new Error(await errorText(res));
    return res.json();
  }

  // This game's launch that is still fetching or starting, if any, so a
  // button made after navigating back picks up the download.
  async function launchInProgress(romId) {
    try {
      const res = await fetch(ROMMD + "/status");
      if (!res.ok) return null;
      const active = ["preparing", "downloading", "starting"];
      const launch = (await res.json()).launches.find(
        (l) => l.rom_id === romId && active.includes(l.state)
      );
      return launch ? launch.id : null;
    } catch (_) {
      return null;
    }
  }

  function sleep(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }

  function size(bytes) {
    if (bytes >= 1e9) return (bytes / 1e9).toFixed(1) + " GB";
    if (bytes >= 1e6) return Math.round(bytes / 1e6) + " MB";
    return Math.round(bytes / 1e3) + " kB";
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
    let label = null;
    const walker = document.createTreeWalker(button, NodeFilter.SHOW_TEXT);
    for (let n = walker.nextNode(); n; n = walker.nextNode()) {
      if (/^\s*(Play|Copy download link)\s*$/.test(n.nodeValue)) {
        n.nodeValue = n.nodeValue.replace(/Play|Copy download link/, "Launch");
        label = n;
      }
    }
    const setLabel = (text) => {
      if (label) label.nodeValue = label.nodeValue.replace(/\S.*\S|\S/, text);
    };
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

    const reset = (mdi) => {
      delete button.dataset.busy;
      delete button.dataset.launchId;
      setLabel("Launch");
      button.title = "Launch on this PC";
      setIcon(mdi);
      if (mdi !== ICON) setTimeout(() => setIcon(ICON), 3000);
    };
    const fail = (e) => {
      console.error("rommd:", e);
      reset("mdi-alert-circle-outline");
      alert("rommd: " + e.message);
    };

    // Shows the download on the button until the game runs. rommd keeps
    // going if the page is left; coming back picks it up again.
    const follow = async (id) => {
      button.dataset.busy = "1";
      button.dataset.launchId = String(id);
      setIcon("mdi-download");
      for (;;) {
        let l;
        try {
          l = await launchState(id);
        } catch (e) {
          return fail(e);
        }
        if (button.dataset.launchId !== String(id)) return;
        if (l.state === "downloading") {
          const pct = l.total_bytes ? Math.floor((l.downloaded_bytes * 100) / l.total_bytes) : null;
          setLabel(pct === null ? size(l.downloaded_bytes) : pct + "%");
          button.title =
            "Downloading " + l.name + ": " + size(l.downloaded_bytes) +
            (l.total_bytes ? " of " + size(l.total_bytes) : "") + ". Click to cancel.";
        } else if (l.state === "starting") {
          setLabel("Launch");
          setIcon("mdi-loading", true);
          button.title = "Starting " + l.name;
        } else if (l.state === "started") {
          return reset("mdi-check");
        } else if (l.state === "failed") {
          return fail(new Error(l.error));
        } else if (l.state === "cancelled") {
          return reset(ICON);
        }
        await sleep(1000);
      }
    };

    button.addEventListener("click", async (event) => {
      event.preventDefault();
      event.stopPropagation();
      const downloading = button.dataset.launchId;
      if (downloading) {
        if (!confirm("Cancel the download?")) return;
        const res = await rommd("/launches/" + downloading, { method: "DELETE" }).catch(fail);
        if (res && res.status === 409) alert("rommd: the download had finished; the game is starting.");
        return;
      }
      if (button.dataset.busy) return;
      button.dataset.busy = "1";
      setIcon("mdi-loading", true);
      try {
        const id = await launch(romId);
        if (id === null) reset("mdi-check");
        else follow(id);
      } catch (e) {
        fail(e);
      }
    });
    launchInProgress(romId).then((id) => {
      if (id !== null && !button.dataset.busy) follow(id);
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
