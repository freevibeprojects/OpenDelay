// SPDX-License-Identifier: GPL-3.0-only
// Script for the on-stream overlay (overlay.html).
"use strict";
const PALETTE = {
  red: "#ef4444", orange: "#f97316", yellow: "#facc15", lime: "#84cc16", green: "#22c55e", teal: "#14b8a6",
  cyan: "#22d3ee", blue: "#3b82f6", indigo: "#6366f1", violet: "#a48ffb", pink: "#ec4899", white: "#f4f4f6",
};
const $ = (id) => document.getElementById(id);
const preview = OD.params.get("preview") === "1";
const scale = Math.min(4, Math.max(0.25, parseFloat(OD.params.get("scale")) || 1));
$("wrap").style.transform = `scale(${scale})`;
document.body.classList.toggle("preview", preview);

function render(st) {
  const badge = $("badge");
  if (!st) { badge.classList.add("hidden"); return; }
  const tone = PALETTE[OD.params.get("color")] || PALETTE[st.overlayColor] || PALETTE.violet;
  badge.style.setProperty("--tone", tone);
  const d = st.delay;
  const classic = st.overlayStyle === "classic";
  const delayed = st.connected && ["delayed", "buffering"].includes(d.state);
  const goingLive = st.connected && d.state === "draining";
  // The offline preview shows the badge as it looks with the delay on.
  const on = delayed || (preview && !st.connected);
  badge.classList.toggle("classic", classic);
  $("icon-delay").classList.toggle("off", !(on || goingLive));
  $("icon-live").classList.toggle("off", on || goingLive);
  const label = st.overlayText || (classic ? OD.t("badge_delay") : "");
  $("label").textContent = goingLive ? OD.t("badge_going_live") : on ? label : OD.t("badge_live");
  const seconds = (d.state === "delayed" ? d.currentMs : delayed ? d.targetMs : d.configuredMs) / 1000;
  $("value").textContent = on ? OD.duration(Math.round(seconds)) : "";
  $("progress").style.width = d.progress == null ? "0" : `${Math.round(d.progress * 100)}%`;
  const show = preview || (st.overlayVisible && (st.overlayOnlyDelayed ? delayed : st.connected));
  badge.classList.toggle("hidden", !show);
}

OD.loadLanguage().then(() => OD.poll((st) => render(st)));
