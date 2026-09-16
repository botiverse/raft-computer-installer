#!/usr/bin/env python3
"""Build and assemble native installer releases using Python's standard library."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]
TARGETS = {"darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64"}


def target_key():
    system = {"Darwin": "darwin", "Linux": "linux", "Windows": "win32"}.get(platform.system())
    arch = {"arm64": "arm64", "aarch64": "arm64", "x86_64": "x64", "amd64": "x64"}.get(platform.machine().lower())
    target = f"{system}-{arch}"
    if target not in TARGETS:
        raise ValueError(f"unsupported native target: {target}")
    return target


def identity(path):
    digest = hashlib.sha256()
    size = 0
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
            size += len(block)
    return {"sha256": digest.hexdigest(), "size": size}


def sign(path):
    if platform.system() == "Darwin":
        subprocess.run(["codesign", "--force", "--sign", os.environ.get("RAFT_CODESIGN_IDENTITY", "-"), str(path)], check=True)
        subprocess.run(["codesign", "--verify", "--strict", str(path)], check=True)


def assemble(output, require_all=False):
    output = Path(output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    for name in ("install.sh", "install.ps1"):
        shutil.copy2(ROOT / "bootstrap" / name, output / name)
    (output / "install.sh").chmod(0o755)
    inventory = {name: identity(output / name) for name in ("install.sh", "install.ps1")}
    found = set()
    for target in sorted(TARGETS):
        name = "raft-computer-installer" + (".exe" if target.startswith("win32-") else "")
        relative = f"native/{target}/{name}"
        binary = output / relative
        if not binary.is_file():
            continue
        found.add(target)
        inventory[relative] = identity(binary)
        # One self-supervising executable per platform; no runner sidecar.
        sums = binary.parent / "SHA256SUMS"
        sums.write_text(f"{inventory[relative]['sha256']}  {relative}\n", encoding="utf-8", newline="\n")
        inventory[f"native/{target}/SHA256SUMS"] = identity(sums)
    if not found or (require_all and found != TARGETS):
        raise ValueError(f"incomplete native release: {sorted(found)}")
    cargo = tomllib.loads((ROOT / "Cargo.toml").read_text())
    version = cargo["package"]["version"]
    tag = os.environ.get("GITHUB_REF_NAME", "")
    if os.environ.get("GITHUB_REF_TYPE") == "tag" and tag != f"v{version}":
        raise ValueError("release tag does not match Cargo.toml version")
    manifest = {"schema": "raft-computer-installer/release/v3", "installerVersion": version,
                "gitCommit": os.environ.get("GITHUB_SHA"), "kCommit": cargo["dependencies"]["k-carrier"]["rev"],
                "files": inventory}
    path = output / "installer-manifest.json"
    path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8", newline="\n")
    checksums = {**inventory, "installer-manifest.json": identity(path)}
    (output / "SHA256SUMS").write_text("".join(f"{info['sha256']}  {name}\n" for name, info in sorted(checksums.items())), encoding="utf-8", newline="\n")
    return manifest


def build(output, fixtures=False):
    output = Path(output).resolve()
    target = target_key()
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")).resolve()
    command = ["cargo", "build", "--locked", "--release", "--bin", "raft-computer-installer"]
    if fixtures:
        command += ["--features", "verification-fixture", "--bin", "raft-computer-fixture"]
    subprocess.run(command, cwd=ROOT, check=True, env={**os.environ, "CARGO_TARGET_DIR": str(target_dir)})
    name = "raft-computer-installer" + (".exe" if os.name == "nt" else "")
    binary = output / "native" / target / name
    binary.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(target_dir / "release" / name, binary)
    binary.chmod(0o755)
    sign(binary)
    if fixtures:
        name = "raft-computer-fixture" + (".exe" if os.name == "nt" else "")
        fixture = output / "fixtures" / name
        fixture.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(target_dir / "release" / name, fixture)
        fixture.chmod(0o755)
    return assemble(output)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "dist")
    parser.add_argument("--fixtures", action="store_true")
    parser.add_argument("--assemble", action="store_true", help="assemble already built platform artifacts")
    parser.add_argument("--require-all", action="store_true")
    args = parser.parse_args()
    result = assemble(args.output, args.require_all) if args.assemble else build(args.output, args.fixtures)
    print(json.dumps({"version": result["installerVersion"], "files": sorted(result["files"])}))
