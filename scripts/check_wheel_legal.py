"""Fail unless the Python wheel carries the repository's legal files.

The wheel bundles the native library, so it must ship LICENSE, NOTICE.md and
THIRD_PARTY_LICENSES.md byte-identical to the repository root's. The publish
job copies them in; this gate builds a wheel from runtime/python and proves
its dist-info holds them. scripts/check_npm_package_legal.cjs is the npm
package's counterpart.

Usage: python3 scripts/check_wheel_legal.py [--no-build-isolation]
"""

import argparse
from pathlib import Path
import subprocess
import sys
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parents[1]
LEGAL = ("LICENSE", "NOTICE.md", "THIRD_PARTY_LICENSES.md")


def expected():
    return {name: (ROOT / name).read_bytes() for name in LEGAL}


def wheel_contents(out_dir, isolation):
    """The legal files a freshly built wheel records under its dist-info."""
    command = [sys.executable, "-m", "pip", "wheel", "--no-deps", "-w", str(out_dir)]
    if not isolation:
        command.append("--no-build-isolation")
    subprocess.run(command + [str(ROOT / "runtime" / "python")], check=True,
                   capture_output=True, text=True)
    wheels = sorted(Path(out_dir).glob("paste_assetgraph-*.whl"))
    if len(wheels) != 1:
        raise SystemExit(f"expected one wheel, built {[w.name for w in wheels]}")
    found = {}
    with zipfile.ZipFile(wheels[0]) as archive:
        for name in archive.namelist():
            parts = name.split("/")
            if (len(parts) == 3 and parts[0].endswith(".dist-info")
                    and parts[1] == "licenses" and parts[2] in LEGAL):
                found[parts[2]] = archive.read(name)
    return found


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--no-build-isolation", action="store_true")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as out_dir:
        found = wheel_contents(out_dir, not args.no_build_isolation)
    failures = []
    for name, content in expected().items():
        if name not in found:
            failures.append(f"wheel lacks {name}")
        elif found[name] != content:
            failures.append(f"wheel carries a {name} that differs from the repository's")
    for failure in failures:
        print(failure, file=sys.stderr)
    if not failures:
        print(f"wheel carries {', '.join(LEGAL)}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
