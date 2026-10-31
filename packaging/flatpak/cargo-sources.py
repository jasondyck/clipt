#!/usr/bin/env python3
"""Write cargo-sources.json for the Flatpak build from Cargo.lock.

flatpak-builder builds offline, so every crate is downloaded as a source and
unpacked into cargo/vendor/, and a cargo config redirects crates.io there.
clipt only uses crates.io crates, so that's all this handles.

    packaging/flatpak/cargo-sources.py  (from the repo root)
"""

import json
import pathlib
import sys
import tomllib

root = pathlib.Path(__file__).resolve().parents[2]
lock = tomllib.loads((root / "Cargo.lock").read_text())
sources = []
for pkg in lock["package"]:
    src = pkg.get("source")
    if src is None:
        continue  # clipt itself
    if src != "registry+https://github.com/rust-lang/crates.io-index":
        sys.exit(f"{pkg['name']}: unsupported source {src}")
    name, ver, sha = pkg["name"], pkg["version"], pkg["checksum"]
    dest = f"cargo/vendor/{name}-{ver}"
    sources += [
        {
            "type": "archive",
            "archive-type": "tar-gzip",
            "url": f"https://static.crates.io/crates/{name}/{name}-{ver}.crate",
            "sha256": sha,
            "dest": dest,
        },
        {
            "type": "inline",
            "contents": json.dumps({"package": sha, "files": {}}),
            "dest": dest,
            "dest-filename": ".cargo-checksum.json",
        },
    ]
sources.append(
    {
        "type": "inline",
        "contents": '[source.crates-io]\nreplace-with = "vendored-sources"\n\n'
        '[source.vendored-sources]\ndirectory = "cargo/vendor"\n',
        "dest": "cargo",
        "dest-filename": "config.toml",
    }
)
out = pathlib.Path(__file__).with_name("cargo-sources.json")
out.write_text(json.dumps(sources, indent=1) + "\n")
print(f"{out.relative_to(root)}: {len(sources) // 2} crates")
