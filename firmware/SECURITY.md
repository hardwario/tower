# Security policy

This policy covers **tower-firmware** (the TOWER Core Module firmware SDK and product apps),
part of the HARDWARIO TOWER ecosystem.

## Reporting a vulnerability

Please **do not** report security issues through public issues, discussions, or pull requests.

Report privately with GitHub's private vulnerability reporting: open this repository's
**Security** tab and choose **Report a vulnerability**
(<https://github.com/hardwario/tower-firmware/security/advisories/new>). The report stays
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

Of particular interest: the SPIRIT1 radio network layer (AES-128-CCM, nonce/TX-counter handling,
replay protection, ACK handling, OTA pairing, FHSS beacon epochs), the framed console and shell
(input parsing, settings storage), and EEPROM key handling.

## Supported versions

Security fixes are made on the current `main` (there are no tagged releases yet). Older versions
are not patched; update to the latest.
