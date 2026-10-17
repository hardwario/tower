# Security policy

This policy covers the **tower** repository: the whole HARDWARIO TOWER software stack (the
wire protocol, the Core Module firmware SDK and product apps, the `tower` host CLI and gateway,
the bootloader engine and the HIL bench harness).

## Reporting a vulnerability

Please **do not** report security issues through public issues, discussions, or pull requests.

Report privately with GitHub's private vulnerability reporting: open this repository's
**Security** tab and choose **Report a vulnerability**
(<https://github.com/hardwario/tower/security/advisories/new>). The report stays visible only to
you and the maintainers until an advisory is published.

Helpful details:

- the affected version (a `cli-vX.Y.Z` release or a commit SHA) and
  component (directory);
- what the issue is and its impact as you understand it;
- steps to reproduce or a proof of concept (hardware setup, if any).

We will acknowledge the report, keep you updated while we investigate and fix it, and credit you
in the published advisory unless you prefer to stay anonymous. Please give us a reasonable
window to ship a fix before disclosing publicly.

## Scope

Of particular interest, by component:

- **`firmware/`** (and the hardware-free kernels in `crates/net`, `crates/radio`,
  `crates/gateway`, `crates/kv`, `crates/shell`): the SPIRIT1 radio network layer (AES-128-CCM,
  nonce / TX-counter handling, replay protection, ACK handling, OTA pairing, FHSS beacon
  epochs), the framed console and shell (input parsing, settings storage), and EEPROM key
  handling.
- **`crates/protocol`**: decoding of untrusted bytes (panics, out-of-bounds reads, unbounded
  allocation), framing / CRC bypasses, and schema ambiguities that could make firmware and host
  disagree about a frame.
- **`cli/`**: the gateway / MQTT surface (embedded broker, topic handling, node management
  commands), decoding of frames from an untrusted device, and the flashing path. Release
  archives carry signed build-provenance attestations
  (`gh attestation verify <archive> --repo hardwario/tower`); a missing or failing attestation on
  an official archive is itself worth reporting.
- **`crates/flasher`**: handling of malformed bootloader responses or firmware images, and anything
  that could write outside the intended flash range (main flash only). Readout protection is
  opt-in: level 1 is set or cleared only through the explicit `tower protect` / `tower unprotect`
  commands; the tooling never writes option bytes directly and has no path to the permanent
  level 2.
- **`hil/`**: test tooling that runs on a developer's bench, not shipped product code; issues
  that could compromise the host running it (e.g. command injection via `hil.toml` or
  device-supplied data) are still in scope.
- The repository's own tooling: the CI workflows, the `.claude/` skills.

## Supported versions

Security fixes are made on `main` and ship in the next `cli-vX.Y.Z` release (for the CLI) or as
a firmware update built from `main`. Older versions are not patched; update to the latest.
