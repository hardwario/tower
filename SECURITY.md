# Security policy

This policy covers **tower**, the TOWER control plane (submodule pins, lockstep tooling, agent
workflows), part of the HARDWARIO TOWER ecosystem.

## Reporting a vulnerability

Please **do not** report security issues through public issues, discussions, or pull requests.

Report privately with GitHub's private vulnerability reporting: open this repository's
**Security** tab and choose **Report a vulnerability**
(<https://github.com/hardwario/tower/security/advisories/new>). The report stays visible only to
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

This repository holds no product code: a vulnerability in firmware, the CLI, the wire protocol,
the flasher, or the HIL harness belongs in that component's repository (see the table in the
README). Report here only issues in this repo's own tooling (e.g. `tools/check_lockstep.py`, the
CI workflow, the `.claude/` skills) or if you are unsure where a cross-repo issue belongs.

## Supported versions

Security fixes are made on the latest commit on `main` (the current pinned snapshot). Older
versions are not patched; update to the latest.
