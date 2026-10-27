// SPDX-License-Identifier: GPL-3.0-only
// Script for the OBS dock (dock.html).
"use strict";
const $ = (id) => document.getElementById(id);
const TONES = { live: "var(--success)", delayed: "var(--accent-hover)", buffering: "var(--warning)",
                draining: "var(--success)", recovering: "var(--warning)", connecting: "var(--text-secondary)",
                offline: "var(--text-secondary)" };
let last = null;
let pendingDelay = null;
let commitTimer = 0;
let refreshNow = () => {};

function hms(ms) {
  const s = Math.floor(ms / 1000);
  return `${Math.floor(s / 3600)}:${String(Math.floor(s / 60) % 60).padStart(2, "0")}:${String(s % 60).padStart(2, "0")}`;
}

function render(st, err) {
  // Unreachable: grey out the last numbers and controls instead of showing them as current.
  document.body.classList.toggle("stale", !st);
  ["toggle", "golive", "minus", "plus", "overlay"].forEach((id) => { $(id).inert = !st; });
  if (!st) {
    $("pill").textContent = OD.t("unreachable");
    $("pill").style.setProperty("--tone", "var(--error)");
    return;
  }
  last = st;
  const d = st.delay;
  const active = st.connected && ["delayed", "buffering", "draining"].includes(d.state);
  $("pill").textContent = OD.t("state_" + st.state);
  $("pill").style.setProperty("--tone", TONES[st.state] || TONES.offline);
  $("digits").textContent = Math.ceil((active ? d.currentMs : d.configuredMs) / 1000);
  $("number").classList.toggle("active", active);
  $("caption").textContent = OD.t("caption_" + st.state, OD.duration(Math.ceil(d.currentMs / 1000)));
  $("toggle").textContent = active ? OD.t("disable") : OD.t("enable");
  $("toggle").disabled = !d.modeEnabled || d.state === "draining";
  $("golive").disabled = !active || d.state === "draining";
  if (pendingDelay === null) $("delay").textContent = OD.duration(d.configuredMs / 1000);
  const fill = d.bufferTargetMs ? Math.min(1, d.bufferFillMs / d.bufferTargetMs) : 0;
  $("bufferbar").style.width = `${Math.round(fill * 100)}%`;
  $("bufferbar").style.background = d.ready ? "var(--accent)" : "var(--warning)";
  $("buffertext").textContent = st.connected ? `${Math.floor(Math.min(d.bufferFillMs, d.bufferTargetMs) / 1000)} /${Math.floor(d.bufferTargetMs / 1000)} s` : "—";
  $("bitrate").textContent = st.connected ? `${(st.bitrateKbps / 1000).toFixed(1)} Mb/s` : "—";
  $("uptime").textContent = st.connected ? hms(st.uptimeMs) : "—";
  $("banner").style.display = st.emergencyStop ? "block" : "none";
  $("overlay").textContent = st.overlayVisible ? "◉" : "○";
  $("overlay").title = st.overlayVisible ? OD.t("overlay_hide") : OD.t("overlay_show");
  const findings = (st.health && st.health.findings) || [];
  $("health").style.display = findings.length ? "block" : "none";
  $("health").textContent = findings.map((f) => "⚠ " + OD.t("health_" + f.code)).join("  ");
}

async function run(action, extra) {
  const r = await OD.act(action, extra).catch(() => null);
  $("toast").textContent = !r ? OD.t("unreachable") : r.success ? "" : OD.t("r_" + r.code);
  refreshNow();
}

function step(dir) {
  const base = pendingDelay ?? (last ? last.delay.configuredMs / 1000 : 30);
  pendingDelay = Math.min(600, Math.max(1, base + dir));
  $("delay").textContent = OD.duration(pendingDelay);
  clearTimeout(commitTimer);
  commitTimer = setTimeout(() => {
    const seconds = pendingDelay;
    pendingDelay = null;
    run("setDelay", { seconds });
  }, 700);
}

// Press and hold accelerates.
function holdable(button, dir) {
  let timer = 0;
  let ticks = 0;
  const repeat = () => { ticks++; step(ticks > 12 ? 5 * dir : dir); timer = setTimeout(repeat, ticks < 8 ? 140 : 60); };
  const stop = () => { clearTimeout(timer); timer = 0; };
  button.addEventListener("pointerdown", () => { ticks = 0; step(dir); timer = setTimeout(repeat, 450); });
  ["pointerup", "pointerleave", "pointercancel"].forEach((e) => button.addEventListener(e, stop));
  button.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); step(dir); } });
}

$("toggle").onclick = () => {
  const active = last && last.connected && ["delayed", "buffering", "draining"].includes(last.delay.state);
  run(active ? "goLive" : "enableDelay");
};
$("golive").onclick = () => run("goLive");
$("allow").onclick = () => run("clearEmergencyStop");
$("overlay").onclick = () => run("toggleOverlay");
holdable($("minus"), -1);
holdable($("plus"), 1);

OD.loadLanguage().then(() => { refreshNow = OD.poll(render); });
