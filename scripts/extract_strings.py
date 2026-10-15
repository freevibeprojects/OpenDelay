#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""List the msgids of every @tr("...") in the desktop UI, one JSON string per line.
With a .po file argument, list only the msgids that file does not translate:
    python3 scripts/extract_strings.py crates/app/lang/<code>/LC_MESSAGES/opendelay.po"""
import glob, json, re, sys

TR = re.compile(r'@tr\(\s*"((?:[^"\\]|\\.)*)"')
seen = []
for path in sorted(glob.glob("crates/app/ui/**/*.slint", recursive=True)):
    for m in TR.finditer(open(path, encoding="utf-8").read()):
        s = m.group(1).encode().decode("unicode_escape").encode("latin-1").decode("utf-8")
        if s not in seen:
            seen.append(s)
if len(sys.argv) > 1:
    po = open(sys.argv[1], encoding="utf-8").read()
    done = {json.loads(m) for m in re.findall(r'^msgid ("(?:[^"\\]|\\.)*")$', po, re.M)}
    seen = [s for s in seen if s not in done]
for s in seen:
    print(json.dumps(s, ensure_ascii=False))
