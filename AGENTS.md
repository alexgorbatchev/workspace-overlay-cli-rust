---
created_on: 2026-10-14 21:55
last_modified: 2026-10-14 22:40
status: current
---

# workspace-overlay-cli

Linux CLI consumer of the internal `crates/workspace-overlay` package in this repository's Cargo workspace. Keep filesystem, Git, mount and registry behavior in that library.

## Commands

- Build local unpublished release binaries: `just build`; outputs `bin/workspace-overlay` and developer helper `bin/workspace-overlay-fixture`.
- Run: `just run overlay status`; compact mode: `just run-ai overlay status`.
- Reusable demonstration workspace: `just dev`, `just status`, `just stop`; `just dev --project alpha` narrows the foreground session.
- Native disposable end-to-end tests: `just test-local`.
- All target and documentation tests: `just test`.
- Formatting: `cargo fmt --package workspace-overlay-cli`.
- Strict formatting, Clippy, tests and line coverage: `just check`.

## Setup

- Use Rust 1.98+, Cargo, just and cargo-llvm-cov with LLVM tools. Runtime tests require Linux, accessible `/dev/fuse`, Git, findmnt, fusermount3 and psmisc fuser.
- The workspace contains the CLI and `crates/workspace-overlay`; shared dependencies outside this repository remain native path dependencies. Scope formatters to the affected package.
- Rust artifacts are unpublished. Keep development builds local; never replace canonical Go release or dotfiles shim payloads through development recipes.

## CLI boundaries

- Preserve subject-first `overlay mount`, `overlay unmount`, `overlay status`, root raw `--version` / `-v` and embedded `skill`. The source project's explicit embedded-guide contract overrides generic CLI skill-install output guidance.
- Keep the guide at `skills/workspace-overlay/SKILL.md` as the only text source; embed it with `include_str!`. Synchronize it in the same change as commands, arguments, flags, defaults, environment, output or side effects. Test generated help queries and the live command interface.
- Print returned errors exactly once in the executable's `main`. Parse failures print usage before the error; runtime errors omit usage. Retain independent operation and signal-thread close failures.
- Route human display paths and messages through `cli-command-kit`. Preserve native `OsString` and `PathBuf` filesystem arguments. Status is TSV in both output modes.
- The developer fixture helper emits an exact native absolute configuration pathname followed by newline for machine consumption; keep that machine pathname unfolded. Its optional directory defaults to `dev-workspace`; its help, raw version and embedded skill remain offline.
- Prompt only for human mode when stdin and stderr are terminals. Read failures and signals arriving during a prompt must decline authorization. Register each prompt's signal input separately: a previous interrupt may already have requested session cleanup and must still allow its question to be shown.
- Keep signal registrations, delivery threads and child processes owned and joined. A busy cleanup failure must retain library recovery records and leave holder processes running unless the user confirms termination.
- Native tests must use disposable projects, child processes and PTYs. Preserve edited `dev-workspace` fixtures; never reset or clean a user's reusable demonstration repository in the test recipe.

## Verification and boundaries

- Runtime changes require corresponding test-file changes, failing-before/passing-after evidence, a genuine behavior-disable/restoration proof and at least 90% line coverage; scripts are excluded.
- Use the parent build slots and storage policy. Keep evidence in `.tmp/`, binaries in ignored `bin/`, and active incremental artifacts intact.
- Do not force or lazily detach mounts, terminate unrelated processes, modify excluded dotfiles, publish releases/tags/packages/deployments automatically, or claim macOS runtime support. Publication requires explicit authorization.
- Persist standing project rules here; keep task history, progress and test results outside agent instructions.
