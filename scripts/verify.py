#!/usr/bin/env python3
"""Collect native checks in one run; report every runnable failure."""
import argparse
import os
from pathlib import Path
import subprocess
import sys

from build import ROOT, build


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist", type=Path, default=ROOT / "dist")
    parser.add_argument("--no-build", action="store_true", help="use previously built native artifacts")
    parser.add_argument("--real", action="store_true", help="also download and exercise published Computer releases")
    args = parser.parse_args()
    environment = {**os.environ, "RCI_DIST": str(args.dist.resolve())}
    failures = []

    def check(label, command):
        print(f"\n[{label}]", flush=True)
        try:
            result = subprocess.run(command, cwd=ROOT, env=environment, check=False)
            if result.returncode:
                failures.append(label)
        except OSError as error:
            print(f"{label}: {error}", file=sys.stderr)
            failures.append(label)

    check("Rust formatting", ["cargo", "fmt", "--all", "--check"])
    check("Rust diagnostics", ["cargo", "clippy", "--locked", "--all-targets", "--all-features", "--", "-D", "warnings"])
    check("Python syntax", [sys.executable, "-m", "compileall", "-q", "scripts", "verification"])
    ready = True
    if not args.no_build:
        try:
            build(args.dist, fixtures=True)
        except (OSError, ValueError, subprocess.CalledProcessError) as error:
            failures.append("native build")
            ready = False
            print(f"Native process checks blocked by build failure: {error}", file=sys.stderr)
    if ready:
        check("native contract", [sys.executable, "-m", "unittest", "discover", "-s", "verification", "-p", "test_*.py", "-v"])
        if args.real:
            check("published Computer", [sys.executable, "verification/real.py"])
    print("\n" + ("Failed: " + ", ".join(failures) if failures else "All requested checks passed."))
    return int(bool(failures))


if __name__ == "__main__":
    sys.exit(main())
