// SPDX-License-Identifier: GPL-3.0-only
// Shared helpers for the OBS dock, overlay and setup pages.
"use strict";

const OD = (() => {
  const params = new URLSearchParams(location.search);
  const token = document.querySelector('meta[name="opendelay-token"]')?.content ?? "";
  let strings = {};

  // ?lang=<code>, otherwise the browser language, otherwise English.
  async function loadLanguage() {
    const wanted = [params.get("lang"), ...(navigator.languages || [])]
      .filter(Boolean)
      .map((l) => l.toLowerCase().split(/[-_]/)[0])
      .filter((l) => /^[a-z]{2,3}$/.test(l));
    for (const code of [...wanted, "en"]) {
      const r = await fetch(`/assets/lang/${code}.json`).catch(() => null);
      if (r && r.ok) {
        strings = await r.json();
        document.documentElement.lang = code;
        break;
      }
    }
    document.querySelectorAll("[data-t]").forEach((el) => (el.textContent = t(el.dataset.t)));
    document.querySelectorAll("[data-t-label]").forEach((el) => el.setAttribute("aria-label", t(el.dataset.tLabel)));
  }

  // t("key", a, b) replaces {} placeholders in order.
  function t(key, ...args) {
    let i = 0;
    return (strings[key] ?? key).replace(/\{\}/g, () => String(args[i++] ?? ""));
  }

  function duration(seconds) {
    const s = Math.round(seconds);
    if (s >= 60 && s % 60 === 0) return t("min", s / 60);
    if (s >= 60) return t("min_s", Math.floor(s / 60), s % 60);
    return t("s", s);
  }

  async function status() {
    const r = await fetch("/api/status", { cache: "no-store" });
    if (!r.ok) throw new Error(r.statusText);
    return (await r.json()).status;
  }

  async function act(action, extra = {}) {
    const r = await fetch("/api/action", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-OpenDelay-Token": token },
      body: JSON.stringify({ action, ...extra }),
    });
    return r.json();
  }

  // Status is pushed over the WebSocket right after every change. Without it,
  // poll every second, every 150 ms for 3 s after a command, and back off to
  // every 5 s while OpenDelay does not answer.
  function poll(render) {
    let fastUntil = 0;
    let timer = 0;
    let failures = 0;
    let ws = null;
    let wsRetryAt = 0;
    function connect() {
      const s = new WebSocket(`ws://${location.host}/ws`);
      let pushed = false;
      ws = s;
      s.onopen = () => s.send(JSON.stringify({ action: "subscribe" }));
      s.onmessage = (e) => {
        const m = JSON.parse(e.data);
        if (m.type === "status") {
          pushed = true;
          render(m.status, null);
        }
      };
      s.onclose = () => {
        if (ws !== s) return;
        ws = null;
        // A socket that never delivered a status is not retried for a while.
        if (!pushed) wsRetryAt = Date.now() + 10000;
        tick();
      };
    }
    async function tick() {
      clearTimeout(timer);
      if (ws) return;
      try {
        render(await status(), null);
        failures = 0;
        if (Date.now() >= wsRetryAt && typeof WebSocket === "function") connect();
      } catch (e) {
        render(null, e);
        failures++;
      }
      if (ws) return;
      const wait = failures ? Math.min(5000, 1000 * 2 ** (failures - 1)) : 1000;
      timer = setTimeout(tick, Date.now() < fastUntil ? 150 : wait);
    }
    tick();
    return () => {
      fastUntil = Date.now() + 3000;
      tick();
    };
  }

  return { params, t, duration, status, act, poll, loadLanguage };
})();
