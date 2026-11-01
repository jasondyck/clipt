#!/usr/bin/env python3
"""Prepare a release: set the version in Cargo.toml, date the CHANGELOG and
list the release in the Flatpak metainfo.

    prepare-release.py 0.2.0          # bump + changelog, for the Release workflow
    prepare-release.py --verify 0.2.0 # only check Cargo.toml/CHANGELOG agree

The [Unreleased] section's entries move into a new "## [X] - YYYY-MM-DD"
section. If the CHANGELOG already has "## [X] - unreleased" (written ahead
of time), the [Unreleased] entries are merged into it and it gets the date.
Prereleases (X.Y.Z-rc1) aren't added to the metainfo, which lists only
releases software centres should show.
"""

import datetime
import re
import sys
from pathlib import Path

METAINFO = Path("packaging/flatpak/io.github.jasondyck.clipt.metainfo.xml")
SEMVER = re.compile(r"^\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$")


def die(msg):
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)


def cargo_version(text):
    m = re.search(r'(?m)^version = "([^"]+)"', text)
    return m.group(1) if m else None


def sections(lines):
    """[(heading or None, body lines)] split on '## ' headings."""
    out, cur, body = [], None, []
    for line in lines:
        if line.startswith("## "):
            out.append((cur, body))
            cur, body = line, []
        else:
            body.append(line)
    out.append((cur, body))
    return out


def merge_bodies(a, b):
    """Merge two Keep-a-Changelog bodies, combining same-named ### groups."""

    def groups(body):
        pre, gs, name = [], {}, None
        order = []
        for line in body:
            if line.startswith("### "):
                name = line.strip()
                if name not in gs:
                    gs[name] = []
                    order.append(name)
            elif name is None:
                pre.append(line)
            else:
                gs[name].append(line)
        return pre, gs, order

    pa, ga, oa = groups(a)
    pb, gb, ob = groups(b)
    pre = [line for line in pb if line.strip()] or []
    out = [""] + pre + ([""] if pre else [])
    for name in oa + [n for n in ob if n not in oa]:
        items = [line for line in ga.get(name, []) + gb.get(name, []) if line.strip()]
        if items:
            out += [name] + items + [""]
    return out


def add_metainfo_release(text, version, date):
    """Add <release version date/> as the newest release, once."""
    if f'<release version="{version}"' in text:
        return text
    m = re.search(r"(?m)^(\s*)<releases>\n", text)
    if not m:
        die(f"{METAINFO} has no <releases>")
    indent = m.group(1) + "  "
    line = f'{indent}<release version="{version}" date="{date}"/>\n'
    return text[: m.end()] + line + text[m.end() :]


def main():
    args = sys.argv[1:]
    verify = "--verify" in args
    args = [a for a in args if a != "--verify"]
    if len(args) != 1 or not SEMVER.match(args[0]):
        die("usage: prepare-release.py [--verify] X.Y.Z[-pre]")
    version = args[0]
    cargo = Path("Cargo.toml")
    log = Path("CHANGELOG.md")
    ctext = cargo.read_text()

    if verify:
        have = cargo_version(ctext)
        if have != version:
            die(f"tag says {version} but Cargo.toml says {have}")
        if f"## [{version}] - " not in log.read_text():
            die(f"CHANGELOG.md has no section for {version}")
        print(f"ok: {version}")
        return

    ctext, n = re.subn(r'(?m)^version = "[^"]+"', f'version = "{version}"', ctext, count=1)
    if n != 1:
        die("no version line in Cargo.toml")
    cargo.write_text(ctext)

    today = datetime.date.today().isoformat()
    secs = sections(log.read_text().splitlines())
    unreleased = next((b for h, b in secs if h and h.startswith("## [Unreleased]")), None)
    if unreleased is None:
        die("CHANGELOG.md has no [Unreleased] section")
    out = []
    placed = False
    for h, body in secs:
        if h is None:
            out += body
        elif h.startswith("## [Unreleased]"):
            out += [h, ""]
            # A pre-written section for this version absorbs the entries below.
            if not any(x and x.startswith(f"## [{version}]") for x, _ in secs):
                out += [f"## [{version}] - {today}"] + merge_bodies(unreleased, [])
                placed = True
        elif h.startswith(f"## [{version}]"):
            out += [f"## [{version}] - {today}"] + merge_bodies(unreleased, body)
            placed = True
        else:
            out += [h] + body
    if not placed:
        die("couldn't place the release section")
    log.write_text("\n".join(out).rstrip() + "\n")
    if "-" not in version:
        METAINFO.write_text(add_metainfo_release(METAINFO.read_text(), version, today))
    print(f"prepared {version} ({today})")


if __name__ == "__main__":
    main()
