# tools/hwtest: hardware test harness

Scripts that drive the reusable hardware test suite in [`docs/test-plan.md`](../../docs/test-plan.md)
on two TOWER Core Modules over their FTDI serial ports. See the test plan for the full matrix,
pass signals, and edge-case catalogue; this README is just the runner mechanics.

## Why these exist
- One-shot KAT examples print their verdict ~ms after boot then idle, so a capture must reset
  *then* read: mode B runs `tower logs --reset` (it pulses NRST on the handle it streams from, so
  the decoded capture starts at the boot banner) under `cap.py`, which bounds the run.
- Flashing two FTDIs concurrently is unreliable (Write-Memory timeouts, port re-enumeration), so
  `tb.sh` flashes **sequentially**, then captures both ports **concurrently**.

## Scripts
| script | purpose |
|---|---|
| `cap.py SECS CMD...` | run CMD for SECS seconds, capture stdout, kill the process group (no `timeout` on macOS) |
| `build.sh <example> <outbin> [features]` | `cargo objcopy` an example to a raw `.bin` (run from `firmware/`) |
| `fc.sh PORT BIN SECS OUT` | flash BIN to PORT, then mode-B capture (`tower logs --reset`) for SECS |
| `tb.sh NODE_BIN NP GW_BIN GP SECS NOUT GOUT` | two-board: flash node then gw (sequential), capture both |

## Quick recipes
```sh
# resolve current ports (they re-enumerate; re-run each session)
P1=$(ls /dev/cu.usbserial-* | sort | head -1); P2=$(ls /dev/cu.usbserial-* | sort | tail -1)

# host unit tests (the shared crates the firmware builds on; `just test` at the repo root runs
# the whole host workspace)
just test

# a one-shot KAT (mode B): flash + capture-from-reset + grep the verdict
tools/hwtest/build.sh crypto_ccm_kat /tmp/ccm.bin
tower -d "$P1" flash /tmp/ccm.bin
python3 tools/hwtest/cap.py 5 tower -d "$P1" logs --reset --color never | grep -iE 'PASS|FAIL'

# a continuous example (mode A): flash, then decoded logs
tower -d "$P1" flash /tmp/x.bin
python3 tools/hwtest/cap.py 6 tower -d "$P1" logs --color never

# two-board: one board role-node, the other default (role-gateway is a no-op == default)
tools/hwtest/build.sh net_secure_ping /tmp/nsp_n.bin role-node
tools/hwtest/build.sh net_secure_ping /tmp/nsp_d.bin
tools/hwtest/tb.sh /tmp/nsp_n.bin "$P1" /tmp/nsp_d.bin "$P2" 9 /tmp/n.txt /tmp/g.txt

# interactive shell, headless
tower -d "$P1" exec "system settings print"
tower -d "$P1" complete "system settings set m"
```

## Notes
- `RESULTS-2026-06-29.md` is a recorded full run (≈33 PASS + 4 findings), captured with the
  tooling of that day (mode B was `jolt monitor --reset` + `strings`).
- The inline shell is **zsh**: no `mapfile`; `${v:+--flag "$v"}` collapses to one arg (use explicit `if`).
