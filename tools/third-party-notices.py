#!/usr/bin/env python3
"""Print THIRD_PARTY_NOTICES.md for the server binary.

    python3 tools/third-party-notices.py >THIRD_PARTY_NOTICES.md

Needs cargo. The Dockerfile runs it, so the image and the release archives
carry the notices of exactly the crates they were built from; `cargo deny`
(deny.toml) decides which licences those crates may have.

The notices cover what ends up in the binary: the normal dependencies (not
build, dev or proc-macro) of magnetite-sync on the image's TARGETS. They end
with the licence texts, read from each crate's own licence files. Crates with
the same text share one copy, with all their copyright lines above it. A crate
offered under several licences, one of them MIT, is used under MIT and only its
MIT file is copied.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

BINARY = "magnetite-sync"
# The container image's targets (amd64 and arm64, static musl builds).
TARGETS = ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"]

LICENCE_FILE = re.compile(r"^(licen[cs]e|copying|unlicense)", re.I)
# A copyright line ("Copyright (c) 2019 Jane", "© Unicode"), not licence prose that
# wraps onto a new line ("copyright license to ...", "(c) You must retain ...").
COPYRIGHT = re.compile(
    r"^[\s/#*]*(?:(?:Copyright|COPYRIGHT)\s+(?!AND\b|NOTICE\b|HOLDERS?\b|OWNER\b)(?:\([cC]\)|©|\d|[A-Z])"
    r"|\([cC]\)\s*\d|©)"
)
# A symlink published as a plain file: it holds only its target's path.
LINK = re.compile(r"^\S+$")

CC0_TEXT = """\
Dedicated to the public domain under CC0 1.0 Universal
(https://creativecommons.org/publicdomain/zero/1.0/legalcode). No notice is required."""
# Shipped crates published without a licence file: (copyright line, text) taken
# from their repositories. A new crate without a file stops the script.
NO_LICENCE_FILE = {
    "bitcoin_hashes": ("", CC0_TEXT),
}
# Shipped crates whose licence files are links ("../LICENSE-MIT"), and the crate
# of the same repository and version that carries the texts they point to.
LICENCE_FROM = {
    "sqlx-core": "sqlx",
    "sqlx-postgres": "sqlx",
}
# Opening words of common licence texts, only to title the groups of texts.
KNOWN_TEXTS = {
    "Apache-2.0": "Apache License",
    "BSD-3-Clause": "Neither the name",
    "BSL-1.0": "Boost Software License",
    "CC0-1.0": "CC0 1.0 Universal",
    "CDLA-Permissive-2.0": "Community Data License Agreement",
    "ISC": "Permission to use, copy, modify, and/or distribute",
    "MIT": "to deal in the Software without restriction",
    "Unicode-3.0": "UNICODE LICENSE V3",
    "Zlib": "This software is provided 'as-is'",
}

INTRO = """\
# Third-party notices

magnetite-sync is licensed under the EUPL-1.2 (`LICENSE`). It is built with the
Rust standard library and links the crates below, each under its own licence.
musl's licence (`musl-COPYRIGHT`) and the Rust standard library's
(`COPYRIGHT-library.html`) are next to this file.

Written by `tools/third-party-notices.py` when the binary was built."""

Key = tuple[str, str]  # (name, version)
Package = tuple[str, Path]  # (licence expression, directory with its files)


def run(*cmd: str) -> str:
    return subprocess.run(cmd, cwd=ROOT, check=True, capture_output=True, text=True).stdout


def shipped_crates() -> dict[Key, Package]:
    """Licence and source directory of every third-party crate in the binary."""
    meta = json.loads(run("cargo", "metadata", "--format-version", "1", "--locked"))
    ours = set(meta["workspace_members"])
    crates = {
        (p["name"], p["version"]): (p["license"] or "UNKNOWN", Path(p["manifest_path"]).parent)
        for p in meta["packages"]
        if p["id"] not in ours
    }
    cmd = ["cargo", "tree", "--locked", "-p", BINARY, "-e", "normal,no-proc-macro"]
    cmd += ["--prefix", "none", "-f", "{p}"]
    for target in TARGETS:
        cmd += ["--target", target]
    shipped = {}
    for line in filter(None, run(*cmd).splitlines()):
        name, version = line.split()[:2]
        key = (name, version.removeprefix("v"))
        if key in crates:  # skips this workspace's crates
            shipped[key] = crates[key]
    return shipped


def licence_files(key: Key, packages: dict[Key, Package]) -> list[tuple[str, str]]:
    """(copyright lines, licence text) of one package, from its own files."""
    licence, folder = packages[key]
    if key[0] in LICENCE_FROM:
        # A KeyError here: the crates' versions differ, check where the links point now.
        folder = packages[(LICENCE_FROM[key[0]], key[1])][1]
    files = sorted(f for f in folder.iterdir() if f.is_file() and LICENCE_FILE.match(f.name))
    # The REUSE layout: texts in LICENSES/, named after their SPDX ids.
    if not files and (folder / "LICENSES").is_dir():
        files = sorted(f for f in (folder / "LICENSES").iterdir() if f.is_file())
    if " AND " not in licence and "MIT" in licence:
        files = [f for f in files if "MIT" in f.name.upper()] or files
    if not files:
        if key[0] not in NO_LICENCE_FILE:
            sys.exit(f"{key[0]} {key[1]} ships no licence file: add it to NO_LICENCE_FILE")
        return [NO_LICENCE_FILE[key[0]]]
    texts = []
    for file in files:
        content = file.read_text(errors="replace").strip()
        if LINK.match(content):
            sys.exit(f"{key[0]} {key[1]}: {file.name} only links to {content}: add it to LICENCE_FROM")
        lines = content.splitlines()
        copyright = "\n".join(line.strip() for line in lines if COPYRIGHT.match(line))
        texts.append((copyright, "\n".join(line for line in lines if not COPYRIGHT.match(line)).strip()))
    return texts


def render_texts(packages: dict[Key, Package]) -> list[str]:
    # Grouped by the text with whitespace normalised: (text, copyright lines, users).
    groups: dict[str, tuple[str, set[str], set[str]]] = {}
    for key in sorted(packages):
        for copyright, text in licence_files(key, packages):
            _, copyrights, users = groups.setdefault(" ".join(text.split()), (text, set(), set()))
            copyrights.update(filter(None, copyright.splitlines()))
            users.add(f"{key[0]} {key[1]}")

    out = ["", "## Licence texts", ""]
    out += ["The licence texts of the crates above. Crates with the same"]
    out += ["text share one copy, preceded by all of their copyright lines.", ""]
    ordered = sorted(groups.values(), key=lambda g: (-len(g[2]), g[0]))
    for number, (text, copyrights, users) in enumerate(ordered, 1):
        flat = " ".join(re.sub(r"(?m)^[\s/#*]+", "", text).split())
        names = sorted((flat.find(words), spdx) for spdx, words in KNOWN_TEXTS.items() if words in flat)
        fence = "````" if "```" in text else "```"
        out += [f"### {number}. {', '.join(n for _, n in names) or 'Other'}", ""]
        out += [f"Used by: {', '.join(sorted(users, key=str.lower))}.", "", fence]
        out += sorted(copyrights, key=str.lower) + ([""] if copyrights else [])
        out += [text, fence, ""]
    return out[:-1]


def render() -> str:
    shipped = shipped_crates()
    out = [INTRO, "", "## Rust crates", ""]
    out += ["| Crate | Version | Licence |", "|---|---|---|"]
    for key in sorted(shipped):
        out.append(f"| {key[0]} | {key[1]} | {shipped[key][0]} |")
    out += render_texts(shipped)
    return "\n".join(out) + "\n"


if __name__ == "__main__":
    sys.stdout.write(render())
