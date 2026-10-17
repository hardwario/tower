#!/usr/bin/env python3
"""ppk2d: Nordic PPK2 current-measurement sidecar for the HIL harness.

Protocol: **line-JSON over stdio**. On startup write ONE line naming the backend, then read one
JSON command per line on stdin and write one JSON reply per line on stdout. Every line carries
`"ok"`: true, or false plus an `"error"` string. The Rust harness (`src/lib.rs`, `Ppk2`) drives
this and fails on anything but `"ok": true`.

    startup                     -> {"ok":true,"backend":"real"|"stub","port":<dev>|null}
                                   (or {"ok":false,"error":...} and exit 1; see "Backend")
    {"cmd":"on","mv":<mV>}      enable source-measure at <mV>          -> {"ok":true}
    {"cmd":"off"}               disable the supply                    -> {"ok":true}
    {"cmd":"cycle","mv":<mV>}   power-cycle (off, settle, on) at <mV>  -> {"ok":true}
    {"cmd":"avg","ms":<ms>}     sample the current for <ms>            ->
                                {"ok":true,"median_ua":<µA>,"mean_ua":<µA>,"samples":<n>}

stdout is the protocol channel and NOTHING else may write to it: `ppk2-api` prints diagnostics
("Measurement outside of range!") with plain `print`, which would desync the line protocol. So
`main` keeps a private duplicate of the real stdout for the replies and points fd 1 at stderr
before `ppk2-api` is imported.

Why a sidecar (not a Rust PPK2 driver): the PPK2 host protocol + calibration live in Nordic's
Python tooling (`ppk2-api`); wrapping it in a line-JSON process keeps that dependency out of the
Rust harness and lets the confounder handling live in one documented place.

The three known confounders this sidecar is responsible for (see the low-power measurement notes):

 1. **Debug-domain residual (+~200 µA).** A debug probe (J-Link/ST-Link) attached over SWD leaves
    the STM32 debug power domain energised, adding ~200 µA that never goes away until a clean
    power cycle. So `avg` ALWAYS power-cycles first (or requires a prior `cycle`); never measure
    without cycling.
 2. **Supply rail (pinned, not a knee artifact).** An early bench saw ~+200 µA at 3 V from a
    board/adapter consumer with an LED-like ~2 V knee; on the current bench the true DUT floor is
    honest at both rails: 5.1 µA @ 1.8 V, 6.1 µA @ 3.0 V (examples/lowpower.rs; the ~1 µA rise is
    ordinary CMOS leakage). The bench standardises on **1.8 V** so readings stay comparable and
    any such knee consumer stays dark. This sidecar refuses `on`/`cycle` outside (0, 3600] mV and
    the harness fixture pins `supply_mv = 1800`.
 3. **PPK2 CDC desync during SWD traffic.** While probe-rs is hammering SWD, the PPK2's USB CDC can
    desync and report bursts of tens of mA of garbage. So this sidecar NEVER samples mid-flash and
    REJECTS any averaged reading above 50 mA as noise (the Rust side enforces the same ceiling).

Backend: a real PPK2 (via `ppk2-api`) or nothing. If no PPK2 initialises (no device, the
`ppk2-api` import fails, a stale/broken `.venv` (e.g. its python symlink dangling after a Python
upgrade), a calibration-read flake), the sidecar reports WHY on the startup line and exits; it does
NOT silently substitute the modelled **stub**, whose believable ~12 µA would pass a power test that
measured nothing. The stub is used only when `TOWER_PPK2_STUB=1` explicitly opts in to a plumbing
run (and the startup line then says `"backend":"stub"`, which the Rust side also checks). CI never
starts this sidecar at all: the power test is `#[ignore]`d and feature-gated. The confounder
*policy* above is enforced identically on both backends: it lives in `Ppk2Base`, and each backend
only supplies the raw hardware hooks.

To use real hardware: `python3 -m venv hil/.venv && hil/.venv/bin/pip install ppk2-api pyserial`.
The harness spawns plain `python3 ppk2d.py`; if `ppk2_api` isn't in that interpreter, this script
re-execs into a sibling `./.venv` automatically (see `_ensure_deps`). Override the device port with
`TOWER_PPK2_PORT` if auto-detection picks the wrong Nordic CDC.
"""

import importlib.util
import json
import os
import sys
import time

# The protocol channel (a private dup of the real stdout), set up by `_claim_stdout` in `main`.
_proto = sys.stdout

# Why the real backend isn't available, collected for the startup error line.
_dep_notes = []

# Sanity ceilings (mirror the Rust `PPK2_SANE_MAX_UA`).
SANE_MAX_UA = 50_000.0  # >50 mA averaged => CDC desync (confounder #3), never a valid STOP floor.
MAX_SUPPLY_MV = 3600    # never source above the board's abs-max; the bench uses 1800 (knee, #2).

# Settle time after toggling the supply, so the debug-domain residual (confounder #1) fully
# discharges before a measurement is taken.
CYCLE_SETTLE_S = 0.30

# Nordic PPK2 USB identity (VID 0x1915 / PID 0xC00A): the fallback port detector matches on it.
PPK2_VID = 0x1915
PPK2_PID = 0xC00A


class Ppk2Base:
    """The confounder policy, backend-independent. Subclasses supply only the three hardware hooks
    (`_hw_on` / `_hw_off` / `_hw_samples_ua`); `set_on`/`set_off`/`cycle`/`measure` enforce the
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

    def _hw_samples_ua(self, ms):
        raise NotImplementedError

    # --- policy (shared) ---
    def set_on(self, mv):
        if mv <= 0 or mv > MAX_SUPPLY_MV:
            raise ValueError(f"supply {mv} mV out of range (0, {MAX_SUPPLY_MV}]; see confounder #2")
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

    def measure(self, ms):
        """Sample for `ms` and return (median_ua, mean_ua, n). BOTH statistics, because they answer
        different questions: the MEDIAN is the quiescent STOP floor (robust to the brief periodic
        wake spikes: the ~500 ms console VBUS poll), the MEAN is the true average draw (what a
        battery sees). Reporting only the median under the name "average" hid wake regressions."""
        if not self.on:
            raise RuntimeError("avg requested with supply off; `on`/`cycle` first")
        if not self.cycled_since_measure:
            # Enforce confounder #1: refuse a measurement that wasn't preceded by a power cycle.
            raise RuntimeError("avg requested without a prior power-cycle (confounder #1: debug "
                               "domain adds ~200 µA until cycled); call `cycle` first")
        raw = self._hw_samples_ua(ms)
        # Physically plausible samples only (0..sane max): a desynced parser yields tens-of-amps
        # and negative garbage, which must not pollute either statistic.
        clean = sorted(x for x in raw if 0.0 <= x <= SANE_MAX_UA)
        if not clean:
            raise RuntimeError(f"no plausible PPK2 samples over the window ({len(raw)} raw, all rejected)")
        median = clean[len(clean) // 2]
        mean = sum(clean) / len(clean)
        # Guard mirrors the Rust ceiling: a real desynced read exceeds this and must be rejected as
        # noise rather than reported (confounder #3).
        if max(median, mean) > SANE_MAX_UA:
            raise RuntimeError(f"averaged {max(median, mean)} µA > {SANE_MAX_UA} µA: CDC desync mid-flash (#3)")
        # A fresh measurement "consumes" the cycle: the next avg must cycle again.
        self.cycled_since_measure = False
        return median, mean, len(clean)


class Ppk2Stub(Ppk2Base):
    """No hardware: returns a believable Core-Module STOP floor at 1.8 V on the PPK2-powered bench.
    For deliberate plumbing runs ONLY (`TOWER_PPK2_STUB=1`), never an automatic fallback, because
    its fake ~12 µA is indistinguishable from a passing measurement."""

    def _hw_on(self, mv):
        pass

    def _hw_off(self):
        pass

    def _hw_samples_ua(self, ms):
        return [12.0] * max(1, int(ms))


class Ppk2Real(Ppk2Base):
    """Drives a physical PPK2 via `ppk2-api` in source-measure mode (the PPK2 both supplies the DUT
    and measures its current)."""

    def __init__(self, port):
        from ppk2_api.ppk2_api import PPK2_API

        self.ppk2 = PPK2_API(port)
        self._measuring = False
        # A PPK2 exposes TWO CDC endpoints and only one speaks the control protocol, so a
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
                # residual binary in the read: flush what's buffered and retry
                if ser is not None and getattr(ser, "in_waiting", 0):
                    ser.read(ser.in_waiting)
        if not loaded:
            # Release the port before giving up: a PPK2 presents two CDC endpoints on ONE physical
            # device, and leaving the dead endpoint's handle open disturbs the good endpoint so its
            # calibration read then also fails; auto-detect would then find no PPK2 at all (it
            # used to flake to the stub: a fake ~12 µA that masqueraded as a real STOP floor).
            # Close so `make_device`'s next candidate opens cleanly, and settle briefly for the
            # shared device to quiesce.
            if ser is not None:
                try:
                    ser.close()
                except Exception:
                    pass
            time.sleep(0.3)
            raise RuntimeError(f"{port}: PPK2 calibration read failed (not the control CDC endpoint?)")
        self.ppk2.use_source_meter()   # source-measure: PPK2 powers the DUT and meters it
        super().__init__()

    def _hw_on(self, mv):
        # Power the DUT but do NOT start the measurement stream here. The caller settles (lets the
        # firmware reach STOP) for seconds after `on`/`cycle`, and if the PPK2 were streaming during
        # that gap the un-drained OS serial buffer would overflow, permanently desyncing the 4-byte
        # sample parser into garbage. The stream is started fresh, and read continuously, only inside
        # `_hw_samples_ua`. Power vs. measurement-streaming are independent on the PPK2.
        self.ppk2.set_source_voltage(int(mv))
        self.ppk2.toggle_DUT_power("ON")

    def _hw_off(self):
        try:
            if self._measuring:
                self.ppk2.stop_measuring()
                self._measuring = False
        finally:
            self.ppk2.toggle_DUT_power("OFF")

    def _hw_samples_ua(self, ms):
        # Start a FRESH measurement stream and read it CONTINUOUSLY: this is the whole ballgame.
        # Two hard-won rules, both learned by watching a live ~20 µA board read as garbage:
        #  (1) NEVER let the stream run un-drained. If start_measuring() is left running across the
        #      caller's multi-second settle sleep, the OS serial buffer overflows and the 4-byte
        #      sample parser can never realign → garbage (tens of A + ~50 % zeros). So the stream is
        #      OFF during settle (see _hw_on) and started here, then drained tightly.
        #  (2) NEVER discard raw get_data() bytes mid-stream. get_samples keeps an internal
        #      byte-remainder to reassemble 4-byte samples across calls; dropping even one chunk
        #      desyncs it. So every get_data() goes through get_samples(); only the VALUES are
        #      dropped (during the short stabilize), never the bytes.
        # Pre-start flush is fine (those bytes precede the stream); mid-stream discard is not.
        #  (3) Reset the parser's byte-remainder per session too. ppk2-api keeps it across
        #      stop/start, but the pre-start flush above discards the tail of the PREVIOUS stream,
        #      so a stale remainder would glue old bytes onto the new stream's first sample and the
        #      second `avg` could start misaligned.
        # Returns the raw sample list; `Ppk2Base.measure` filters it and computes median + mean.
        ser = getattr(self.ppk2, "ser", None)
        if ser is not None and getattr(ser, "in_waiting", 0):
            ser.read(ser.in_waiting)          # clear residue BEFORE the stream starts (raw ok here)
        if hasattr(self.ppk2, "remainder"):
            self.ppk2.remainder = {"sequence": b"", "len": 0}
        self.ppk2.start_measuring()
        self._measuring = True
        try:
            stabilize = time.monotonic() + 0.2   # drop the first partial frame; keep alignment
            while time.monotonic() < stabilize:
                data = self.ppk2.get_data()
                if data:
                    self.ppk2.get_samples(data)
                time.sleep(0.002)
            raw = []
            deadline = time.monotonic() + ms / 1000.0
            while time.monotonic() < deadline:
                data = self.ppk2.get_data()
                if data:
                    samples, _ = self.ppk2.get_samples(data)
                    raw.extend(samples)
                time.sleep(0.002)
        finally:
            self.ppk2.stop_measuring()
            self._measuring = False
        return raw


def _ppk2_candidates():
    """Ordered candidate serial devices for the PPK2. A PPK2 exposes **two** CDC endpoints (both
    carry its Nordic VID) but only one speaks the measurement protocol, so this returns every match
    and the caller tries each until one initialises. Order: an explicit `TOWER_PPK2_PORT` first,
    then a pyserial scan filtered to the Nordic VID/PID (precise: never returns a J-Link), then
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
    while the real-hardware deps live in an uncommitted, gitignored venv. Otherwise records WHY in
    `_dep_notes` (for the startup error line) and returns.

    Uses `find_spec`, not `import`: nothing from `ppk2-api` may run before `_claim_stdout` has
    moved fd 1 off the protocol channel, and an exec here inherits the fds unchanged.

    A `_PPK2D_REEXEC` sentinel (not a path comparison) guards against an exec loop: a venv's
    `bin/python3` symlinks to the base interpreter, so `realpath(sys.executable)` equals the venv
    python's realpath and can't distinguish "already in the venv"."""
    if importlib.util.find_spec("ppk2_api") is not None:
        return
    if os.environ.get("_PPK2D_REEXEC"):
        _dep_notes.append(f"`ppk2_api` is not installed in the venv ({sys.executable}); "
                          "run hil/.venv/bin/pip install ppk2-api pyserial")
        return
    here = os.path.dirname(os.path.abspath(__file__))
    venv_py = os.path.join(here, ".venv", "bin", "python3")
    if os.path.exists(venv_py):
        os.environ["_PPK2D_REEXEC"] = "1"
        os.execv(venv_py, [venv_py, os.path.abspath(__file__), *sys.argv[1:]])
    if os.path.lexists(venv_py):
        _dep_notes.append(f"{venv_py} is a dangling symlink (Python upgraded since the venv was "
                          "made?); recreate it: rm -rf hil/.venv && python3 -m venv hil/.venv && "
                          "hil/.venv/bin/pip install ppk2-api pyserial")
    else:
        _dep_notes.append(f"`ppk2_api` is not importable by {sys.executable} and there is no "
                          "hil/.venv; python3 -m venv hil/.venv && hil/.venv/bin/pip install "
                          "ppk2-api pyserial")


def _claim_stdout():
    """Keep a private duplicate of the real stdout as the protocol channel, then point fd 1 (and
    `sys.stdout`) at stderr, so a stray `print` (`ppk2-api`'s "Measurement outside of range!")
    lands in the log instead of desyncing the line-JSON replies. Call AFTER `_ensure_deps` (an exec
    would otherwise start the venv interpreter with fd 1 already pointing at stderr)."""
    global _proto
    sys.stdout.flush()
    _proto = os.fdopen(os.dup(1), "w", buffering=1)
    os.dup2(2, 1)
    sys.stdout = sys.stderr


def _reply(obj):
    _proto.write(json.dumps(obj) + "\n")
    _proto.flush()


def make_device():
    """Pick the backend: a real PPK2 (trying each candidate CDC endpoint until one
    initialises; a PPK2 presents two) or, ONLY with `TOWER_PPK2_STUB=1`, the modelled stub. Returns
    `(device, backend, port)`; raises with every reason collected when neither applies. The choice
    is also logged to stderr so a power run makes clear whether it measured real silicon."""
    reasons = list(_dep_notes)
    candidates = _ppk2_candidates()
    for port in candidates:
        try:
            dev = Ppk2Real(port)
            print(f"ppk2d: real PPK2 on {port}", file=sys.stderr, flush=True)
            return dev, "real", port
        except Exception as e:
            print(f"ppk2d: {port} not a live PPK2 ({e})", file=sys.stderr, flush=True)
            reasons.append(f"{port}: {e}")
    if not candidates:
        reasons.append("no Nordic PPK2 (VID 0x1915 / PID 0xC00A) enumerated; is it plugged in? "
                       "(TOWER_PPK2_PORT overrides detection)")
    if os.environ.get("TOWER_PPK2_STUB") == "1":
        print("ppk2d: TOWER_PPK2_STUB=1, using the MODELLED STUB (plumbing run, fake readings)",
              file=sys.stderr, flush=True)
        return Ppk2Stub(), "stub", None
    raise RuntimeError("no PPK2 initialised: " + "; ".join(reasons) +
                       " (the modelled stub is opt-in only: TOWER_PPK2_STUB=1)")


def main():
    _ensure_deps()
    _claim_stdout()
    try:
        dev, backend, port = make_device()
    except Exception as e:
        _reply({"ok": False, "error": str(e)})
        sys.exit(1)
    _reply({"ok": True, "backend": backend, "port": port})
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
                median, mean, n = dev.measure(int(req.get("ms", 100)))
                reply = {"ok": True, "median_ua": median, "mean_ua": mean, "samples": n}
            else:
                reply = {"ok": False, "error": f"unknown cmd {cmd!r}"}
        except Exception as e:  # surface any error as a JSON line the harness can see
            reply = {"ok": False, "error": str(e)}
        _reply(reply)


if __name__ == "__main__":
    main()
