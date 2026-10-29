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
  // Only the delay time, and only while the delay is on.
  const delayed = st.connected && ["delayed", "buffering"].includes(d.state);
  const seconds = (d.state === "delayed" ? d.currentMs : delayed ? d.targetMs : d.configuredMs) / 1000;
  $("value").textContent = OD.duration(Math.round(seconds));
  $("progress").style.width = d.progress == null ? "0" : `${Math.round(d.progress * 100)}%`;
  const show = preview || (st.overlayVisible && delayed);
  badge.classList.toggle("hidden", !show);
}

OD.loadLanguage().then(() => OD.poll((st) => render(st)));
