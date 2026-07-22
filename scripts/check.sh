#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "${ROOT_DIR}"

required_commands=(cargo taplo)
for command_name in "${required_commands[@]}"; do
  if ! command -v "${command_name}" >/dev/null 2>&1; then
    echo "Required command not found: ${command_name}" >&2
    exit 127
  fi
done

if ! cargo deny --version >/dev/null 2>&1; then
  echo "Required Cargo subcommand not found: cargo-deny" >&2
  exit 127
fi

cargo fmt --all -- --check
taplo format --check
taplo lint
cargo check --locked --workspace --all-targets --all-features
cargo clippy --locked --workspace --all-targets --all-features
cargo test --locked --workspace --all-targets --all-features
cargo test --locked --workspace --doc --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --all-features --no-deps
cargo deny -L error --locked check
