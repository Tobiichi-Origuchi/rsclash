#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "${ROOT_DIR}"

required_commands=(actionlint cargo shellcheck taplo)
for command_name in "${required_commands[@]}"; do
  if ! command -v "${command_name}" >/dev/null 2>&1; then
    echo "Required command not found: ${command_name}" >&2
    exit 127
  fi
done

required_cargo_subcommands=(cargo-deny cargo-machete)
for cargo_subcommand in "${required_cargo_subcommands[@]}"; do
  if ! "${cargo_subcommand}" --version >/dev/null 2>&1; then
    echo "Required Cargo subcommand not found: ${cargo_subcommand}" >&2
    exit 127
  fi
done

cargo fmt --all -- --check
taplo format --check
taplo lint
shellcheck scripts/*.sh
actionlint
cargo machete --with-metadata
cargo check --locked --workspace --all-targets --all-features
cargo clippy --locked --workspace --all-targets --all-features
cargo test --locked --workspace --all-targets --all-features
cargo test --locked --workspace --doc --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --all-features --no-deps
cargo deny -L error --locked check
