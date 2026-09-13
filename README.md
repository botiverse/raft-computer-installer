# Raft Computer Installer

An independent installer, upgrader and repair tool for Raft Computer, built
on [K](https://github.com/botiverse/k-carrier). It is released on its own
schedule and works when Computer cannot start.

It is the reference implementation of K's
[installer contract](https://botiverse.github.io/k-carrier/installer.html):
what an installer does for every way a request can arrive, every machine it
can land on, and every way it can end. K supplies the transaction; this
project supplies everything around it. Read the contract first; this README
only says what is specific to Computer.

## Interface

```
curl -fsSL https://cdn.raft.build/computer/install.sh | sh
irm https://cdn.raft.build/computer/install.ps1 | iex                       # Windows
curl -fsSL https://cdn.raft.build/computer/install.sh | sh -s -- --channel alpha
curl -fsSL https://cdn.raft.build/computer/install.sh | sh -s -- --version 1.0.31 --yes
curl -fsSL https://cdn.raft.build/computer/install.sh | sh -s -- repair --version 1.0.31 --yes
CI=1 curl -fsSL https://cdn.raft.build/computer/install.sh | sh            # unattended: current release, no questions
```

| Command | Does |
|---|---|
| `install`, `upgrade` (default) | Bring this machine to one exact version: install if fresh, adopt then upgrade if installed before K, upgrade through K if managed, repair if broken. A fresh install ends with first setup (`raft-computer login`) when someone is there, then starts Computer; unattended it stays installed and says what to do next. |
| `repair` | Ask for repair explicitly: quarantine K's state and reinstall. Held unless the machine is broken. |
| `status` | What is installed and what is running. The receipt is not a live observation. |

| Option | Meaning |
|---|---|
| `--version V` | The exact version. Default: the channel's current release, resolved through Hands. |
| `--channel main\|alpha` | Which channel to resolve. Attended, the resolved version is shown and asked about. |
| `--yes` | Skip the question when attended. Unattended runs never ask; running one is the consent, repair included. |
| `--allow-downgrade` | Intend an older target; otherwise it is held. Going back to a version that worked is this. |

| Environment | Meaning |
|---|---|
| `CI`, `RAFT_COMPUTER_NON_INTERACTIVE=1` | Unattended: no questions. Otherwise a terminal decides. |
| `RAFT_HOME` (or `SLOCK_HOME`) | Computer's state root; K state lives at `<home>/computer/k`. Default `~/.slock`. |
| `RAFT_COMPUTER_INSTALL_DIR` | Where `raft-computer` and its sidecar are published. Default `~/.local/bin`, which a fresh install adds to `~/.zshrc` or `~/.bashrc` when missing; `RAFT_COMPUTER_NO_MODIFY_PATH=1` leaves profiles alone. |
| `RAFT_COMPUTER_RELEASE_BASE` | CDN holding `<version>/manifest.json` and artifacts. |
| `RAFT_COMPUTER_HANDS_ORIGIN`, `RAFT_COMPUTER_HANDS_APP` | The release authority a channel is resolved through. |
| `RAFT_COMPUTER_INSTALLER_VERSION`, `RAFT_COMPUTER_INSTALLER_RELEASE_BASE` | Which installer the bootstrap fetches and from where. |
| `RAFT_COMPUTER_INSTALLER_NODE` | Node 24+ used to run the installer. |
| `RAFT_COMPUTER_OPERATION_ID` | For launchers only: the operation id, so a repeated request replays its receipt instead of running again. |

Exit codes: 0 upgraded, up to date or installed; 1 failed or rolled back;
2 not done. Every run prints one line, in plain words; the
receipt under `<home>/computer/installer/receipts/` has the details.

## Pieces

| File | Role |
|---|---|
| `install.sh`, `install.ps1` | Bootstrap: download one pinned installer release, verify `SHA256SUMS`, and exec the matching native SEA. No Node fallback or portable runtime is published. |
| `native/<platform>/raft-computer-installer` | The entry as a single executable (Node SEA): presence, version resolution and Hands/CDN identity check, consent, settle, read the world, install/adopt/repair, one line and one exit code. Supervises the runner through K's launcher. |
| `native/<platform>/raft-computer-installer-runner` | K plus the Computer adapter as a single executable. Serves one request on stdin under K's lock; verified and retained by the supervisor for recovery. |

The adapter drives Computer through its CLI on `PATH`: `stop`, `start`, and
`status --json`, whose `attestation` carries `servicePid`,
`computerVersion` and `serviceGeneration`, the start id K compares across the
handover. Slot bytes are published onto `PATH` atomically before `start`;
nothing runs from inside a slot.

Computer needs a login before it can start, so the adapter has two modes,
decided at `quiesce` and remembered in `<home>/computer/installer/host-mode.json`
for recovery. If a service was running, it is stopped and the candidate must
come back as a live service. If nothing was running, the candidate is run
for its `--version` and that process is the readback: a new pid and start id
every time. A fresh install never starts Computer by itself: it publishes,
self-checks, then runs `raft-computer login` on the terminal when someone
is there, and only after a successful login starts Computer and reads it
back. `status --json` must work without a service and may carry a top-level
`nextStep` string ("run raft-computer login"), which the installer repeats. The `photon_rs_bg.wasm` sidecar is verified
against the release manifest, kept per version under the installer's state,
and published beside the binary with the slot it belongs to.

State: K owns `<home>/computer/k`. The installer owns
`<home>/computer/installer/{receipts,scratch,sidecars,quarantine}`. K's
records are its working memory for one transaction; anything there the
installer or K cannot read is moved to `quarantine/` and the machine is
reinstalled in the same run. Nothing under `<home>` is ever deleted by the
installer, and nothing outside `computer/` is touched.

## Develop

```
npm ci
npm run typecheck
npm run build        # internal bundle plus dist/install.sh, SHA256SUMS (portable bundle is not published)
npm run build:native # plus dist/native/<this platform>/raft-computer-installer{,-runner}
npm test             # real processes against a fake Computer: unattended, fresh, cold and live
                     # upgrades, replay, downgrade, first setup, adopt, foreign
                     # manager, broken and repair; and through the bootstrap: clean
                     # install of the current release and of one version, a machine
                     # without Node, a tampered installer, and the dirty K state an
                     # earlier K may have left (all reinstalled over, nothing deleted)
npm run test:native  # the same suites driving the single executables
```

Against a real Computer build, everything but a live-service upgrade (that
needs a login):

```
node scripts/e2e-real-computer.mjs ../slock/packages/computer/dist/raft-computer.js
```

It serves that build as two versions from a local release base and drives
the bootstrap through fresh install, cold upgrade, up to date, held
downgrade, an intended downgrade, `raft-computer upgrade`, status, and a
broken machine repaired. Temp homes only.

A tag `v*` runs `.github/workflows/release.yml`: one job per platform builds
and tests the executables, then one job assembles the portable files and
every executable under one `SHA256SUMS` and publishes a GitHub prerelease.
Mirror the assets under `RAFT_COMPUTER_INSTALLER_RELEASE_BASE`, keeping the
`native/<platform>/` layout; the bootstrap never runs an unverified file.

## Not yet

- `raft-computer upgrade` in Computer itself: it should ask, then run this
  bootstrap unattended with `--version`.
- Windows is ported (console instead of `/dev/tty`, processes through
  CIM, the running exe renamed aside before the new one is published, the
  user PATH in the registry, `win32-x64` in the release matrix, the test
  fake built as a single executable) but has not yet run on a Windows
  machine; CI's `windows-latest` job is the first check.
- macOS executables are ad-hoc signed; set `RAFT_CODESIGN_IDENTITY` in the
  build for a Developer ID signature.
