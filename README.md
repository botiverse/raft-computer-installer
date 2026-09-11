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
curl -fsSL https://cdn.raft.build/computer/install.sh | sh -s -- --channel alpha
curl -fsSL https://cdn.raft.build/computer/install.sh | sh -s -- --version 1.0.31 --yes
curl -fsSL https://cdn.raft.build/computer/install.sh | sh -s -- repair --version 1.0.31 --yes
CI=1 curl -fsSL https://cdn.raft.build/computer/install.sh | sh            # unattended: current release, no questions
```

| Command | Does |
|---|---|
| `install`, `upgrade` (default) | Bring this machine to one exact version: install if fresh, adopt then upgrade if installed before K, upgrade through K if managed. Refuse if broken. |
| `rollback` | Upgrade to the previous stable version as an explicit target. |
| `repair` | Quarantine K's state and reinstall. Attended only; its own request and consent. |
| `recover <recovery.json>` | Retry an unresolved recovery offline. |
| `status` | What is installed and what is running. The receipt is not a live observation. |

| Option | Meaning |
|---|---|
| `--version V` | The exact version. Default: the channel's current release, resolved through Hands. |
| `--channel main\|alpha` | Which channel to resolve. Attended, the resolved version is shown and asked about. |
| `--yes` | Skip the question when attended. Unattended runs never ask; running one is the consent. Repair always needs it, and is attended only. |
| `--approved-by WHO` | Who consented, for the receipt; defaults to the local user. |
| `--id ID` | Operation id; the same id replays the first receipt. |
| `--allow-downgrade` | Intend an older target; otherwise it is held. |
| `--json` | Print the outcome as JSON instead of one line. |

| Environment | Meaning |
|---|---|
| `CI`, `RAFT_COMPUTER_NON_INTERACTIVE=1` | Unattended: no questions, no repair. Otherwise a terminal decides. |
| `RAFT_HOME` (or `SLOCK_HOME`) | Computer's state root; K state lives at `<home>/computer/k`. Default `~/.slock`. |
| `RAFT_COMPUTER_INSTALL_DIR` | Where `raft-computer` and its sidecar are published. Default `~/.local/bin`. |
| `RAFT_COMPUTER_RELEASE_BASE` | CDN holding `<version>/manifest.json` and artifacts. |
| `RAFT_COMPUTER_HANDS_ORIGIN`, `RAFT_COMPUTER_HANDS_APP` | The release authority a channel is resolved through. |
| `RAFT_COMPUTER_INSTALLER_VERSION`, `RAFT_COMPUTER_INSTALLER_RELEASE_BASE` | Which installer the bootstrap fetches and from where. |
| `RAFT_COMPUTER_INSTALLER_NODE` | Node 24+ used to run the installer. |

Exit codes: 0 promoted, up to date or installed; 1 failed or rolled back;
2 held or refused; 3 unresolved. Every run prints one line.

## Pieces

| File | Role |
|---|---|
| `install.sh` | Bootstrap: download one pinned installer release, verify `SHA256SUMS`, exec it. No install logic. Prefers the single executable for this machine; falls back to Node 24 and the portable files. |
| `native/<platform>/raft-computer-installer` | The entry as a single executable (Node SEA): presence, version resolution and Hands/CDN identity check, consent, settle, read the world, install/adopt/repair, one line and one exit code. Supervises the runner through K's launcher. |
| `native/<platform>/raft-computer-installer-runner` | K plus the Computer adapter as a single executable. Serves one request on stdin under K's lock; verified and retained by the supervisor for recovery. |
| `cli.cjs`, `runner.mjs` | The same two, portable, for machines without a published executable. Need Node 24. |

The adapter drives Computer through its CLI on `PATH`: `stop`, `start`, and
`status --json`, whose `attestation` carries `servicePid`,
`computerVersion` and `serviceGeneration`, the start id K compares across the
handover. Slot bytes are published onto `PATH` atomically before `start`;
nothing runs from inside a slot. The `photon_rs_bg.wasm` sidecar is verified
against the release manifest, kept per version under the installer's state,
and published beside the binary with the slot it belongs to.

State: K owns `<home>/computer/k`. The installer owns
`<home>/computer/installer/{receipts,scratch,sidecars,quarantine}`.

## Develop

```
npm ci
npm run typecheck
npm run build        # dist/cli.cjs, dist/runner.mjs, dist/install.sh, SHA256SUMS
npm run build:native # plus dist/native/<this platform>/raft-computer-installer{,-runner}
npm test             # real processes against a fake Computer: unattended, fresh, managed,
                     # replay, rollback, downgrade, adopt, foreign manager, broken and repair
npm run test:native  # the same suite driving the single executables
```

A tag `v*` runs `.github/workflows/release.yml`: one job per platform builds
and tests the executables, then one job assembles the portable files and
every executable under one `SHA256SUMS` and publishes a GitHub prerelease.
Mirror the assets under `RAFT_COMPUTER_INSTALLER_RELEASE_BASE`, keeping the
`native/<platform>/` layout; the bootstrap never runs an unverified file.

## Not yet

- `raft-computer upgrade` in Computer itself: it should ask, then run this
  bootstrap unattended with `--version`.
- Windows: `install.ps1` was removed until the entry is ported.
- macOS executables are ad-hoc signed; set `RAFT_CODESIGN_IDENTITY` in the
  build for a Developer ID signature.
