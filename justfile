mod workspace_overlay 'crates/workspace-overlay/justfile'

set dotenv-load := false

default:
    @just --list

build:
    cargo build --package workspace-overlay-cli --release --bins --locked
    mkdir -p bin
    cp target/release/workspace-overlay target/release/workspace-overlay-fixture bin/

run *args:
    cargo run --package workspace-overlay-cli --bin workspace-overlay --locked -- {{ args }}

run-ai *args:
    AGENT=1 cargo run --package workspace-overlay-cli --bin workspace-overlay --locked -- {{ args }}

dev *args:
    cargo run --package workspace-overlay-cli --bin workspace-overlay-fixture --locked -- dev-workspace
    cargo run --package workspace-overlay-cli --bin workspace-overlay --locked -- overlay mount --config dev-workspace/workspace-overlay.toml --replace {{ args }}

stop *args:
    cargo run --package workspace-overlay-cli --bin workspace-overlay --locked -- overlay unmount --config dev-workspace/workspace-overlay.toml {{ args }}

status *args:
    cargo run --package workspace-overlay-cli --bin workspace-overlay --locked -- overlay status --config dev-workspace/workspace-overlay.toml {{ args }}

# Exercise disposable real workspaces without resetting an edited dev-workspace.
test-local *args:
    cargo test --package workspace-overlay-cli --test runtime --locked -- {{ args }}

[private]
_test:
    cargo test --package workspace-overlay-cli --all-targets --locked
    cargo test --package workspace-overlay-cli --doc --locked

[private]
_fmt:
    cargo fmt --package workspace-overlay-cli -- --check

[private]
_lint:
    cargo clippy --package workspace-overlay-cli --all-targets --locked -- -D warnings

coverage:
    cargo llvm-cov --package workspace-overlay-cli --exclude-from-report workspace-overlay --all-targets --locked --fail-under-lines 90

check: _fmt _lint _test coverage
    just workspace_overlay check

test: _test
    just workspace_overlay test

lint: _lint
    just workspace_overlay lint

fmt: _fmt
    just workspace_overlay fmt-check
