# Security policy

This policy covers **jolt**, the STM32L0 UART-bootloader flasher (library + `jolt` binary),
part of the HARDWARIO TOWER ecosystem.

## Reporting a vulnerability

Please **do not** report security issues through public issues, discussions, or pull requests.

Report privately with GitHub's private vulnerability reporting: open this repository's
**Security** tab and choose **Report a vulnerability**
(<https://github.com/hardwario/jolt/security/advisories/new>). The report stays visible only to
you and the maintainers until an advisory is published.

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

Of particular interest: handling of malformed bootloader responses or firmware images, and
anything that could write outside the intended flash range. Release archives from the current
release workflow (newer than v1.4.0) carry signed build-provenance attestations (`gh attestation
verify <archive> --repo hardwario/jolt`).

## Supported versions

Security fixes are made on the latest release (`vX.Y.Z`) and `main`. Older versions are not
patched; update to the latest.
