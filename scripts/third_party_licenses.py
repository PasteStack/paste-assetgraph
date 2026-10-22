"""Write THIRD_PARTY_LICENSES.md: every crate linked into the shipped binaries.

The CLI release binaries and the FFI library the language runtimes bundle
statically link their Rust dependencies, so each one's license text travels
with them. The list is the normal (non-dev, non-build) dependency closure of
the workspace crates across every target, read from `cargo metadata --locked`,
and each crate's own license and notice files are copied verbatim. A crate
that ships no license file is listed with the standard text of every license
its SPDX expression requires (each conjunct of an AND, at least one
alternative of an OR), from scripts/license-texts, and with its holders: the
upstream copyright lines in UPSTREAM_NOTICES when they were checked against
the repository's LICENSE at that version, else the crate's own AUTHORS file,
else an attribution taken from Cargo metadata and labelled as such. The
notices in PORTED_NOTICES — the oxc repository's THIRD-PARTY-LICENSE and a
few verified ports — ride along with the reason each applies; they are
not a complete account of ported code, and the generated intro says so. A crate
with no holder source, or with a required license text missing, fails the
run. An AUTHORS file names holders; it is never a license file.

The toolchain components every binary also links — the Rust standard
library and the crates it links, musl libc in the static Linux CLI
binaries, and in the windows-gnu ones the runtime the Zig linker adds (Zig's
libc and compiler_rt, LLVM libunwind and the MinGW-w64 CRT) — are listed with
their upstream notices, from scripts/license-texts/notices.

Usage: python3 scripts/third_party_licenses.py [--check]
--check exits non-zero when the committed file differs from a fresh render.
"""

import argparse
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "THIRD_PARTY_LICENSES.md"
LICENSE_PREFIXES = ("license", "licence", "copying", "notice", "copyright", "unlicense")
HOLDER_PREFIXES = ("authors",)
STANDARD_TEXTS = ROOT / "scripts" / "license-texts"
NOTICES = STANDARD_TEXTS / "notices"

# Copyright lines for crates whose published package carries no license
# file, copied from the LICENSE in the upstream repository at the tag of the
# shipped version (or, where none was tagged, the commit the package records,
# else the nearest tags), with where they were read. Keyed by (repository,
# version): a version bump falls back to the labelled Cargo attribution until
# the new tag's LICENSE is checked. The crates still labelled have no holder
# line to copy: assert-unchecked and parcel_sourcemap carry no LICENSE at
# their release commit, the cssparser-color (MPL-2.0) and wasip2 license
# files name no holder, and seahash's repository has not been checked.
UPSTREAM_NOTICES = {
    ("https://github.com/oxc-project/oxc", "0.52.0"): (
        "tag crates_v0.52.0",
        [
            "Copyright (c) 2024-present VoidZero Inc. & Contributors",
            "Copyright (c) 2023 Boshen",
        ],
    ),
    # grass and grass_compiler share the repository.
    ("https://github.com/connorskees/grass", "0.13.4"): (
        "tag 0.13.4",
        ["Copyright (c) 2020 Connor Skees"],
    ),
    ("https://github.com/oxc-project/oxc-index-vec", "2.0.0"): (
        "tag v2.0.0",
        [
            "Copyright (c) 2024-present VoidZero Inc. & Contributors",
            "Copyright (c) 2023 Boshen",
        ],
    ),
    # No 1.0.9 tag exists; the LICENSE reads identically at both neighbours.
    ("https://github.com/oxc-project/oxc-sourcemap", "1.0.9"): (
        "tags v1.0.7 and v2.0.0, the nearest to 1.0.9, which has none",
        [
            "Copyright (c) 2024-present VoidZero Inc. & Contributors",
            "Copyright (c) 2023 Boshen",
        ],
    ),
    # base64-simd 0.7.0.
    ("https://github.com/Nugine/simd", "0.7.0"): (
        "tag v0.7.0",
        ["Copyright (c) 2021 Nugine"],
    ),
    # simd-abstraction 0.7.1.
    ("https://github.com/Nugine/simd", "0.7.1"): (
        "commit f3381a84, which the 0.7.1 package records; no v0.7.1 tag exists",
        ["Copyright (c) 2021 Nugine"],
    ),
    # base64-simd 0.8.0 and vsimd 0.8.0 share the tag.
    ("https://github.com/Nugine/simd", "0.8.0"): (
        "tag v0.8.0",
        ["Copyright (c) 2021 Nugine"],
    ),
    ("https://github.com/Nugine/const-str", "0.3.2"): (
        "commit a80a9428, which the 0.3.2 package records; no v0.3.2 tag exists",
        ["Copyright (c) 2020 Nugine"],
    ),
    # winapi-i686-pc-windows-gnu and winapi-x86_64-pc-windows-gnu 0.4.0,
    # published before packages recorded their commit; no tag names it.
    ("https://github.com/retep998/winapi-rs", "0.4.0"): (
        "commit 9497609e (LICENSE-MIT), which set both import-library crates"
        " to 0.4.0 and whose copies of their files match the packages",
        ["Copyright (c) 2015 The winapi-rs Developers"],
    ),
}

# Notices of code a crate was ported from, reproduced beside the crate's own
# because the port carries the original's terms. Over-inclusion is the safe
# direction for a notice; each entry says why it is there. Keyed by
# (repository, version, crate) — a crate of None covers every crate of that
# repository at that version. Ports noted only in test code, or from crates
# that are themselves listed (bumpalo, petgraph, the Rust standard library),
# need no entry here.
PORTED_NOTICES = {
    ("https://github.com/oxc-project/oxc-sourcemap", "1.0.9", None): [
        (
            "rust-sourcemap-9.1.0-LICENSE.txt",
            "its README calls it a fork of getsentry/rust-sourcemap and its"
            " decoder and encoder name rust-sourcemap 9.1.0 as their source;"
            " this is that project's BSD-3-Clause LICENSE at tag 9.1.0",
        ),
    ],
    ("https://github.com/oxc-project/oxc", "0.52.0", None): [
        (
            "oxc-crates_v0.52.0-THIRD-PARTY-LICENSE.txt",
            "the oxc repository's own THIRD-PARTY-LICENSE at tag"
            " crates_v0.52.0, covering the projects its crates adapt code from"
            " (among them esbuild, terser and closure-compiler)",
        ),
    ],
    ("https://github.com/oxc-project/oxc", "0.52.0", "oxc_ecmascript"): [
        (
            "boa-61567687-LICENSE-MIT.txt",
            "to_int_32 is copied from Boa at commit 61567687, which the oxc"
            " notice does not list; this is Boa's LICENSE-MIT at that commit"
            " (Boa is MIT or Unlicense)",
        ),
    ],
    ("https://github.com/oxc-project/oxc", "0.52.0", "oxc_traverse"): [
        (
            "babel-419644f2-LICENSE.txt",
            "its scope and identifier helpers are ported from Babel at commit"
            " 419644f2, which the oxc notice does not list; this is Babel's"
            " LICENSE at that commit",
        ),
    ],
}

# Linked into every binary by the toolchain rather than through Cargo.lock.
# The Rust notice is the COPYRIGHT file at the tag matching Cargo.toml's
# rust-version; a bump fails the run until the notice is refreshed.
TOOLCHAIN = [
    {
        "name": "Rust standard library (std, core, alloc)",
        "version": "1.83.0",
        "license": "MIT OR Apache-2.0",
        "source": "https://github.com/rust-lang/rust/tree/1.83.0/library",
        "holders": "The Rust Project contributors; copyrights are retained by"
                   " their contributors (see the COPYRIGHT notice)",
        "linked": "every binary and native library",
        "notice": "rust-1.83.0-COPYRIGHT.txt",
        "texts": ["MIT", "Apache-2.0"],
    },
    {
        "name": "musl libc",
        "version": "1.2.x (the notice is unchanged from 1.2.3 through 1.2.5)",
        "license": "MIT",
        "source": "https://musl.libc.org/",
        "holders": "Copyright © 2005-2020 Rich Felker, et al.",
        "linked": "the static Linux musl CLI binaries",
        "notice": "musl-COPYRIGHT.txt",
        "texts": [],
    },
    # The windows-gnu binaries are linked by Zig 0.16.0 (cargo-zigbuild in the
    # CI image). Its link line adds Zig's own libc pieces and compiler_rt,
    # LLVM libunwind and the MinGW-w64 CRT; no winpthreads library is linked.
    # The notices are the ones Zig 0.16.0 ships beside those sources.
    {
        "name": "Zig runtime libraries (zigc, compiler_rt, ubsan_rt)",
        "version": "0.16.0",
        "license": "MIT",
        "source": "https://github.com/ziglang/zig/tree/0.16.0/lib",
        "holders": "Copyright (c) Zig contributors",
        "linked": "the windows-gnu binaries and native library",
        "notice": "zig-0.16.0/LICENSE.txt",
        "texts": [],
    },
    {
        "name": "LLVM libunwind (as bundled by Zig)",
        "version": "Zig 0.16.0",
        "license": "Apache-2.0 WITH LLVM-exception",
        "source": "https://github.com/ziglang/zig/tree/0.16.0/lib/libunwind",
        "holders": "the LLVM project contributors listed in CREDITS.TXT",
        "linked": "the windows-gnu binaries and native library",
        "notice": "zig-0.16.0/libunwind-LICENSE.TXT",
        "texts": [],
    },
    {
        "name": "MinGW-w64 CRT (as bundled by Zig)",
        "version": "Zig 0.16.0",
        "license": "ZPL-2.1, with public-domain, BSD and LGPL parts as marked",
        "source": "https://github.com/ziglang/zig/tree/0.16.0/lib/libc/mingw",
        "holders": "Copyright (c) 2009 - 2013 by the mingw-w64 project",
        "linked": "the windows-gnu binaries and native library",
        "notice": "zig-0.16.0/mingw-COPYING.txt",
        "texts": [],
    },
]

# Crates the Rust standard library itself links for the shipped targets
# (Linux musl, macOS, windows-gnu), at the versions library/Cargo.lock pins
# at the Rust tag above. Their license files, under notices/std/, are the
# ones in each crate's published package. LLVM code std carries — libunwind
# on musl, compiler-rt in compiler_builtins — is covered by the Rust
# COPYRIGHT notice and compiler_builtins' own LICENSE.txt.
STD_CRATES = [
    ("addr2line", "0.22.0", "Apache-2.0 OR MIT"),
    ("adler", "1.0.2", "0BSD OR MIT OR Apache-2.0"),
    ("cfg-if", "1.0.0", "MIT OR Apache-2.0"),
    ("compiler_builtins", "0.1.133", "MIT OR Apache-2.0"),
    ("gimli", "0.29.0", "MIT OR Apache-2.0"),
    ("hashbrown", "0.15.0", "MIT OR Apache-2.0"),
    ("libc", "0.2.161", "MIT OR Apache-2.0"),
    ("memchr", "2.5.0", "Unlicense OR MIT"),
    ("miniz_oxide", "0.7.4", "MIT OR Zlib OR Apache-2.0"),
    ("object", "0.36.4", "Apache-2.0 OR MIT"),
    ("rustc-demangle", "0.1.24", "MIT OR Apache-2.0"),
]

# The MinGW-w64 runtime notice for binaries that link its runtime
# statically. It reads identically at mingw-w64 tags v11.0.0 and v12.0.0.
# winpthreads is not linked: the windows-gnu link line names no pthread
# library.
MINGW = [
    ("MinGW-w64 runtime", "COPYING.MinGW-w64-runtime.txt",
     "COPYING.MinGW-w64-runtime/COPYING.MinGW-w64-runtime.txt"),
]


def metadata():
    out = subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1"],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def shipped_packages(meta):
    nodes = {node["id"]: node for node in meta["resolve"]["nodes"]}
    members = set(meta["workspace_members"])
    packages = {pkg["id"]: pkg for pkg in meta["packages"]}

    # A proc-macro runs inside the compiler; nothing of it, nor of what it
    # alone depends on, is linked in — the walk never descends into one.
    def proc_macro(pid):
        return any("proc-macro" in target["kind"] for target in packages[pid]["targets"])

    seen = set()
    stack = list(members)
    while stack:
        current = stack.pop()
        for dep in nodes[current]["deps"]:
            if any(kind["kind"] is None for kind in dep["dep_kinds"]) and dep["pkg"] not in seen:
                seen.add(dep["pkg"])
                if not proc_macro(dep["pkg"]):
                    stack.append(dep["pkg"])
    linked = (packages[pid] for pid in seen - members if not proc_macro(pid))
    return sorted(linked, key=lambda pkg: (pkg["name"], pkg["version"]))


def license_files(pkg, prefixes=LICENSE_PREFIXES):
    crate_dir = Path(pkg["manifest_path"]).parent
    files = []
    for path in sorted(crate_dir.iterdir()):
        if path.is_file() and path.name.lower().startswith(prefixes):
            files.append(path)
    if prefixes is not LICENSE_PREFIXES:
        return files
    if pkg.get("license_file"):
        named = crate_dir / pkg["license_file"]
        if named.is_file() and named not in files:
            files.append(named)
    return files


def standard_texts(expression):
    """The standard texts an SPDX expression (or the legacy `/`-separated
    list) requires, and whether every requirement is met: each conjunct of
    an AND needs its text, an OR needs at least one alternative's, and the
    texts of every satisfiable alternative are included. `X WITH Y` needs
    the text of X; the exception's text, when on hand, is added too."""
    tokens = (expression or "").replace("/", " OR ").replace("(", " ( ").replace(")", " ) ").split()
    position = 0

    def peek():
        return tokens[position] if position < len(tokens) else None

    def take():
        nonlocal position
        position += 1
        return tokens[position - 1]

    def atom():
        if peek() == "(":
            take()
            found, ok = disjunction()
            if peek() != ")":
                return [], False
            take()
            return found, ok
        spdx = take() if peek() not in (None, ")", "AND", "OR", "WITH") else None
        if spdx is None:
            return [], False
        path = STANDARD_TEXTS / f"{spdx.rstrip('+')}.txt"
        found, ok = ([path], True) if path.is_file() else ([], False)
        if peek() == "WITH":
            take()
            exception = take() if peek() not in (None, ")", "AND", "OR") else None
            if exception is None:
                return [], False
            extra = STANDARD_TEXTS / f"{exception}.txt"
            if ok and extra.is_file():
                found.append(extra)
        return found, ok

    def conjunction():
        found, ok = atom()
        while peek() == "AND":
            take()
            more, more_ok = atom()
            found += more
            ok = ok and more_ok
        return found, ok

    def disjunction():
        found, ok = conjunction()
        found = found if ok else []
        while peek() == "OR":
            take()
            more, more_ok = conjunction()
            if more_ok:
                found += more
            ok = ok or more_ok
        return found, ok

    found, ok = disjunction()
    if peek() is not None:
        ok = False
    unique = []
    for path in found:
        if path not in unique:
            unique.append(path)
    return unique, ok and bool(unique)


def attribution(pkg):
    """The holder names Cargo metadata offers — unverified against the
    upstream notice, so rendered as an attribution, never as copyright."""
    if pkg.get("authors"):
        return ", ".join(pkg["authors"])
    if pkg.get("repository"):
        return f"the {pkg['name']} authors ({pkg['repository']})"
    return None


def render(meta):
    lines = [
        "# Third-party licenses",
        "",
        "The paste-assetgraph CLI binaries and the native library bundled with",
        "the Node, Python, and JVM runtimes statically link the Rust crates",
        "below. Each is listed with its declared license and its own license",
        "and notice files, reproduced verbatim. Source for every crate,",
        "including those under the MPL-2.0, is available from crates.io at the",
        "version shown and from the repository each crate names.",
        "",
        "Generated by `scripts/third_party_licenses.py` from `Cargo.lock`;",
        "do not edit by hand. Identical license texts are reproduced once,",
        "under License texts, and each crate names the texts it carries.",
        "A crate that ships no license file is listed with the standard",
        "text of each license its declared expression requires and with its",
        "holders: the copyright lines of the upstream LICENSE where they were",
        "checked at the tag named, the crate's AUTHORS file, or otherwise an",
        "attribution from its Cargo metadata, labelled as such.",
        "",
        "Some crates also carry ported-code notices: the oxc repository's",
        "own THIRD-PARTY-LICENSE for the oxc crates, and the notices of the",
        "few ported projects named in each entry, with the reason each",
        "applies. That list is not a complete account of ported code: other",
        "crates adapt code from further projects, under those projects'",
        "terms as the crates' own sources and license files record them.",
        "",
        "## Crates",
        "",
    ]
    texts = {}
    unattributed = []
    for pkg in shipped_packages(meta):
        name, version = pkg["name"], pkg["version"]
        refs = []
        for path in license_files(pkg):
            text = path.read_text(encoding="utf-8", errors="replace").rstrip()
            texts.setdefault(text, len(texts) + 1)
            refs.append(f"{path.name} (text {texts[text]})")
        holder_lines = []
        standard = []
        ported_lines = []
        repo = pkg.get("repository")
        ported = PORTED_NOTICES.get((repo, version, None), []) + PORTED_NOTICES.get(
            (repo, version, name), []
        )
        for notice, reason in ported:
            text = (NOTICES / notice).read_text(encoding="utf-8").rstrip()
            texts.setdefault(text, len(texts) + 1)
            ported_lines.append(f"- Ported-code notice: {notice} (text {texts[text]}) — {reason}")
        if not refs:
            upstream = UPSTREAM_NOTICES.get((pkg.get("repository"), version))
            authors_files = license_files(pkg, HOLDER_PREFIXES)
            if upstream:
                where, notice_lines = upstream
                holder_lines.append(
                    f"- Copyright notice (upstream LICENSE, {where}): {'; '.join(notice_lines)}"
                )
            elif authors_files:
                for path in authors_files:
                    text = path.read_text(encoding="utf-8", errors="replace").rstrip()
                    texts.setdefault(text, len(texts) + 1)
                    holder_lines.append(f"- Holders: {path.name} (text {texts[text]})")
            elif attribution(pkg):
                holder_lines.append(f"- Attribution (from Cargo metadata): {attribution(pkg)}")
            paths, satisfied = standard_texts(pkg.get("license"))
            for path in paths:
                text = path.read_text(encoding="utf-8").rstrip()
                texts.setdefault(text, len(texts) + 1)
                standard.append(f"{path.stem} (text {texts[text]})")
            if not holder_lines or not satisfied:
                unattributed.append(f"{name} {version}")
        lines.append(f"### {name} {version}")
        lines.append("")
        lines.append(f"- License: {pkg.get('license') or 'see its license files'}")
        lines.append(f"- Source: https://crates.io/crates/{name}/{version}")
        if pkg.get("repository"):
            lines.append(f"- Repository: {pkg['repository']}")
        if refs:
            lines.append(f"- License files: {', '.join(refs)}")
        lines.extend(holder_lines)
        lines.extend(ported_lines)
        if standard:
            lines.append(f"- Standard license texts: {', '.join(standard)}")
        lines.append("")
    if unattributed:
        raise SystemExit(
            "no license file, holder source, or required standard license text for: "
            + ", ".join(unattributed)
        )
    rust_version = next(
        line.split('"')[1]
        for line in (ROOT / "Cargo.toml").read_text(encoding="utf-8").splitlines()
        if line.startswith("rust-version")
    )
    if not TOOLCHAIN[0]["version"].startswith(rust_version + "."):
        raise SystemExit(
            f"rust-version {rust_version} does not match the Rust notice for "
            f"{TOOLCHAIN[0]['version']} — refresh scripts/license-texts/notices"
        )
    lines.append("## Toolchain components")
    lines.append("")
    lines.append("Linked by the compiler toolchain rather than through `Cargo.lock`.")
    lines.append("")
    for component in TOOLCHAIN:
        notice = (NOTICES / component["notice"]).read_text(encoding="utf-8").rstrip()
        texts.setdefault(notice, len(texts) + 1)
        refs = [f"{component['notice']} (text {texts[notice]})"]
        for spdx in component["texts"]:
            text = (STANDARD_TEXTS / f"{spdx}.txt").read_text(encoding="utf-8").rstrip()
            texts.setdefault(text, len(texts) + 1)
            refs.append(f"{spdx} (text {texts[text]})")
        lines.append(f"### {component['name']} {component['version']}")
        lines.append("")
        lines.append(f"- License: {component['license']}")
        lines.append(f"- Source: {component['source']}")
        lines.append(f"- Linked into: {component['linked']}")
        lines.append(f"- Copyright: {component['holders']}")
        lines.append(f"- Notice and license texts: {', '.join(refs)}")
        lines.append("")
    rust = TOOLCHAIN[0]["version"]
    for name, version, license in STD_CRATES:
        folder = NOTICES / "std" / f"{name}-{version}"
        refs = []
        for path in sorted(folder.iterdir()):
            text = path.read_text(encoding="utf-8", errors="replace").rstrip()
            texts.setdefault(text, len(texts) + 1)
            refs.append(f"{path.name} (text {texts[text]})")
        lines.append(f"### {name} {version} (linked by the Rust standard library {rust})")
        lines.append("")
        lines.append(f"- License: {license}")
        lines.append(f"- Source: https://crates.io/crates/{name}/{version}")
        lines.append("- Linked into: every binary and native library")
        lines.append(f"- License files: {', '.join(refs)}")
        lines.append("")
    for name, notice, upstream in MINGW:
        text = (NOTICES / "mingw-w64" / notice).read_text(encoding="utf-8").rstrip()
        texts.setdefault(text, len(texts) + 1)
        lines.append(f"### {name} (mingw-w64 v11.0.0–v12.0.0)")
        lines.append("")
        lines.append(f"- Source: https://github.com/mingw-w64/mingw-w64/blob/v12.0.0/{upstream}")
        lines.append("- Linked into: the windows-gnu binaries and native library")
        lines.append(f"- Notice: {notice} (text {texts[text]})")
        lines.append("")
    lines.append("## License texts")
    lines.append("")
    for text, number in texts.items():
        fence = "````" if "```" in text else "```"
        lines.append(f"### Text {number}")
        lines.append("")
        lines.append(f"{fence}text")
        lines.append(text)
        lines.append(fence)
        lines.append("")
    return "\n".join(lines).rstrip() + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    rendered = render(metadata())
    if args.check:
        current = OUTPUT.read_text(encoding="utf-8") if OUTPUT.is_file() else ""
        if current != rendered:
            print("THIRD_PARTY_LICENSES.md is stale — run scripts/third_party_licenses.py",
                  file=sys.stderr)
            return 1
        return 0
    OUTPUT.write_text(rendered, encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
