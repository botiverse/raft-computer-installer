# Native installer verification

The drivers use Python's standard library and real Rust executables. There is no
Node runtime, package install, JS wrapper or source-text assertion in the core
verification path. Run against the committed `Cargo.lock`; source entry points
alone do not prove a platform passed:

```sh
python3 scripts/verify.py
python3 scripts/verify.py --real
```

The driver collects formatting, Clippy, Rust behavior and Python syntax failures, builds the
installer and fixture, then runs every runnable native scenario. A failed build
blocks process scenarios and is reported as such. Consolidate failures by root
cause, finish the whole repair batch, then rerun failed and affected checks. Do
not repeatedly run one check after each small edit or claim an unrun case passed.

## What runs

`fixture.rs` is a native Computer-shaped executable with independent versions and
behaviors embedded in its bytes. It implements login, start, stop, status and a
real TCP service with a per-start identity. The Python harness serves immutable
fixture releases and release-authority metadata on loopback, and launches the
actual installer or shell/PowerShell bootstrap in isolated temporary homes.

`test_native.py` covers unattended consent, exact and channel targets, stopped and
running upgrades, adoption, replay and conflicting IDs, current status versus
historical receipts, downgrade policy, pre-handover candidate rejection, live
rollback, sidecars, forced-stop identity, external-manager ownership, damaged K
state and repair, failed-repair preservation, proxy/NO_PROXY, bootstrap pinning,
integrity, cleanup and a PATH without Node. Worker-kill scenarios interrupt a real
handover and require supervisor recovery to restore the original running/stopped
mode. First setup uses a real Unix PTY or a Windows console.

Windows PATH mutation runs only in a disposable GitHub Actions account, saves the
prior registry value and restores it afterwards. On other Windows hosts that
case is explicitly skipped; a local run therefore does not prove it passed.
macOS fixture binaries are re-signed after their embedded version changes.

`real.py` exercises **actual published Computer binaries**, including their real
WASM sidecar, from the public authority and CDN. Only the locally built installer
is served on loopback. It installs an older release, upgrades, checks up-to-date
and downgrade policy, adopts pre-K bytes and repairs unreadable K state. The
default chooses the current stable channel and an earlier published patch; use
`--current X.Y.Z --older X.Y.Z` when those defaults cannot select a pair. Metadata
and network failures fail the check rather than silently substituting fixtures.

These product runs never log in and assert that no service starts. They do not
prove authenticated live-service behavior. Fixture lifecycle tests prove actual
process behavior against the fixture contract; they do not replace real product
or target-platform verification. A failed real-product cleanup retains its own
temporary home for inspection instead of deleting state underneath a service.

Linux's Rust process test creates a same-user non-dumpable child. Inventory must
skip its inaccessible executable while a previously recorded identity remains
unresolved, never falsely declared exited. Native protocol tests repeatedly
exercise short-lived worker/controller responses and preserve uncertain host
results. Publisher tests verify immutable redirects, byte identity and separate
public requests from authenticated API calls.

## Evidence and cleanup

Record commands, commit, OS/architecture, failures, relevant receipts and cleanup
results in the shared [alignment progress document](https://github.com/botiverse/elephant/blob/rust-native/docs/installer-alignment.zh-CN.md).
Do not put login material, user terminal output or credentials in reports. The
harness stops only its isolated services and removes its own temporary files.
Kill cases target identities created by the fixture; never point them at a user's
installed Computer or a production state directory.

The old TS suites and JS fake-release server have been replaced. In particular,
one JS product build relabeled as multiple versions is no longer accepted as
real-product evidence: each candidate must report the version of its actual
native artifact. Published-product cold paths and the native fault fixture are
reported separately.

## Manual authenticated acceptance

See [manual-authenticated-upgrade.md](manual-authenticated-upgrade.md) for the
not-yet-executed real-agent upgrade checklist, including Computer API and
Electron entry points. Automated cold-path results do not satisfy this checklist.

## Version matrix design

[Version matrix and rationale](version-matrix.md) defines fixed historical
baselines, candidate/latest targets, platform coverage and execution tiers.
It is a design; the current CI still uses the adjacent-release default described above.
