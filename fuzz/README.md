# fuzz: coverage-guided fuzzing of the shared crates

[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) + libFuzzer targets for the hardware-free
crates every TOWER end builds on: the wire protocol, the EEPROM key-value store, the shell kernels
and the radio timing / compliance / CCM logic. Each target feeds arbitrary input to the code and
asserts the properties the crate documents; a panic (a failed assertion, an overflow, an
out-of-bounds index), a hang or runaway memory is a finding.

This directory is a **standalone** package with its own workspace and its own `Cargo.lock`
(committed): the root host workspace lists it in `exclude`. It is the one part of the repo that
needs **nightly** (cargo-fuzz passes nightly-only sanitizer flags to rustc), so
`fuzz/rust-toolchain.toml` selects nightly; everything else stays on the pinned 1.99.1. The crates
under test are path dependencies, so a fuzz build always exercises the tree it sits in.

## Setup

```sh
rustup toolchain install nightly
cargo install cargo-fuzz --locked --version 0.13.2
```

## Running

From the repo root:

```sh
just fuzz kv_store            # one target, 60 s (the default)
just fuzz protocol_frames 600 # one target, 10 minutes
just fuzz-all 120             # every target in turn, 2 minutes each; stops at the first crash
```

`just fuzz` is `cargo +nightly fuzz run <target> fuzz/corpus/<target> fuzz/seeds/<target> --
-max_total_time=<seconds> -timeout=10 -create_missing_dirs=1`. libFuzzer reads both directories
and writes new interesting inputs only to the first one, `fuzz/corpus/<target>/` (git-ignored, it
grows across runs and makes later runs start deeper). Any libFuzzer flag can go after `--`, e.g.
`-jobs=8 -workers=8` for parallel runs or `-max_len=8192` for longer inputs.

`.github/workflows/fuzz.yml` runs every target for 60 s weekly (and on demand, with a chosen
duration) and uploads `fuzz/artifacts/<target>/` when one fails.

Other cargo-fuzz commands, from the repo root:

```sh
cargo +nightly fuzz build                        # build every target (a quick compile gate)
cargo +nightly fuzz list
cargo +nightly fuzz coverage kv_store fuzz/corpus/kv_store fuzz/seeds/kv_store  # needs llvm-tools on nightly
cargo +1.99.1 fmt --manifest-path fuzz/Cargo.toml   # the pinned rustfmt + the root rustfmt.toml
```

## The targets

| Target | Code under test | What it asserts |
|---|---|---|
| `protocol_frames` | `tower-protocol`: `FrameDecoder`, `decode_frame`, `decode_msg`, `encode_frame` | Raw mode: arbitrary bytes through one streaming `FrameDecoder`, as a host reads a serial port. Every deframed buffer: `decode_frame` follows its documented check order (length, then CRC, then version, then type; `BadVersion` only on a CRC-valid frame); `decode_msg` agrees with it and fails only with `Malformed` on a framing-valid frame; anything that decodes re-encodes (a too-small buffer is `Overflow`), deframes and decodes to the same value, deterministically. Nested payloads are decoded the way their consumer does (`Uplink.data` as a `NodeMsg`, a queued downlink as a `NodeCmd`, `MgmtResponse.data` as every record type). Framed mode: the fuzzer's payloads are wrapped by the real producer after a burst of line noise and a resync `0x00`, and must come through exactly as sent. |
| `protocol_radio` | `tower-protocol`: `decode_node_msg` / `decode_node_cmd`, the encoders, the management records | The input as a `NodeMsg` envelope, a `NodeCmd` envelope and a record stream of every `mgmt` record type (decoded like the CLI's `parse_records`): documented errors only (`TooShort`, `BadVersion { got }`, `Malformed`), every accepted value round-trips, an MTU-sized envelope always re-encodes within the MTU, every record consumes input (a zero-length record would spin the CLI's loop). The same bytes also build `NodeMsg` / `NodeCmd` values: the encoders never emit an over-MTU envelope and what they emit decodes back. |
| `kv_store` | `tower-kv` over an in-memory 6 KiB EEPROM (and a 512 B one, for many flips per input) | Starting from a blank part, a raw image, a legacy single-region log or a corrupted store, then `set` / `delete` / `compact` (plain and `_with`), `maintain` slices, `wipe`, reads, reboots, byte corruption and **power cuts** (the Nth write lands only a prefix, optionally a torn garbage byte, and every later write fails until the reboot). An independent reader of the documented on-EEPROM format checks every transition: a completed op has exactly its documented effect on every key, a refused one none, a cut one leaves the state before or after it (a wipe is all or nothing: no torn mix, no resurrected or phantom key), `init` and reboots preserve the content, the codec's own reads agree with the image, it never addresses past `capacity`, deleting an absent key writes nothing, a `maintain` slice keeps its word budget. From a blank part, `Full` / `TooManyKeys` must be justified by the live set and the generation only grows. |
| `shell_kernels` | `tower-shell`: `parse_addr`, `xorshift32_nonzero`, `Window`, `common_prefix`, `CompletionBudget` | `parse_addr` on arbitrary UTF-8 and lossily decoded bytes matches an independent implementation of its grammar, accepted literals re-parse from their canonical forms; the PRNG never yields 0. The shell's re-run pagination loop over a stable stream reassembles it exactly, every pass captures exactly its documented byte range within `cap`; over a stream that drifts between passes it never splits a char and terminates. `common_prefix` is the longest char-boundary common prefix. The completer's budget really fits: the actual postcard encoding of the admitted `ShellCompletions` stays within `used()`, and at `MAX_PAYLOAD` it fits one console frame. |
| `radio_kernels` | `tower-radio`: `DutyGovernor`, `frame_toa_ms`, `hop_channel`, `ccm::Ccm` | The duty governor under any refill / consume sequence matches a wide-integer model of its arithmetic and never spends more than its cap plus the duty share of the time credited (EU duty / FCC §15.247 dwell). `frame_toa_ms` is the exact ceiling for any length (saturating, never wrapping low) and monotonic. `hop_channel` is a perfect permutation for any seed, identical in every cycle (§15.247 spacing), for N = 80 and edge sizes. CCM agrees byte for byte with RustCrypto's `ccm` (ciphertext and tag), opens exactly what the reference opens (every single-bit tamper is refused), and the key-check value is `AES_K(0)`. |

`src/lib.rs` holds the protocol oracles shared by the two protocol targets.

### Known issue excluded from `kv_store`

The one-time migration of a **legacy** (pre-double-buffer) log longer than one half
(`tower-kv`'s `legacy_compact`) packs the log in place with no commit point, so a power cut in the
middle of it loses the live records not yet copied. It is reported, not fixed (a power-safe
in-place pack needs a different migration design), and `kv_store`'s boot check skips exactly that
case (a cut while migrating such a log). Remove the exception in `Harness::boot` when it is fixed.

## Adding a seed

Seeds live in `seeds/<target>/`, one input per file; libFuzzer only reads them. Keep them few and
small, each reaching something the others do not.

- `protocol_frames`: a leading mode byte, then the stream: an even byte followed by raw wire bytes
  (whole COBS frames ending in `0x00`, e.g. from a capture of a real console), or an odd byte
  followed by `[type index][seq lo][seq hi][noise len][payload len][payload]` chunks.
- `protocol_radio`: the raw bytes of a radio envelope (`RADIO_SCHEMA_VERSION` then the postcard
  body) or of a management record stream.
- `kv_store`, `shell_kernels`, `radio_kernels`: structured inputs decoded by `arbitrary`, so they
  are not written by hand. Run the target, pick a file from `corpus/<target>/` that reaches the
  behaviour you want (`cargo +nightly fuzz fmt <target> <file>` prints it as a Rust value), and
  copy it into `seeds/<target>/`.

The current protocol seeds are real frames / envelopes of every message and record type, built
with the crate's own encoders.

## From a crash to a regression test

1. Reproduce: `cargo +nightly fuzz run <target> fuzz/artifacts/<target>/crash-<hash>`.
2. Minimize: `cargo +nightly fuzz tmin <target> fuzz/artifacts/<target>/crash-<hash>` (writes
   `minimized-from-<hash>` next to it).
3. Read it: `cargo +nightly fuzz fmt <target> <minimized file>` prints the decoded input (the op
   sequence, the image, the value) for the structured targets.
4. Write a **unit test in the owning crate** that replays the minimal scenario with plain code,
   no fuzzer (the crate's own test style; see `crates/kv`'s
   `corrupt_mid_log_record_never_resurrects_the_tail` or `crates/radio`'s
   `frame_toa_total_for_any_length`). Watch it fail, fix the crate, watch it pass:
   `cargo test -p <crate>` at the repo root is the gate CI runs.
5. Re-run the artifact against the fixed tree, then fuzz the target again for a while. Optionally
   copy the minimized input into `seeds/<target>/` so every future run replays it first.
