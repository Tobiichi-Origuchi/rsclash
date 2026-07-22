# Contributing to rsclash

rsclash currently targets Linux. Keep platform-specific behavior behind traits and `cfg`
boundaries so Windows and macOS implementations can be added without changing domain, Mihomo,
configuration, or UI logic.

## Toolchain and system dependencies

The repository pins Rust 1.92.0, which is also the minimum supported Rust version (MSRV). Rustup
will select it automatically from `rust-toolchain.toml`.

On Debian or Ubuntu, install the current Linux build dependencies with:

```shell
sudo apt-get install \
  build-essential \
  libwayland-dev \
  libxkbcommon-dev \
  pkg-config \
  shellcheck
```

The Linux tray uses the StatusNotifierItem and DBusMenu protocols directly through `ksni`; it does
not link GTK, GLib, or AppIndicator. The desktop session must provide an SNI host, such as KDE
Plasma, a compatible Waybar or Quickshell tray, or GNOME with an AppIndicator extension.

Install the pinned repository tools with:

```shell
cargo install --locked taplo-cli --version 0.10.0
cargo install --locked cargo-deny --version 0.20.2
```

Install actionlint 1.7.10 from its official release and place the `actionlint` binary on `PATH`.

## Required checks

Run the complete local gate before committing:

```shell
./scripts/check.sh
```

The script checks Rust and TOML formatting, TOML validity, shell scripts, GitHub Actions workflows,
compilation of every target and feature, strict Clippy lints, unit and documentation tests, rustdoc
warnings, dependency advisories, licenses, duplicate versions, and dependency sources.
`cargo-deny` refreshes its advisory database, so the complete check may require network access.

Use the real Mihomo integration suite after changing the client or runtime deployment code:

```shell
./scripts/test-mihomo-integration.sh
```

The integration script downloads a pinned Mihomo archive, verifies its SHA-256 digest, and caches it
outside the repository.

## Code standards

- Format Rust and TOML with the checked-in configuration. Indentation is two spaces.
- Keep all code comments and documentation comments in English.
- Treat warnings as errors and do not weaken workspace lints for an entire crate.
- A narrow lint allowance must be placed at the smallest practical scope and include an English
  `reason`.
- Unsafe Rust is forbidden in workspace crates.
- Avoid blocking work in the egui render path. Keep UI state separate from operating-system,
  filesystem, process, and network operations.
- Preserve Linux support and keep new platform implementations replaceable behind existing
  abstractions.
- Keep commits atomic, use a short English imperative message, and commit each meaningful task.

## Dependency and MSRV policy

Dependencies must be declared in the workspace when shared and must use an exact compatible lower
bound rather than a wildcard. The lockfile is committed, and CI commands use `--locked`.

`deny.toml` rejects unknown registries, Git dependencies, wildcard dependency requirements, known
vulnerabilities, yanked crates, and unapproved licenses. Every advisory or license exception must be
narrow, documented, and removed as soon as the transitive dependency permits it. Stale exceptions
are errors.

Rust 1.92 is the verified dependency floor for egui/eframe 0.35. Raising the MSRV must be a deliberate
standalone change: update `Cargo.toml`, `rust-toolchain.toml`, and `.clippy.toml`; pass the complete
gate on the new version; and demonstrate that the immediately preceding version cannot resolve or
build the selected dependency graph. CI also checks the latest stable compiler to detect upcoming
compiler and lint changes early.
