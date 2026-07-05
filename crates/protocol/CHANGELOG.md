# Changelog — tower-protocol

All notable changes to the wire format and the crate API. The wire format is postcard
(not self-describing): every entry that says **wire change** implies a `PROTOCOL_VERSION`
bump and a coordinated re-pin of all three consumers (`tower-firmware`, `tower-cli`,
`tower-hil`).

## v1.1.0 — 2026-07-04

**Wire change** — `PROTOCOL_VERSION` 1 → 2.

- `Hello` extended: `{ protocol_version, firmware_name, firmware_version, session_id: u32 }` —
  the host banner can now show what firmware it is talking to and detect device reboots
  (`session_id` changes).
- Added `decode_msg`: typed one-call decode of an inner frame into the `Msg` enum.
- Added `Error::Malformed` (CRC-valid frame whose body fails to deserialize).
- Golden vectors regenerated for the new schema.

## v1.0.0 — 2026-07-02

Initial (re-baselined) release.

- COBS framing with `0x00` delimiters + CRC-32 over the inner frame.
- Postcard message schema: `Hello`, `Log`, `Print`, `Event`, `Dropped`, `ShellCommand`,
  `ShellResponse`, `ShellComplete`, `ShellCompletions`.
- Codec hardening: over-long frames rejected on both encode and decode, actionable
  version-mismatch error, golden wire-byte vectors + CRC check-vector tests.
- Note: the FOTA subsystem was removed and the crate re-baselined at this version; the
  previous `v1.0.0`/`v1.1.0` tags from the FOTA era were deleted and `v1.0.0` re-cut
  (2026-07-02). Anything resolving the old SHAs must re-pin.
