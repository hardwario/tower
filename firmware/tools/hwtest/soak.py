#!/usr/bin/env python3
"""Soak accounting from an MQTT recording of a running `tower gateway`.

Record with (any length, the bridge and its nodes left alone or driven by a script):

    mosquitto_sub -h 127.0.0.1 -t 'tower/#' -v -F '%I %t %p' > soak.log

then:

    python3 tools/hwtest/soak.py soak.log [--prefix tower/] [--heartbeat 900]

Per node it prints what the recording can prove:

- uplinks heard, and the longest silence between two (against the node's heartbeat: a sleeping
  node that is healthy is never silent for longer);
- events per kind, and events LOST: every event carries the node's running count of its kind, so
  a jump in the count is an event the host never saw (a count that starts over is a reboot);
- every reported state: how often it was reported, its first and last value, its range;
- downlinks: the gateway's transmit outcomes for the node.

And for the gateway: how long the recording is, and every change of its `gateway/status`
(`online`, `degraded`, `offline`).

The exit code is 1 when an event was lost, a node was silent for longer than 1.5 heartbeats, or
the gateway left `online`; 0 otherwise. Standard library only.
"""

import argparse
import datetime
import json
import sys
from collections import defaultdict


def parse_line(line):
    """`<ISO time> <topic> <payload>` -> (datetime, topic, payload) or None."""
    parts = line.rstrip("\n").split(" ", 2)
    if len(parts) < 2:
        return None
    try:
        when = datetime.datetime.strptime(parts[0][:19], "%Y-%m-%dT%H:%M:%S")
    except ValueError:
        return None
    return when, parts[1], parts[2] if len(parts) > 2 else ""


def as_json(payload):
    try:
        return json.loads(payload)
    except ValueError:
        return None


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("recording")
    ap.add_argument("--prefix", default="tower/")
    ap.add_argument("--heartbeat", type=int, default=900, help="the nodes' heartbeat in seconds")
    args = ap.parse_args()

    first = last = None
    status = []  # (when, state)
    nodes = defaultdict(
        lambda: {
            "uplinks": [],  # times
            "events": defaultdict(list),  # kind -> [count]
            "states": defaultdict(list),  # prop -> [value]
            "tx": defaultdict(int),  # outcome -> n
            "kind": "",
            "name": "",
        }
    )
    with open(args.recording, encoding="utf-8", errors="replace") as f:
        for line in f:
            parsed = parse_line(line)
            if parsed is None:
                continue
            when, topic, payload = parsed
            first = first or when
            last = when
            if not topic.startswith(args.prefix):
                continue
            path = topic[len(args.prefix):].split("/")
            data = as_json(payload)
            if path == ["gateway", "status"] and isinstance(data, dict):
                state = data.get("state")
                if not status or status[-1][1] != state:
                    status.append((when, state))
            elif path == ["radio", "tx"] and isinstance(data, dict):
                nodes[data.get("dest", "?")]["tx"][data.get("outcome", "?")] += 1
            elif path[0] == "nodes" and len(path) >= 2:
                node = nodes[path[1]]
                if len(path) == 2 and isinstance(data, dict):
                    node["kind"] = data.get("kind") or node["kind"]
                    node["name"] = data.get("name") or node["name"]
                elif path[2:] == ["uplink"]:
                    node["uplinks"].append(when)
                elif len(path) == 4 and path[2] == "event" and isinstance(data, dict):
                    node["events"][path[3]].append(data.get("count"))
                elif len(path) >= 4 and path[2] == "state" and isinstance(data, dict):
                    # A grouped state is two levels deep (`state/temperature/system`).
                    node["states"]["/".join(path[3:])].append(data.get("value"))

    if first is None:
        sys.exit("no dated lines: record with `mosquitto_sub -v -F '%I %t %p'`")
    failed = False
    span = (last - first).total_seconds()
    print(f"recording: {first} to {last} ({span / 3600:.2f} h)")
    if status:
        print("gateway: " + ", ".join(f"{s} at {w.time()}" for w, s in status))
        if any(s != "online" for _, s in status[1:]) or status[0][1] != "online":
            failed = True
    else:
        print("gateway: no status seen (it is retained: the recorder connected before the bridge?)")

    for addr, n in sorted(nodes.items()):
        if not n["uplinks"] and not n["tx"]:
            continue
        print(f"\nnode {addr} {n['name']!r} ({n['kind'] or 'kind unknown'})")
        ups = n["uplinks"]
        if ups:
            gaps = [(b - a).total_seconds() for a, b in zip(ups, ups[1:])]
            # The silence before the first and after the last uplink counts too.
            gaps += [(ups[0] - first).total_seconds(), (last - ups[-1]).total_seconds()]
            longest = max(gaps)
            verdict = "ok" if longest <= 1.5 * args.heartbeat else "TOO LONG"
            failed |= verdict != "ok"
            print(f"  uplinks: {len(ups)}, the longest silence {longest:.0f} s ({verdict}, heartbeat {args.heartbeat} s)")
        for kind, counts in sorted(n["events"].items()):
            lost = reboots = 0
            for a, b in zip(counts, counts[1:]):
                if not isinstance(a, int) or not isinstance(b, int):
                    continue
                if b <= a:
                    reboots += 1  # the count started over
                else:
                    lost += b - a - 1
            failed |= lost > 0
            print(f"  event {kind}: {len(counts)} seen, {lost} lost, the count started over {reboots} time(s)")
        for prop, values in sorted(n["states"].items()):
            numbers = [v for v in values if isinstance(v, (int, float)) and not isinstance(v, bool)]
            if numbers and len(numbers) == len(values):
                print(
                    f"  state {prop}: {len(values)} reports, {values[0]} first, {values[-1]} last, "
                    f"{min(numbers)} to {max(numbers)}"
                )
            else:
                print(f"  state {prop}: {len(values)} reports, {values[0]!r} first, {values[-1]!r} last")
        if n["tx"]:
            print("  downlinks: " + ", ".join(f"{k} {v}" for k, v in sorted(n["tx"].items())))
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
