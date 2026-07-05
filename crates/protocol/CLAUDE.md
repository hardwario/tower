# tower-protocol — working notes for Claude

The shared wire-format crate. It is consumed **by git tag** (not a path, not crates.io) by
**three** repos that MUST stay in lockstep with it:

- **`tower-firmware`** (github.com/hardwario/tower-firmware) — pins the tag in **two**
  manifests: the `tower` lib (`Cargo.toml`) and the `tower-kv` crate
  (`crates/tower-kv/Cargo.toml`, which shares the CRC primitive).
- **`tower-cli`** (github.com/hardwario/tower-cli) — the host CLI (`Cargo.toml`).
- **`tower-hil`** (github.com/hardwario/tower-hil) — the HIL bench harness (`Cargo.toml`);
  decodes the framed console natively. (It lived at `tower-firmware/tools/hil` until
  2026-07-05; it is its own repo now.)

**Cardinal rule:** postcard is *not* self-describing — a producer and a consumer built against
different versions silently mis-decode. All ends must always build the **same tag**. This already
bit once (a consumer pinned a tag that didn't exist yet); don't let it drift.

## Releasing a change — do this whenever you change anything that ships

1. **If the wire format changed** — any postcard struct/enum (field *or* variant **order**
   counts), the framing, or a `MsgType` discriminant — bump `PROTOCOL_VERSION` in `src/lib.rs`
   and regenerate the golden vectors (`tests/golden.rs`). CI enforces the pairing both ways:
   the `test` job fails if the goldens no longer match the code, and the `wire-bump-guard` job
   fails if the golden bytes changed relative to the latest tag while `PROTOCOL_VERSION` did not
   (`tools/check_wire_bump.py`).
2. **Bump `version` in `Cargo.toml`** (semver: wire/behaviour change → minor; fix → patch), and
   add a `CHANGELOG.md` entry.
3. **Commit, push `main`, tag, push the tag:**
   ```sh
   git push origin main
   git tag -a vX.Y.Z -m "tower-protocol X.Y.Z" && git push origin vX.Y.Z
   ```
   (CI auto-creates the tag from the `Cargo.toml` version as a backstop if you forget — but tag it
   yourself so the consumers can be bumped right away. Pushes need **SSH**: `git@github.com:…`.)
4. **Propagate to ALL THREE consumers in the same change-set** — never bump one without the
   others:
   - **tower-firmware:** set `tag = "vX.Y.Z"` in `Cargo.toml` **and**
     `crates/tower-kv/Cargo.toml`; then `cargo update -p tower-protocol` (covers the workspace);
     `just test` + build an example; commit + push.
   - **tower-cli:** set `tag = "vX.Y.Z"` in `Cargo.toml`; `cargo update -p tower-protocol`; build;
     commit + push. ⚠️ tower-cli may have a **gitignored `.cargo/config.toml` `paths` override**
     that shadows the git source — move it aside before `cargo update` (else the lock won't
     re-resolve to the new tag), then restore it.
   - **tower-hil:** set `tag = "vX.Y.Z"` in `Cargo.toml`; `cargo update -p tower-protocol`;
     `cargo test --no-run` (compile-check — no bench hardware needed); commit + push.
5. In the control-plane repo (`hardwario/tower`), run `/lockstep` to verify the alignment, then
   `/pin` to freeze the new SHAs.

## Tests / checks

`cargo test` and `cargo clippy --all-targets -- -D warnings`
(both in CI, plus a `thumbv6m-none-eabi` no_std build and the wire-bump guard). The crate is
**hand-formatted** — there is no `rustfmt` gate; do **not** bulk-reformat.

## Local co-development

cargo fetches this public repo over HTTPS fine; **pushes need SSH**. To have a consumer pick up
local edits without re-tagging, use a `paths` override (`paths = ["/abs/path/to/tower-protocol"]`):
tower-cli keeps it in its gitignored `.cargo/config.toml`; tower-firmware can't (its
`.cargo/config.toml` is committed for the build target), so put it in your `~/.cargo/config.toml`
— or, when working from the `hardwario/tower` control plane, in the root's git-ignored
`.cargo/config.toml`, which covers all the checkouts at once. Remove the override before pinning.
