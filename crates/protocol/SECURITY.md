# Security policy

This policy covers **tower-protocol**, the shared TOWER wire-format crate (COBS + CRC-32
framing, postcard message and radio schemas), part of the HARDWARIO TOWER ecosystem.

## Reporting a vulnerability

Please **do not** report security issues through public issues, discussions, or pull requests.

Report privately with GitHub's private vulnerability reporting: open this repository's
**Security** tab and choose **Report a vulnerability**
(<https://github.com/hardwario/tower-protocol/security/advisories/new>). The report stays
visible only to you and the maintainers until an advisory is published.

Helpful details:

- the affected version (release tag or commit SHA);
- what the issue is and its impact as you understand it;
- steps to reproduce or a proof of concept (hardware setup, if any).

We will acknowledge the report, keep you updated while we investigate and fix it, and credit you
in the published advisory unless you prefer to stay anonymous. Please give us a reasonable
window to ship a fix before disclosing publicly.

If an issue spans several TOWER repositories (for example a wire-format flaw that affects both
the firmware and the CLI), report it in the repository where it originates (usually
[`tower-protocol`](https://github.com/hardwario/tower-protocol)) and mention the others; we
coordinate the fix across all of them.

## Scope

Of particular interest: decoding of untrusted bytes (panics, out-of-bounds reads, unbounded
allocation), framing/CRC bypasses, and schema ambiguities that could make firmware and host
disagree about a frame. A fix ships as a new tag, and every consumer (tower-firmware, tower-cli,
tower-hil) is bumped to it in one coordinated change-set.

## Supported versions

Security fixes are made on the latest release tag (`vX.Y.Z`) and `main`. Older versions are not
patched; update to the latest.
