# Raft Computer Installer

An independent installer, upgrader and repair tool for Raft Computer, built on
[K](https://github.com/botiverse/k-carrier). The installer can be released without
releasing Computer and must remain usable when Computer cannot start.

The installer is released independently from Computer. A bootstrap script may be
published beside the installer, but the three identities remain separate:

- **bootstrap** (`install.sh`/`install.ps1`) — platform detection, installer
  download, hash/signature verification, and process launch only;
- **installer** — its own version and platform artifacts, state directory,
  request/receipt protocol, and K transaction runner;
- **Computer** — the product version and platform artifact selected by the
  Computer release authority.

The bootstrap does not need Hands. It selects a pinned, authenticated installer
release and starts it. The installer may consult Hands to resolve a Computer
channel or fetch a pinned Computer manifest directly. Hands chooses the product
release; it does not define the installer version.

## Scope

- A thin installation script that obtains, verifies and starts the installer.
- The Computer adapter: release lookup, installation ownership, service lifecycle,
  health checks, and product-specific setup and repair.
- Installer builds, independent release metadata and platform acceptance tests.

## Identity and receipt contract

Every request and receipt carries the protocol version, `installerVersion`,
`computerVersion`, `operationId`, operation, phase and terminal status. Artifact
metadata carries its own URL, size and SHA-256. `installerVersion` identifies
the helper that performed the operation; `computerVersion` identifies the
product bytes being installed. They must never be compared as one version
stream.

Installer publication is not machine upgrade success. A machine is upgraded
only after the installer writes a terminal receipt and the Computer adapter
reads back the live product version, pid and start-id from the running process.
Credentials, agent configuration, workspaces and user data are outside the
executable rollback boundary.

K supplies the upgrade transaction, executable slots, journal and recovery.
Computer exposes lifecycle and health controls. Credentials, agent configuration
and workspaces must survive installation and repair; executable rollback does not
undo application data migrations.

## Release workflow

A tag such as `v0.1.0-rc.1` runs `.github/workflows/release.yml`. CI pins Node
24, installs the immutable K commit, bundles the installer, writes an installer
manifest and provenance record, generates `SHA256SUMS`, runs the bootstrap
acceptance test, and publishes a private prerelease. A deployment may mirror
the release assets under `RAFT_COMPUTER_INSTALLER_RELEASE_BASE`; the bootstrap
never trusts an unverified `cli.cjs`.

The current release artifact is a Node 24 portable installer. Packaging a
runtime-independent SEA is a separate platform build step; until that is
published, machines must provide Node 24 (or set `RAFT_COMPUTER_INSTALLER_NODE`)
and a release mirror.

This project is intended to serve as a real product integration of K's external
runner, alongside K's smaller [service example](https://github.com/botiverse/k-carrier/tree/archer/external-runner-17/examples/external-service).
