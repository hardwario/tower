#!/usr/bin/env python3
"""ppk2d — Nordic PPK2 current-measurement sidecar for the HIL harness.

Protocol: **line-JSON over stdio**. Read one JSON command per line on stdin, write one JSON reply
per line on stdout. The Rust harness (`src/lib.rs`, `Ppk2`) drives this.

Commands
--------
    {"cmd":"on","mv":<mV>}      enable source-measure at <mV>          -> {"ok":true}
    {"cmd":"off"}               disable the supply                    -> {"ok":true}
    {"cmd":"cycle","mv":<mV>}   power-cycle (off, settle, on) at <mV>  -> {"ok":true}
    {"cmd":"avg","ms":<ms>}     average current over <ms>              -> {"ua":<microamps>}

Why a sidecar (not a Rust PPK2 driver): the PPK2 host protocol + calibration live in Nordic's
Python tooling (`ppk2-api`); wrapping it in a line-JSON process keeps that dependency out of the
Rust harness and lets the confounder handling live in one documented place.

The three known confounders this sidecar is responsible for (see the low-power measurement notes):

 1. **Debug-domain residual (+~200 µA).** A debug probe (J-Link/ST-Link) attached over SWD leaves
    the STM32 debug power domain energised, adding ~200 µA that never goes away until a clean
    power cycle. So `avg` ALWAYS power-cycles first (or requires a prior `cycle`) — never measure
    without cycling.
 2. **~2 V regulator/brown-out knee.** Measured at 3 V the deep-sleep current can read
    artificially low (below the knee the part behaves differently); the honest STOP floor is taken
    at **1.8 V**. This sidecar refuses `on`/`cycle`/`avg` above a sane supply ceiling and the
    harness fixture pins `supply_mv = 1800`.
 3. **PPK2 CDC desync during SWD traffic.** While probe-rs is hammering SWD, the PPK2's USB CDC can
    desync and report bursts of tens of mA of garbage. So this sidecar NEVER samples mid-flash and
    REJECTS any averaged reading above 50 mA as noise (the Rust side enforces the same ceiling).

Backend selection (transparent): if a real PPK2 is reachable and `ppk2-api` is importable, this
drives the hardware; otherwise it falls back to a modelled **stub** so the harness still compiles
and its plumbing unit-tests (the hardware HIL tests are `#[ignore]`d, so CI always takes the stub).
The confounder *policy* above is enforced identically on both paths — it lives in `Ppk2Base`, and
each backend only supplies the three raw hardware hooks.

To use real hardware: `python3 -m venv hil/.venv && hil/.venv/bin/pip install ppk2-api pyserial`.
The harness spawns plain `python3 ppk2d.py`; if `ppk2_api` isn't in that interpreter, this script
re-execs into a sibling `./.venv` automatically (see `_ensure_deps`). Override the device port with
`TOWER_PPK2_PORT` if auto-detection picks the wrong Nordic CDC.
"""

import json
import os
import sys
import time

# Sanity ceilings (mirror the Rust `PPK2_SANE_MAX_UA`).
SANE_MAX_UA = 50_000.0  # >50 mA averaged => CDC desync (confounder #3), never a valid STOP floor.
MAX_SUPPLY_MV = 3600    # never source above the board's abs-max; the bench uses 1800 (knee, #2).

# Settle time after toggling the supply, so the debug-domain residual (confounder #1) fully
# discharges before a measurement is taken.
CYCLE_SETTLE_S = 0.30

# Nordic PPK2 USB identity (VID 0x1915 / PID 0xC00A) — the fallback port detector matches on it.
PPK2_VID = 0x1915
PPK2_PID = 0xC00A


class Ppk2Base:
    """The confounder policy, backend-independent. Subclasses supply only the three hardware hooks
    (`_hw_on` / `_hw_off` / `_hw_sample_mean_ua`); `set_on`/`set_off`/`cycle`/`avg_ua` enforce the
    supply ceiling, the cycle-before-measure rule, and the CDC-desync rejection identically for the
    real PPK2 and the stub."""

    def __init__(self):
        self.on = False
        self.mv = 0
        self.cycled_since_measure = False

    # --- hardware hooks (overridden per backend) ---
    def _hw_on(self, mv):
        raise NotImplementedError

    def _hw_off(self):
        raise NotImplementedError

    def _hw_sample_mean_ua(self, ms):
        raise NotImplementedError

    # --- policy (shared) ---
    def set_on(self, mv):
        if mv <= 0 or mv > MAX_SUPPLY_MV:
            raise ValueError(f"supply {mv} mV out of range (0, {MAX_SUPPLY_MV}] — see confounder #2")
        self._hw_on(mv)
        self.on = True
        self.mv = mv

    def set_off(self):
        self._hw_off()
        self.on = False

    def cycle(self, mv):
        # Confounder #1: a clean power cycle clears the debug-domain residual current. Always the
        # preamble to a measurement.
        self.set_off()
        time.sleep(CYCLE_SETTLE_S)
        self.set_on(mv)
        self.cycled_since_measure = True

    def avg_ua(self, ms):
        if not self.on:
            raise RuntimeError("avg requested with supply off — `on`/`cycle` first")
        if not self.cycled_since_measure:
            # Enforce confounder #1: refuse a measurement that wasn't preceded by a power cycle.
            raise RuntimeError("avg requested without a prior power-cycle (confounder #1: debug "
                               "domain adds ~200 µA until cycled) — call `cycle` first")
        ua = self._hw_sample_mean_ua(ms)
        # Guard mirrors the Rust ceiling: a real desynced read exceeds this and must be rejected as
        # noise rather than reported (confounder #3).
        if ua > SANE_MAX_UA:
            raise RuntimeError(f"averaged {ua} µA > {SANE_MAX_UA} µA — CDC desync mid-flash (#3)")
        # A fresh measurement "consumes" the cycle: the next avg must cycle again.
        self.cycled_since_measure = False
        return ua


class Ppk2Stub(Ppk2Base):
    """No hardware: returns a believable Core-Module STOP floor at 1.8 V with the FTDI unplugged.
    Keeps the harness plumbing testable and CI hardware-free."""

    def _hw_on(self, mv):
        pass

    def _hw_off(self):
        pass

    def _hw_sample_mean_ua(self, ms):
        return 12.0


class Ppk2Real(Ppk2Base):
    """Drives a physical PPK2 via `ppk2-api` in source-measure mode (the PPK2 both supplies the DUT
    and measures its current)."""

    def __init__(self, port):
        from ppk2_api.ppk2_api import PPK2_API

        self.ppk2 = PPK2_API(port)
        self._measuring = False
        # A PPK2 exposes TWO CDC endpoints and only one speaks the control protocol — so a
        # successful open is NOT enough. `get_modifiers` reads the per-unit calibration; if it
        # comes back falsy this is the wrong endpoint (or a prior session left a measurement
        # stream that corrupts the read), so raise and let `make_device` try the next candidate.
        # Without this check the sidecar silently ran on the dead endpoint and every reading was
        # uncalibrated garbage (~45 A → rejected as CDC desync). Clear any stale stream first, then
        # retry: the metadata reply lands a moment after the request.
        try:
            self.ppk2.stop_measuring()
        except Exception:
            pass
        # Drain any leftover measurement stream: a prior sidecar that died mid-measure (e.g. a
        # harness panic) leaves the device streaming, and those BINARY sample bytes make the
        # metadata read's utf-8 decode throw (`invalid start byte`). Read+discard until quiet.
        ser = getattr(self.ppk2, "ser", None)
        if ser is not None:
            deadline = time.monotonic() + 1.5
            while time.monotonic() < deadline:
                pending = getattr(ser, "in_waiting", 0)
                if pending:
                    ser.read(pending)
                    time.sleep(0.05)
                else:
                    time.sleep(0.05)
                    if not getattr(ser, "in_waiting", 0):
                        break
        loaded = False
        for _ in range(6):
            time.sleep(0.15)
            try:
                if self.ppk2.get_modifiers():
                    loaded = True
                    break
            except Exception:
                # residual binary in the read — flush what's buffered and retry
                if ser is not None and getattr(ser, "in_waiting", 0):
                    ser.read(ser.in_waiting)
        if not loaded:
            raise RuntimeError(f"{port}: PPK2 calibration read failed (not the control CDC endpoint?)")
        self.ppk2.use_source_meter()   # source-measure: PPK2 powers the DUT and meters it
        super().__init__()

    def _hw_on(self, mv):
        self.ppk2.set_source_voltage(int(mv))
        self.ppk2.toggle_DUT_power("ON")
        self.ppk2.start_measuring()
        self._measuring = True

    def _hw_off(self):
        try:
            if self._measuring:
                self.ppk2.stop_measuring()
                self._measuring = False
        finally:
            self.ppk2.toggle_DUT_power("OFF")

    def _hw_sample_mean_ua(self, ms):
        # Drop the first buffered chunk (a partial/misaligned read the parser can turn into a
        # spike), then drain get_data() for `ms`. PPK2 free-runs at ~100 kHz, so even a short window
        # is thousands of samples; a short poll sleep keeps the USB reads efficient.
        #
        # Return the **median** of the physically-plausible samples, not the raw mean:
        #  - get_samples occasionally emits non-physical spikes (misaligned parse / CDC desync —
        #    seen up to ~10 A), which a raw mean would let dominate. Filter them out (0..sane max).
        #  - the STOP *floor* is the quiescent current; a plain mean is skewed upward by the brief
        #    periodic wake spikes (the ~500 ms console VBUS poll), so the median reports the floor.
        self.ppk2.get_data()
        raw = []
        deadline = time.monotonic() + ms / 1000.0
        while time.monotonic() < deadline:
            data = self.ppk2.get_data()
            if data:
                samples, _ = self.ppk2.get_samples(data)
                raw.extend(samples)
            time.sleep(0.005)
        clean = [x for x in raw if 0.0 <= x <= SANE_MAX_UA]
        if not clean:
            raise RuntimeError(f"no plausible PPK2 samples over the window ({len(raw)} raw, all rejected)")
        clean.sort()
        return clean[len(clean) // 2]  # median — robust to spikes + periodic wakes


def _ppk2_candidates():
    """Ordered candidate serial devices for the PPK2. A PPK2 exposes **two** CDC endpoints (both
    carry its Nordic VID) but only one speaks the measurement protocol, so this returns every match
    and the caller tries each until one initialises. Order: an explicit `TOWER_PPK2_PORT` first,
    then a pyserial scan filtered to the Nordic VID/PID (precise — never returns a J-Link), then
    ppk2-api's own (unfiltered) enumerator as a last resort. De-duplicated, order preserved."""
    out = []
    env = os.environ.get("TOWER_PPK2_PORT")
    if env:
        out.append(env)
    try:
        import serial.tools.list_ports as list_ports
        for p in list_ports.comports():
            if (p.vid, p.pid) == (PPK2_VID, PPK2_PID):
                out.append(p.device)
    except Exception:
        pass
    try:
        from ppk2_api.ppk2_api import PPK2_API
        for f in PPK2_API.list_devices() or []:
            out.append(f[0] if isinstance(f, (list, tuple)) else f)
    except Exception:
        pass
    seen = set()
    return [p for p in out if not (p in seen or seen.add(p))]


def _ensure_deps():
    """If `ppk2_api` isn't importable in this interpreter but a sibling `./.venv` has it, re-exec
    into that venv's python. This lets the Rust harness keep spawning plain `python3 ppk2d.py`
    while the real-hardware deps live in an uncommitted, gitignored venv. No-op (falls through to
    the stub) when neither is present.

    A `_PPK2D_REEXEC` sentinel — not a path comparison — guards against an exec loop: a venv's
    `bin/python3` symlinks to the base interpreter, so `realpath(sys.executable)` equals the venv
    python's realpath and can't distinguish "already in the venv"."""
    try:
        import ppk2_api  # noqa: F401
        return
    except ImportError:
        pass
    if os.environ.get("_PPK2D_REEXEC"):
        return  # already re-exec'd once — the venv still lacks the dep; make_device() → stub
    here = os.path.dirname(os.path.abspath(__file__))
    venv_py = os.path.join(here, ".venv", "bin", "python3")
    if os.path.exists(venv_py):
        os.environ["_PPK2D_REEXEC"] = "1"
        os.execv(venv_py, [venv_py, os.path.abspath(__file__), *sys.argv[1:]])


def make_device():
    """Pick the backend: a real PPK2 if one is reachable and `ppk2-api` loads, else the stub. Tries
    each candidate CDC endpoint (a PPK2 presents two) until one initialises. The choice is logged to
    stderr so a power run makes clear whether it measured real silicon."""
    candidates = _ppk2_candidates()
    for port in candidates:
        try:
            dev = Ppk2Real(port)
            print(f"ppk2d: real PPK2 on {port}", file=sys.stderr, flush=True)
            return dev
        except Exception as e:
            print(f"ppk2d: {port} not a live PPK2 ({e})", file=sys.stderr, flush=True)
    print(f"ppk2d: no PPK2 initialised ({len(candidates)} candidate(s)); using stub",
          file=sys.stderr, flush=True)
    return Ppk2Stub()


def main():
    _ensure_deps()
    dev = make_device()
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
            cmd = req.get("cmd")
            if cmd == "on":
                dev.set_on(int(req["mv"]))
                reply = {"ok": True}
            elif cmd == "off":
                dev.set_off()
                reply = {"ok": True}
            elif cmd == "cycle":
                dev.cycle(int(req["mv"]))
                reply = {"ok": True}
            elif cmd == "avg":
                reply = {"ua": dev.avg_ua(int(req.get("ms", 100)))}
            else:
                reply = {"error": f"unknown cmd {cmd!r}"}
        except Exception as e:  # surface any error as a JSON line the harness can see
            reply = {"error": str(e)}
        sys.stdout.write(json.dumps(reply) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
