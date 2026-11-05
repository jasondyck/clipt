#!/usr/bin/env python3
"""Write bucket/clipt.json, the Scoop manifest, for a release.

The repository is its own Scoop bucket:
    scoop bucket add clipt https://github.com/jasondyck/clipt
    scoop install clipt/clipt
The release workflow runs this with the new Windows zip's SHA-256 and
commits the result, so the bucket follows releases without Scoop's own
autoupdate machinery. By hand, from the repo root:
    .github/scripts/scoop-manifest.py 0.9.0 <sha256 of clipt-v0.9.0-x86_64-windows.zip>
"""

import json
import pathlib
import re
import sys

version, sha256 = sys.argv[1:3]
if not re.fullmatch(r"\d+\.\d+\.\d+", version):
    sys.exit(f"{version}: Scoop gets releases only, not prereleases")
if not re.fullmatch(r"[0-9a-f]{64}", sha256):
    sys.exit(f"{sha256}: not a SHA-256")

name = f"clipt-v{version}-x86_64-windows"
manifest = {
    "version": version,
    "description": "Screenshots and screen recordings: select, annotate, copy or save",
    "homepage": "https://github.com/jasondyck/clipt",
    "license": "MIT|Apache-2.0",
    "notes": "Recording needs ffmpeg: scoop install ffmpeg. Update clipt with scoop update clipt.",
    "suggest": {"ffmpeg": "main/ffmpeg"},
    "architecture": {
        "64bit": {
            "url": f"https://github.com/jasondyck/clipt/releases/download/v{version}/{name}.zip",
            "hash": sha256,
            "extract_dir": name,
        }
    },
    "bin": "clipt.exe",
    "shortcuts": [["clipt.exe", "clipt"], ["clipt.exe", "clipt tray", "tray"]],
    "checkver": {"github": "https://github.com/jasondyck/clipt"},
}

out = pathlib.Path(__file__).resolve().parents[2] / "bucket" / "clipt.json"
out.parent.mkdir(exist_ok=True)
out.write_text(json.dumps(manifest, indent=4, ensure_ascii=False) + "\n")
print(f"{out.relative_to(out.parents[1])}: clipt {version}")
