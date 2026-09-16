# Raft Computer Installer

An independent installer, upgrader and repair tool for Raft Computer, written in
Rust and built on [K](https://github.com/botiverse/k-carrier). It is released
separately from Computer and can repair an installation that cannot start.

It implements K's [installer contract](https://botiverse.github.io/k-carrier/installer.html).
K supplies the transaction; this project supplies Computer's release source,
installation paths, service commands and first setup. The contract defines the
shared behavior; this README covers how to use and develop the Computer adapter.

## Install and use

```sh
curl -fsSL https://cdn.raft.build/computer/install.sh | sh
curl -fsSL https://cdn.raft.build/computer/install.sh | sh -s -- --channel alpha
curl -fsSL https://cdn.raft.build/computer/install.sh | sh -s -- --version 1.0.31 --yes
curl -fsSL https://cdn.raft.build/computer/install.sh | sh -s -- repair --version 1.0.31 --yes
curl -fsSL https://cdn.raft.build/computer/install.sh | sh -s -- status
curl -fsSL https://cdn.raft.build/computer/install.sh | CI=1 sh
```

On Windows:

```powershell
irm https://cdn.raft.build/computer/install.ps1 | iex
& ([scriptblock]::Create((irm https://cdn.raft.build/computer/install.ps1))) --channel alpha
```

The bootstrap downloads and verifies the installer, runs it, and cleans up its
temporary files. It uses a published release. To run code from this checkout,
use the native executable produced by the build command below.

| Command | Behavior |
| --- | --- |
| `install`, `upgrade` | Bring the machine to the selected version: install if fresh, adopt a pre-K installation, upgrade a managed installation, or repair a broken one. No command means `upgrade` in the native CLI and `install` in the bootstrap; both choose the path from the machine's state. |
| `repair` | Reinstall a broken installation, preserving credentials, configuration and user files. Held when the installation is healthy. |
| `status` | Settle interrupted work and report the current installation. Does not reinstall. |
| `recover` | Recover interrupted work from local state, without selecting a new release. |

| Option | Meaning |
| --- | --- |
| `--version V` | Select an exact Computer version. |
| `--channel NAME` | Select `main` (default), `alpha`, or a named feature channel. Resolved once through Hands; mutually exclusive with `--version`. |
| `--yes`, `-y` | Accept the selected version without an attended confirmation. Unattended runs never ask; invocation is consent. |
| `--allow-downgrade` | Explicitly allow an older target. Otherwise the request is held. |
| `--json` | Print a structured result instead of the human result line. |

`raft-computer-installer --version` alone prints the installer's own version.

First setup uses `raft-computer login` when someone is at the terminal. After a
successful login, Computer starts. An unattended fresh install stays stopped and
reports the next step. Upgrades do not repeat login: a running service comes back
running, and a stopped installation stays stopped.

A Computer CLI waiting for its installer is not a running service. The installer
attests that CLI's immediate-parent process identity at entry and retains the
exclusion only for that operation. Remote service callers remain product processes.
Computer CLIs must explicitly declare that they are waiting. A terminal does not
establish this role. For older Computer versions, invoke the standalone installer
directly: upgrading through an older CLI without this declaration is unsupported.
An undeclared CLI caller is rejected before installation changes. Supported
calling CLIs receive the final installer exit code, not background acceptance.

Exit codes: **0** succeeded or already up to date; **1** failed or rolled back;
**2** held; **3** recovery unresolved. Details are recorded under
`<RAFT_HOME>/computer/installer/receipts/`. Repeating a completed operation ID
returns its unchanged receipt with a historical-result label; use `status` to
observe the machine now. An unreadable old receipt prevents reusing that request
ID, but does not by itself mark the current installation broken. Active recovery
records remain authoritative until their operation is confirmed finished.

## Configuration

| Environment | Meaning |
| --- | --- |
| `CI`, `RAFT_COMPUTER_NON_INTERACTIVE=1` | Run unattended. Otherwise terminal availability determines whether to ask. |
| `RAFT_HOME`, fallback `SLOCK_HOME` | Computer's state root; default `~/.slock`. |
| `RAFT_COMPUTER_INSTALL_DIR` | Directory for `raft-computer` and its sidecar; default `~/.local/bin`. For that default, update a supported shell profile or Windows user PATH when needed. Custom directories produce a PATH hint. |
| `RAFT_COMPUTER_NO_MODIFY_PATH=1` | Leave PATH configuration unchanged. |
| `RAFT_COMPUTER_RELEASE_BASE` | Product CDN holding `<version>/manifest.json` and artifacts; default `https://cdn.raft.build/computer`. |
| `RAFT_COMPUTER_HANDS_ORIGIN`, `RAFT_COMPUTER_HANDS_APP` | Product release authority; defaults `https://hands.build` and `raft-computer-cli`. |
| `RAFT_COMPUTER_INSTALLER_CHANNEL` | Installer release channel; default `main`. Separate from Computer's `--channel`. |
| `RAFT_COMPUTER_INSTALLER_DL_BASE` | Installer download authority; default `https://hands.build/dl/raft-computer-installer`. |
| `RAFT_COMPUTER_INSTALLER_RELEASE_BASE` | Exact static installer release directory containing `SHA256SUMS` and `native/<target>/…`; overrides installer channel resolution. |
| `RAFT_COMPUTER_OPERATION_ID` | Launcher-supplied request ID for replay; generated when omitted. |
| `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY` | Download proxy settings. |

## Computer adapter

The adapter uses Computer's `start`, `stop`, `status --json` and `--version`
commands. A live status attestation supplies `servicePid`, `computerVersion` and
`serviceGeneration`; the installer also checks OS process identity. Stopped
installations are checked through a short-lived `--version` process.

`photon_rs_bg.wasm` is verified against the release manifest, cached per version,
and published beside its matching binary, including during rollback. Product
bytes run from the installation directory, outside K's slots.

K owns `<RAFT_HOME>/computer/k`. The installer keeps its operation records,
receipts, cached sidecars and recovery files under `<RAFT_HOME>/computer/installer`.
Repair temporarily retains recovery inputs under `quarantine/` while the result
is unresolved. After verification and a durable final receipt, the installer
removes obsolete recovery payloads, downloads and sidecars, retaining only the
current sidecar and small receipts. Interrupted cleanup resumes on the next
command. Computer user data and credentials are preserved. Installations owned
by another package manager are held.

## Develop

Use Rust 1.89 (pinned in `rust-toolchain.toml`), Python 3.11 or newer, and the
platform's native linker. Node is not required for building, running or core
verification. On Windows, use `python` in place of `python3`.

```sh
python3 scripts/build.py           # native executable, bootstrap and manifests
python3 scripts/verify.py          # static checks, build and native process scenarios
python3 scripts/verify.py --real   # also exercise actual published Computer binaries
```

The executable is `dist/native/<target>/raft-computer-installer` (`.exe` on
Windows). Targets are `darwin-arm64`, `darwin-x64`, `linux-arm64`, `linux-x64` and
`win32-x64`. macOS builds are ad-hoc signed by default; set
`RAFT_CODESIGN_IDENTITY` to use a Developer ID identity.

See [verification/README.md](verification/README.md) for scenarios, isolated test
resources and the distinction between fixture and real-product coverage.
Implementation progress and verification results are tracked in the
[alignment document](https://github.com/botiverse/elephant/blob/rust-native/docs/installer-alignment.zh-CN.md).

## Release

A `v<version>` tag matching `Cargo.toml` runs the five-platform build and validation
workflow. Each platform produces one native installer executable, which includes
its supervisor and worker.

`scripts/publish.py` publishes through the Hands HTTP API: one build containing
all platform binaries and checksums, followed by one release and public download
readback. CI uses `HANDS_INSTALLER_DEPLOY_TOKEN`; prereleases go to `alpha`, stable
versions to `main`. GitHub receives the bootstrap scripts, manifest, checksums and
an archive of the complete release.

The bootstrap resolves the installer channel once and downloads all files from
that immutable release. Static mirrors use the same `native/<target>/` layout.

### Installer channel promotion

Version tags publish prereleases to `alpha` and stable versions to `main`.
The default bootstrap always resolves the installer's `main` channel, separately
from the Computer version/channel it will install. To promote an already verified
release to another installer channel, run the **Promote installer release**
workflow with its exact tag and destination channel. It reuses the GitHub release
archive, checks the tag/commit and every asset hash, and downloads the published
Hands bytes again; it does not rebuild them or advance the Computer channel.
