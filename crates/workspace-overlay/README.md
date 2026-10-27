`workspace-overlay` combines project files and ordered shared sources into writable Linux workspace mounts. Applications use it to share project guidance and skills across checkouts and Git worktrees while keeping each contribution in its original directory.

# What It Does

- **Layered files**: Joins text in declaration order, merges directories, and explains binary collisions without overwriting their sources.
- **Writable contributions**: Saves unmarked edits into the last contribution and marked sections into their respective source files, including editor save-and-rename operations.
- **Git isolation**: Gives Git callers the project backing view and manages exact, independently owned exclusion blocks for overlay-only paths.
- **Recoverable sessions**: Watches worktree registrations, retains failed cleanup records, reports busy processes, and supports explicit abandoned-session recovery.

# How It Works

1. Load a workspace configuration and select its projects.
2. Open backing directories before mounting over the project paths.
3. Read combined files and save edits through each mounted project or worktree.
4. Cancel the owning session to unmount and restore its Git exclusions, or recover an interrupted session explicitly.

# How it Really Works

- Calls are synchronous. A serving session owns its watcher and FUSE threads and blocks until cancellation, external unmount, or failure. Importing the package creates no mounts or processes.
- Project bytes come first, followed by matching overlays in configuration order. Existing overlay source entries cannot be deleted through a mount. Entries created through a mount remain removable for that mount session.
- Marked text preserves every section, including text outside section delimiters. Unmarked writes must preserve earlier contributions. A failed write may leave partial bytes; failed restoration is returned alongside the original native failure.
- Metadata timeouts apply only to caller-invariant entries and attributes. Native backing watches invalidate cached rows; watcher failures disable timeouts and remain observable through `View::take_cache_warning`.
- Opening a binary collision emits a warning through the Rust `log` interface for each open. The application configures the logger; metadata queries do not emit collision warnings.
- Ordinary unmount failures preserve registry records and exclusion blocks. A `Mounted` handle supports retry. A stopped owning session returns its cleanup failure; use `management::unmount` after releasing the reported holders to recover the retained state.
- Directory synchronization opens a readable descriptor through the retained backing capability. Synchronization and explicit close errors remain observable to the caller.
- Registry files use private permissions, atomic replacement, native pathname bytes, and inode-checked owner locks. Recovery restores exact owned Git blocks and refuses to overwrite blocks modified by another writer.
- Registry state defaults to `$XDG_STATE_HOME/workspace-overlay`, or `~/.local/state/workspace-overlay`. Configuration discovery walks ancestors for `workspace-overlay.toml`; relative configured paths resolve from the configuration directory.

# Prerequisites

- Linux with accessible `/dev/fuse` and a working [FUSE](https://github.com/libfuse/libfuse) installation.
- [Git](https://git-scm.com/), `findmnt`, `fusermount3`, and the psmisc `fuser` command on `PATH` for session and recovery operations.
- Rust 1.98 or later for local package consumers.

# Installation

This unpublished implementation crate belongs to the `workspace-overlay-cli` repository. From a consumer beside that repository:

```sh
cargo add workspace-overlay --path ../workspace-overlay-cli/crates/workspace-overlay
```

# Quick Start

With a valid `workspace-overlay.toml` in the current directory or an ancestor:

```rust,no_run
use workspace_overlay::{Cancellation, config::Configuration, registry::Store, session};

fn main() -> workspace_overlay::Result<()> {
    let config = Configuration::discover(None)?;
    let cancel = Cancellation::default();
    let stop = cancel.clone();
    session::serve(
        config.selections(None)?,
        session::Options::default(),
        Store::from_environment()?,
        &cancel,
        None,
        &mut |event| {
            if matches!(event, session::Event::Mounted { .. }) {
                stop.cancel();
            }
            Ok(())
        },
    )
}
```

This example cancels after the first successful mount so it demonstrates ownership and cleanup without leaving a persistent session.

# API

| Entry point | Purpose |
| :--- | :--- |
| `config::Configuration` | Load strict TOML, discover configuration, and select ordered sources |
| `fixture::create` | Create an isolated sample workspace with two Git projects, linked worktrees and ordered sources |
| `view::View`, `Mounted` | Open backing capabilities and explicitly own a native mount |
| `session::serve` | Supervise selected projects, worktree changes, diagnostics, and cleanup |
| `management::status`, `management::unmount` | Inspect known targets and stop or recover sessions |
| `registry::Store` | Select isolated durable ownership and recovery storage |
| `holders` | Inspect native mount holders and explicitly stop confirmed processes |
| `gitexclude::Exclude` | Own, update, and restore one exact advisory-locked Git exclusion block |
| `Cancellation` | Cancel owned operations and wake their supervised work |

# Configuration

| Setting | Default | Meaning |
| :--- | :--- | :--- |
| `version` | Required, `1` | Configuration schema |
| `defaults.collision` | `concat` | Supported collision behavior |
| `defaults.write` | `most-specific` | Supported writable-layer behavior |
| `projects.<name>.path` | Required | Project backing directory relative to the configuration root |
| `overlays` | Empty | Ordered named sources, project selectors, and path globs |
| `markers` | Common Markdown, HTML, hash and slash rules | Ordered custom rules take precedence over built-in rules |
| `session::Options::worktrees` | `true` | Include registered worktrees and reconcile changes |
| `session::Options::replace` | `false` | Release existing overlay sessions before mounting |

# Sample Workspace

`fixture::create(directory, &cancel)` returns the absolute path to a generated `workspace-overlay.toml`. The destination contains two committed Git projects, linked verification worktrees and shared guidance files. It reuses a completed fixture without rewriting user edits, and refuses unrelated, incomplete or damaged destinations. Cancellation or Git failure can leave partial files available for inspection; those files have no completion marker and cannot be reused as a completed fixture.

# License

[MIT](LICENSE), with retained fuser attribution and native contract patches in [../../third_party/fuser/PATCHES.md](../../third_party/fuser/PATCHES.md).
