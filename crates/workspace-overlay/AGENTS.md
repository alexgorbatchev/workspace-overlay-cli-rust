---
created_on: 2026-10-14 21:27
last_modified: 2026-10-14 22:40
status: current
---

# workspace-overlay

Linux native FUSE overlays and recoverable Git worktree sessions. This internal workspace package belongs to `workspace-overlay-cli`; keep library APIs independent of CLI parsing and output. Inherit `../../AGENTS.md` and run local recipes from this package directory.

## Commands

- Build: `just build`.
- Test: `just test`; native integration cases require accessible `/dev/fuse`, Git, findmnt, fusermount3, and psmisc fuser.
- Format: `cargo fmt --package workspace-overlay`.
- Strict checks and coverage: `just check`.
- Keep temporary files under `.tmp/`; tests create isolated projects and state directories.

## Native contracts

- Open project, source, Git metadata and exclusion directory capabilities before any mount. A serving process must not access its own mounted path during startup or management; native test clients are deliberate controlled exceptions.
- Preserve native non-UTF8 paths at filesystem, subprocess, registry and mount boundaries. Human path formatting belongs to the consumer.
- Git caller identity is the requesting thread's `/proc/<tid>/comm`. Keep project-only Git reads separate from merged ordinary reads, including attribute and negative-entry cache decisions.
- Merge project bytes first and overlays in declaration order. Marked saves preserve all sections; unmarked saves preserve earlier bytes. Existing overlay sources remain protected while newly created overlay entries retain session identity.
- Keep descriptor stats valid after unlink and rename. Open descriptions own native locks and allocation operations; distinguish `FUSE_LK_FLOCK` from POSIX record locking.
- Preserve ordinary retryable unmounts. Never force or lazily detach, unmount foreign filesystem types, or discard recovery records and Git blocks after an unmount failure.
- Registry owner acquisition must validate the locked inode against the current pathname after locking. Remove the owner lock while holding it and retain independent cleanup failures.
- Recovery restores only an exact owned exclusion block, after every mount is released. Keep failed or externally modified blocks available for explicit recovery.
- Worktree reconciliation is event-driven with bounded debounce, retry and fallback. Unsupported registrations are skipped; removed registrations release suppression so re-registration can be considered again.
- Native watchers own their callback lifecycle and must disable metadata timeouts on failure. Cache only caller-invariant results, including possible future overlay contributions.
- Preserve vendored fuser source attribution and document every semantic extension in `../../third_party/fuser/PATCHES.md`; consult its nested instructions before edits.

## Verification and boundaries

- Runtime changes require corresponding test changes, red/green evidence, a genuine behavior-disable/restoration proof and at least 90% line coverage; scripts are excluded.
- Use real local FUSE, filesystem, Git and subprocess behavior. Do not replace supported native operations with stubs or claim other-platform runtime verification from Linux tests.
- Retain typed primary and cleanup errors, child ownership, cancellation and partial write outcomes. Explicit close failures are observable; do not retry a failed Linux close on a descriptor number that may have been reused.
- Rust artifacts are unpublished. Never publish releases, tags, packages or production deployments automatically without explicit authorization.
- Persist only standing project rules here; keep task history and verification logs under `.tmp/`.
