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

// The pill is the status live region: only real changes are written, so polling stays silent.
function pill(text) {
  if ($("pill").textContent !== text) $("pill").textContent = text;
}

function render(st, err) {
  // Unreachable: grey out the last numbers instead of showing them as current.
  document.body.classList.toggle("stale", !st);
  if (!st) {
    pill(OD.t("unreachable"));
    $("pill").style.setProperty("--tone", "var(--error)");
    return;
  }
  last = st;
  const d = st.delay;
  const active = st.connected && ["delayed", "buffering", "draining"].includes(d.state);
  pill(OD.t("state_" + st.state));
  $("pill").style.setProperty("--tone", TONES[st.state] || TONES.offline);
  // How far behind viewers are now (0 when live); the stepper below shows the delay setting.
  $("digits").textContent = Math.round(d.currentMs / 1000);
  $("number").classList.toggle("active", active);
  const behind = OD.duration(Math.round(d.currentMs / 1000));
  const finishing = (st.streams || []).some((s) => s.phase === "finishing");
  $("caption").textContent = finishing ? OD.t("caption_finishing", behind)
    : st.state === "buffering" ? OD.t("caption_buffering", `${Math.round((d.progress || 0) * 100)}%`)
    : OD.t("caption_" + st.state, behind);
  $("toggle").textContent = active ? OD.t("disable") : OD.t("enable");
  const hadFocus = document.activeElement === $("golive");
  // Stays enabled while going live so keyboard focus has somewhere to land; a press then only says to wait.
  $("toggle").disabled = !d.modeEnabled;
  $("golive").disabled = !active || d.state === "draining";
  // A focused button that just got disabled would drop keyboard focus to the page.
  if (hadFocus && $("golive").disabled) $("toggle").focus();
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
  $("overlay").setAttribute("aria-pressed", String(st.overlayVisible));
  const findings = (st.health && st.health.findings) || [];
  $("health").style.display = findings.length ? "block" : "none";
  $("health").textContent = findings.map((f) => "⚠ " + OD.t("health_" + f.code)).join("  ");
}

async function run(action, extra) {
  const r = await OD.act(action, extra).catch(() => null);
  $("toast").textContent = !r ? OD.t("unreachable") : r.success ? "" : OD.t("r_" + r.code);
  // In a short dock the message can sit below the fold.
  if ($("toast").textContent) $("toast").scrollIntoView({ block: "nearest" });
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
  }, 400);
}

// Press and hold accelerates.
function holdable(button, dir) {
  let timer = 0;
  let ticks = 0;
  const repeat = () => { ticks++; step(ticks > 12 ? 5 * dir : dir); timer = setTimeout(repeat, ticks < 8 ? 140 : 60); };
  const stop = () => { clearTimeout(timer); timer = 0; };
  button.addEventListener("pointerdown", () => { ticks = 0; step(dir); timer = setTimeout(repeat, 450); });
  ["pointerup", "pointerleave", "pointercancel"].forEach((e) => button.addEventListener(e, stop));
  // Keyboard: Enter/Space step this way, arrows step either way; held keys speed up like a held pointer.
  button.addEventListener("keydown", (e) => {
    const d = { Enter: dir, " ": dir, ArrowUp: 1, ArrowRight: 1, ArrowDown: -1, ArrowLeft: -1 }[e.key];
    if (!d) return;
    e.preventDefault();
    ticks = e.repeat ? ticks + 1 : 0;
    step(ticks > 12 ? 5 * d : d);
  });
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
