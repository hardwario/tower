# tower-protocol — working notes for Claude

The shared wire-format crate. It is consumed **by git tag** (not a path, not crates.io) by two
repos that MUST stay in lockstep with it:

- **`tower-firmware`** (github.com/hardwario/tower-firmware) — the `tower` lib, the
  `tower-bootloader` crate (`features = ["verify"]`), and `tools/fota-sign` (`["verify"]`).
- **`tower-cli`** (github.com/hardwario/tower-cli) — the host CLI.

**Cardinal rule:** postcard is *not* self-describing — a producer and a consumer built against
different versions silently mis-decode. Both ends must always build the **same tag**. This already
bit once (a consumer pinned a tag that didn't exist yet); don't let it drift.

## Releasing a change — do this whenever you change anything that ships

1. **If the wire format changed** — any postcard struct/enum (field *or* variant **order** counts),
   the framing, a `MsgType` discriminant, or the FOTA `Manifest` byte layout — bump
   `PROTOCOL_VERSION` in `src/lib.rs`.
2. **Bump `version` in `Cargo.toml`** (semver: wire/behaviour change → minor; fix → patch).
3. **Commit, push `main`, tag, push the tag:**
   ```sh
   git push origin main
   git tag -a vX.Y.Z -m "tower-protocol X.Y.Z" && git push origin vX.Y.Z
   ```
   (CI auto-creates the tag from the `Cargo.toml` version as a backstop if you forget — but tag it
   yourself so the consumers can be bumped right away. Pushes need **SSH**: `git@github.com:…`.)
4. **Propagate to BOTH consumers in the same change-set** — never bump one without the other:
   - **tower-firmware:** set `tag = "vX.Y.Z"` in `Cargo.toml`, `crates/bootloader/Cargo.toml`, and
     `tools/fota-sign/Cargo.toml`; then
     `cargo update -p tower-protocol` **and**
     `cargo update --manifest-path tools/fota-sign/Cargo.toml -p tower-protocol`;
     `just test` + build a FOTA example; commit + push.
   - **tower-cli:** set `tag = "vX.Y.Z"` in `Cargo.toml`; `cargo update -p tower-protocol`; build;
     commit + push. ⚠️ tower-cli has a **gitignored `.cargo/config.toml` `paths` override** that
     shadows the git source — move it aside before `cargo update` (else the lock won't re-resolve
     to the new tag), then restore it.

## Tests / checks

`cargo test --features verify` and `cargo clippy --all-targets --features verify -- -D warnings`
(both in CI, plus a `thumbv6m-none-eabi` no_std build). The crate is **hand-formatted** — there is
no `rustfmt` gate; do **not** bulk-reformat.

## Local co-development

cargo fetches this public repo over HTTPS fine; **pushes need SSH**. To have a consumer pick up
local edits without re-tagging, use a `paths` override (`paths = ["/abs/path/to/tower-protocol"]`):
tower-cli keeps it in its gitignored `.cargo/config.toml`; tower-firmware can't (its
`.cargo/config.toml` is committed for the build target), so put it in your `~/.cargo/config.toml`.
