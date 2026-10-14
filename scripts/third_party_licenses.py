#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Regenerate THIRD_PARTY_LICENSES.md from `cargo metadata` (Linux + Windows targets)."""
import json, subprocess, sys

TARGETS = ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"]
EXTRA = [  # bundled non-crate assets: (name, version, licence, source)
    ("Inter typeface (assets/fonts)", "4.1", "OFL-1.1", "https://github.com/rsms/inter"),
]

crates = {}
for target in TARGETS:
    meta = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", target]))
    used = {n["id"] for n in meta["resolve"]["nodes"]}
    for p in meta["packages"]:
        if p["id"] in used and p["source"]:  # skip workspace crates
            crates[(p["name"], p["version"])] = (p.get("license") or "SEE LICENSE FILE", p.get("repository") or "")

out = ["# Third-party licenses", "",
       "OpenDelay is GPL-3.0-only. It links or bundles the components below, all under",
       "GPL-3.0-compatible licences (checked by `cargo deny check licenses`).",
       "Regenerate with `python3 scripts/third_party_licenses.py > THIRD_PARTY_LICENSES.md`.", "",
       "| Component | Version | Licence | Source |", "|---|---|---|---|"]
for name, ver, lic, src in EXTRA:
    out.append(f"| {name} | {ver} | {lic} | {src} |")
for (name, ver), (lic, src) in sorted(crates.items()):
    out.append(f"| {name} | {ver} | {lic.replace('|', '/')} | {src} |")
print("\n".join(out))
