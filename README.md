# Raft Computer Installer

An independent installer, upgrader and repair tool for Raft Computer, built on
[K](https://github.com/botiverse/k-carrier). The installer can be released without
releasing Computer and must remain usable when Computer cannot start.

This repository is being established. It does not yet contain a runnable installer
or published artifacts. The existing scaffold is in
[Computer PR #7537](https://github.com/botiverse/slock/pull/7537); migration includes
removing its source imports from the Computer monorepo.

## Scope

- A thin installation script that obtains, verifies and starts the installer.
- The Computer adapter: release lookup, installation ownership, service lifecycle,
  health checks, and product-specific setup and repair.
- Installer builds, independent release metadata and platform acceptance tests.

K supplies the upgrade transaction, executable slots, journal and recovery.
Computer exposes lifecycle and health controls. Credentials, agent configuration
and workspaces must survive installation and repair; executable rollback does not
undo application data migrations.

This project is intended to serve as a real product integration of K's external
runner, alongside K's smaller [service example](https://github.com/botiverse/k-carrier/tree/archer/external-runner-17/examples/external-service).
