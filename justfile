# HARDWARIO TOWER monorepo: task runner (https://just.systems)
#
# Two Cargo workspaces live here: the HOST workspace (this directory: crates/*, cli, hil) and the
# FIRMWARE workspace (firmware/, thumbv6m + flip-link via firmware/.cargo/config.toml). The host
# recipes below run cargo from this directory; the firmware ones delegate to firmware/justfile,
# whose recipes run inside firmware/ so its cargo config applies.
#
#   just check                     # every hardware-free gate CI runs (both workspaces)
#   just test                      # host workspace tests
#   just fw build example blinky   # any firmware/justfile recipe (`just fw` lists them)
#   just hil check                 # any hil/justfile recipe (`just hil` lists them)
#
# Nothing here flashes hardware except what you explicitly forward to `fw` (flash/run) or `hil`
# (the hil* bench recipes).

# On Windows, run recipe lines with cmd.exe (propagates child exit codes); Unix keeps `sh`.
set windows-shell := ["cmd.exe", "/c"]

# The running `just`, so the delegating recipes use the same binary (unquoted: cmd.exe on Windows
# does not understand sh-style quoting).
just := just_executable()

# List the available recipes.
default:
    @{{just}} --list


# === Gates ========================================================================================

# The hardware-free gates of both workspaces and the RustSec audit: what CI runs (plus a real
# `just fw build example blinky`, which also needs cargo-binutils). Needs flip-link,
# cargo-binutils, cargo-audit, python3 >= 3.11 and network access for the advisory DB.
# Run every gate (host, firmware, audit).
check: check-host check-firmware audit

# `tower-hil --features power` adds the PPK2 power group (compiled, never run). The no_std build
# proves the shared crates still build for the MCU on their own (the firmware job builds them with
# the SDK's feature set). Rustdoc covers the libraries; cli and hil are binaries/harness.
# Host workspace gates: fmt, clippy, tests, the no_std build of the shared crates, rustdoc.
check-host: doc test-sha2-compact
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo clippy -p tower-hil --all-targets --features power --locked -- -D warnings
    cargo test --workspace --locked
    cargo test -p tower-hil --features power --no-run --locked
    cargo build --locked --target thumbv6m-none-eabi -p tower-protocol -p tower-kv -p tower-net -p tower-radio -p tower-gateway -p tower-shell

# Firmware gates: `ci` is exactly CI's firmware job; the blinky build also exercises the
# `cargo objcopy` path `just fw flash` uses.
# Firmware workspace gates (fmt, MCU build, RAM budget, clippy, role variants, rustdoc, lists).
check-firmware:
    {{just}} -f firmware/justfile ci
    {{just}} -f firmware/justfile build example blinky

# Vulnerabilities fail; informational warnings (unmaintained, yanked) do not.
# RustSec audit of all three lockfiles (host workspace, firmware, fuzz).
audit:
    cargo audit
    cargo audit --file firmware/Cargo.lock
    cargo audit --file fuzz/Cargo.lock

# Build the host libraries' rustdoc, failing on any warning.
[env("RUSTDOCFLAGS", "-D warnings")]
doc:
    cargo doc --locked --no-deps --workspace --exclude tower-cli --exclude tower-hil

# The firmware builds SHA-256 with sha2's compact software backend (the `sha2_backend_soft` cfg
# in firmware/.cargo/config.toml), which no other host build exercises: run the pairing kernel's
# tests (RFC 5869, the handshake goldens) on exactly that backend, in a target dir of its own so
# the cfg does not invalidate the main build. CI's host job runs the same.
# Run tower-net's tests on the firmware's SHA-256 backend (soft, compact).
[env("RUSTFLAGS", '--cfg sha2_backend="soft" --cfg sha2_backend_soft="compact"')]
[env("CARGO_TARGET_DIR", "target/sha2-compact")]
test-sha2-compact:
    cargo test -p tower-net --locked

# Extra args go to `cargo test`, e.g. `just test -p tower-protocol`.
# Run the host workspace tests.
test *args:
    cargo test --workspace --locked {{args}}

# Apply rustfmt to both workspaces (one root rustfmt.toml covers everything).
fmt:
    cargo fmt --all
    {{just}} -f firmware/justfile fmt


# === Components ===================================================================================

# Build and install the `tower` CLI from this checkout (rebuild it after any wire change).
install-cli:
    cargo install --locked --path cli

# e.g. `just fw examples`, `just fw build example blinky`, `just fw flash app radio_push_button`.
# Run a firmware/justfile recipe (inside firmware/).
fw *args:
    {{just}} -f firmware/justfile {{args}}

# e.g. `just hil check`. The `hil`, `hil-power` and `hil-full` recipes FLASH the bench.
# Run a hil/justfile recipe (inside hil/).
hil *args:
    {{just}} -f hil/justfile {{args}}


# === Fuzzing ======================================================================================

# The ONE nightly corner of the repo: cargo-fuzz needs nightly rustc flags (`cargo install
# cargo-fuzz --locked` + `rustup toolchain install nightly`). The working corpus grows in
# fuzz/corpus/<target> (git-ignored, created on demand); fuzz/seeds/<target> is the curated seed
# corpus, read but never written. A crash lands in fuzz/artifacts/<target>/ and fails the recipe.
# Fuzz one target for `seconds` (default 60), e.g. `just fuzz kv_store 600`.
fuzz target seconds="60":
    cargo +nightly fuzz run {{target}} fuzz/corpus/{{target}} fuzz/seeds/{{target}} -- -max_total_time={{seconds}} -timeout=10 -create_missing_dirs=1

# Fuzz every target in turn for `seconds` each (default 60); stops at the first crash.
fuzz-all seconds="60":
    {{just}} fuzz protocol_frames {{seconds}}
    {{just}} fuzz protocol_radio {{seconds}}
    {{just}} fuzz kv_store {{seconds}}
    {{just}} fuzz shell_kernels {{seconds}}
    {{just}} fuzz radio_kernels {{seconds}}
