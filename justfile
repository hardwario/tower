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

# `tower-hil --features power` adds the PPK2 power group (compiled, never run). The fuzz targets
# (their own workspace, fuzzed on nightly) are checked on the stable pin, so a wire or API change
# that breaks one fails here on the day of the change, as in CI. The no_std build proves the
# shared crates still build for the MCU on their own (the firmware job builds them with the SDK's
# feature set). Rustdoc covers the libraries; cli and hil are binaries/harness.
# Host workspace gates: fmt, clippy, tests, fuzz targets compile, the no_std build, rustdoc.
check-host: doc test-sha2-compact
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo clippy -p tower-hil --all-targets --features power --locked -- -D warnings
    cargo test --workspace --locked
    cargo test -p tower-hil --features power --no-run --locked
    CARGO_TARGET_DIR=target/fuzz-check cargo check --locked --manifest-path fuzz/Cargo.toml --bins
    cargo build --locked --target thumbv6m-none-eabi -p tower-protocol -p tower-kv -p tower-net -p tower-radio -p tower-gateway -p tower-shell -p tower-hmi -p tower-strip -p tower-taf -p tower-modules

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

# e.g. `just fw examples`, `just fw build example blinky`, `just fw flash app radio_button`.
# Run a firmware/justfile recipe (inside firmware/).
fw *args:
    {{just}} -f firmware/justfile {{args}}

# e.g. `just hil check`. The `bench`, `bench-power` and `bench-full` recipes FLASH the bench.
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


# === Mutation testing =============================================================================

# cargo-mutants (`cargo install --locked cargo-mutants`) changes the code of a crate one small
# piece at a time (an operator, a constant returned in place of a result) and runs that crate's
# tests on each change: a mutant no test fails on is code the tests do not hold. It is slow (a
# build and a test run per mutant: about an hour for the eight crates at 4 jobs), so it is
# not part of `check`; `.github/workflows/mutants.yml` runs it weekly. The mutants known to change
# nothing are listed, with the reason, in `.cargo/mutants.toml`. Results land in
# `target/mutants/<crate>/mutants.out/` (`missed.txt`, `caught.txt`, `timeout.txt`, the diffs).
#
# The recipe fails when a mutant SURVIVES (or the tool itself fails). A mutant whose test run
# hangs (an offset that stops advancing loops for ever) is killed by cargo-mutants' timeout: that
# counts as caught, though cargo-mutants' own exit status calls it out. `CARGO_MUTANTS_JOBS`
# sets the parallel jobs (default 4). docs/test-strategy.md, layer 6, has the last results.
# Mutation-test the shared kernels: `just mutants` (all eight) or `just mutants kv`.
[script("python3")]
mutants *crates="net kv protocol gateway radio shell taf modules":
    import os, pathlib, subprocess, sys
    os.environ.setdefault("CARGO_MUTANTS_JOBS", "4")
    survived = False
    summary = []
    for crate in "{{crates}}".split():
        out = pathlib.Path("target/mutants") / crate
        out.mkdir(parents=True, exist_ok=True)
        code = subprocess.run(["cargo", "mutants", "--package", f"tower-{crate}", "--output", str(out)]).returncode
        # 0: every mutant caught; 2: some survived; 3: some test runs timed out (and, possibly,
        # some survived). Anything else is the tool or the unmutated tree failing.
        if code not in (0, 2, 3):
            sys.exit(f"cargo mutants -p tower-{crate} failed (exit status {code})")
        count = lambda name: len((out / "mutants.out" / name).read_text().splitlines())
        caught, missed, timeout, unviable = (count(f"{n}.txt") for n in ("caught", "missed", "timeout", "unviable"))
        summary.append(f"tower-{crate}: {caught + missed + timeout + unviable} mutants, {caught} caught, "
                       f"{timeout} caught by the timeout, {unviable} unviable, {missed} MISSED")
        survived |= missed > 0
    print("\n" + "\n".join(summary))
    if survived:
        sys.exit("surviving mutants: see target/mutants/<crate>/mutants.out/missed.txt (and diff/)")
