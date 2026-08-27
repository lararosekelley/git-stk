# Development
# -----------

# Install dependencies
install: install-rust install-js

install-rust:
    cargo fetch

install-js:
    npm install

# Build release binary
build:
    cargo build --release

clean:
    cargo clean

# Check Cargo package contents
package:
    cargo package --no-verify

# Run publish dry-run checks
publish-dry-run:
    cargo publish --dry-run

# Plan release artifacts
dist-plan:
    dist plan

# Bump the crate version (patch|minor|major); commit and tag manually afterward
bump part:
    cargo run --features dev-tools --bin git-stk-bump -- {{part}}

# Cut a release (main only): bump the version, commit, tag, and push --follow-tags.
# part is major|minor|patch. Refuses to run off main or with a dirty tree.
release part:
    #!/usr/bin/env bash
    set -euo pipefail
    branch="$(git rev-parse --abbrev-ref HEAD)"
    if [ "$branch" != "main" ]; then
        echo "release: must be on 'main' (currently on '$branch')" >&2
        exit 1
    fi
    if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
        echo "release: tracked changes are uncommitted; commit or stash first" >&2
        exit 1
    fi
    cargo run --features dev-tools --bin git-stk-bump -- {{part}}
    # Read the freshly-bumped version from the [package] table only.
    version="$(awk -F'"' '/^\[/{p=($0=="[package]")} p && /^[[:space:]]*version[[:space:]]*=/{print $2; exit}' Cargo.toml)"
    if [ -z "$version" ]; then
        echo "release: could not read new version from Cargo.toml" >&2
        exit 1
    fi
    git add Cargo.toml Cargo.lock
    git commit -m "chore(release): bump to ${version}"
    # Annotated tag: --follow-tags only pushes annotated tags.
    git tag -a "v${version}" -m "v${version}"
    git push --follow-tags
    echo "released v${version}"

# Generate shell completions and man pages
generate-assets:
    cargo run --features generate --bin git-stk-generate -- all target/generated

# Generate shell completions
generate-completions:
    cargo run --features generate --bin git-stk-generate -- completions target/generated/completions

# Generate man pages
generate-man:
    cargo run --features generate --bin git-stk-generate -- man target/generated/man

# Testing
# -------

# Run all tests
test:
    cargo test --features test-fakes

# Code formatting
# ---------------

# Format and lint all
check: format lint test

# Format Rust code
format:
    cargo fmt

# Linting
# -------

# Lint with clippy and check formatting
lint: lint-rust lint-md

lint-rust:
    cargo fmt --check
    # Name the feature-gated binaries' features: a default --all-targets sweep
    # skips them, and git-stk-e2e has shipped broken because of it.
    cargo clippy --all-targets --features test-fakes,e2e,generate,dev-tools -- -D warnings

lint-md:
    npx markdownlint-cli2 "**/*.md"
