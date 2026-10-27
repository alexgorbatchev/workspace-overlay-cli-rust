`workspace-overlay` lets developers share agent instructions, skills and other files across projects and Git worktrees. It presents the combined files in each project while keeping edits in their original project or overlay source.

# What It Does

- **Share files:** Apply ordered overlay directories to selected projects and their linked worktrees.
- **Edit contributions:** Save each marked section back to its own source, including ordinary editor saves.
- **Keep Git accurate:** Show Git the underlying project files and temporarily exclude files supplied only by overlays.
- **Recover interrupted sessions:** Inspect mounts, stop owners and recover recorded mounts and managed exclusions.

# How It Works

- Write `workspace-overlay.toml` beside the projects and shared source directories.
- Run `workspace-overlay overlay mount` there to serve the combined view in the foreground.
- Read and edit the project normally. Colliding text contributions appear in declaration order; supported file types include section markers identifying their sources.
- Run `workspace-overlay overlay status` to inspect targets and `workspace-overlay overlay unmount` to stop them, or interrupt the foreground command.

# How it Really Works

- Configured paths resolve relative to the configuration file's directory. Git worktree discovery stays enabled unless `--worktrees=false` is supplied.
- Project-only files stay in the project. Overlay-only files stay in their source. Marked documents save each contribution separately; invalid or reordered markers fail instead of overwriting the sources.
- Binary collisions produce an explanation rather than concatenated binary data. Overlay files remain protected against deletion through the mounted view.
- Source changes and Git worktree registration changes update the view. Removed linked worktrees are unmounted without stopping the primary project.
- Recovery records live under `$XDG_STATE_HOME/workspace-overlay/`, falling back to `~/.local/state/workspace-overlay/`. Owned Git exclusion blocks are removed after all associated mounts are released; unresolved cleanup retains the records.
- A busy mount stays mounted while its holder processes keep running. A human using terminal stdin and stderr can authorize stopping the listed processes. An input failure or an interrupt during that question declines authorization; agent mode never prompts. Release the holders and run `overlay unmount` to recover.
- Status writes tab-separated paths and filesystem types to stdout in both modes. Progress and errors go to stderr; displayed home paths use `~`. `AGENT=1` gives compact help. Success and clean shutdown exit 0; argument, operation and incomplete-cleanup failures exit 1.
- All operations are local. The tool makes no cloud requests and requires no account or credentials.

# Prerequisites

- Linux with accessible `/dev/fuse` and permission to mount user FUSE filesystems.
- [Git](https://git-scm.com/) for project metadata and worktree discovery.
- [findmnt](https://github.com/util-linux/util-linux) and [fusermount3](https://github.com/libfuse/libfuse) on `PATH` for mount inspection and ordinary unmounts.
- [fuser from psmisc](https://gitlab.com/psmisc/psmisc) on `PATH` to identify processes holding busy mounts.

# Installation

Rust artifacts are unpublished. This independent project has no Rust release download; existing Go release archives contain the Go implementation. Local development commands are in [AGENTS.md](AGENTS.md).

# Quick Start

With `workspace-overlay` available on `PATH`, create `workspace-overlay.toml` in a directory containing `alpha` and `.ai/shared`:

```toml
version = 1

[projects.alpha]
path = "alpha"

[[overlays]]
name = "shared"
source = ".ai/shared"
projects = ["alpha"]
```

```sh
workspace-overlay overlay status
workspace-overlay overlay mount --project alpha
# From another terminal:
workspace-overlay overlay status
workspace-overlay overlay unmount --project alpha
```

Sample status output for a mounted project under `~/workspaces/demo`:

```text
~/workspaces/demo/alpha	fuse.workspace-overlay
```

Read the complete operational guide with `AGENT=1 workspace-overlay skill`.

# Options & Flags

| Flag | Short | Default | Description |
| :--- | :--- | :--- | :--- |
| `--help` | `-h` | `false` | Show help for the current command |
| `--version` | `-v` | `false` | Print the raw root-command version |

`workspace-overlay overlay mount`, `unmount` and `status`:

| Flag | Short | Default | Description |
| :--- | :--- | :--- | :--- |
| `--config <PATH>` | — | empty | Discover the nearest `workspace-overlay.toml` in the current directory or its parents |
| `--project <NAME>` | — | empty | Select all configured projects, or the supplied project name |
| `--worktrees[=true|false]` | — | `true` | Discover Git worktrees; recorded mounts remain available to status and unmount |

`workspace-overlay overlay mount`:

| Flag | Short | Default | Description |
| :--- | :--- | :--- | :--- |
| `--replace` | — | `false` | Stop selected existing overlay owners and recover registrations before mounting |

# License

[MIT](LICENSE), copyright 2026 Alex Gorbatchev.
