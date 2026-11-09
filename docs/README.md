# TOWER documentation

All of TOWER's documentation, by component, mirroring the code. The repository's
[README](../README.md) has the quick start; [TASKS.md](../TASKS.md) holds the open work and the
decisions.

## Firmware (`firmware/`)

| Guide | What it covers |
|---|---|
| [SDK](firmware/sdk.md) | Start here: quick start, the module layout, LED indication, low power, examples and apps, building and flashing |
| [Console](firmware/console.md) | The framed host link (logs, events, the shell), logging style, the Core Module r1 native USB |
| [Radio](firmware/radio.md) | The SPIRIT1 stack: the secured network layer, pairing, node classes, spectrum rules (EU 868, US 915) |
| [Storage](firmware/storage.md) | The power-loss-safe EEPROM key-value store, wear, compaction |
| [taf](firmware/taf.md) | The TOWER Application Framework and its radio node apps, the app contract |
| [Gateway](firmware/gateway.md) | The radio product end to end: node, `radio_gateway` on the Radio Dongle, `tower gateway` |
| [HMI](firmware/hmi.md) | The LCD Module driver and the HMI framework: drawing, fonts, menus, input |
| [LED strip](firmware/strip.md) | The WS2812/SK6812 driver, the render pipeline, effects, the `led_strip` app |
| [PIR](firmware/pir.md) | The PIR Module (PYQ 1648 DigiPyro): its interfaces, timing, tuning |
| [Test plan](firmware/test-plan.md) | The hardware validation suite: every subsystem's pass signals and edge cases |
| [Hardware test scripts](firmware/hwtest.md) | `firmware/tools/hwtest/`; recorded runs in [`results/`](firmware/results/) |

## Host (`cli/`)

| Guide | What it covers |
|---|---|
| [CLI](cli/cli.md) | The `tower` binary: install, usage, the gateway and MQTT bridge, flashing, exit codes |
| [MQTT API](cli/mqtt-api.md) | The topics and payloads of `tower gateway` |

## Shared crates (`crates/`)

| Guide | What it covers |
|---|---|
| [Protocol](protocol.md) | `tower-protocol`: framing, the schemas, the schema tags, changing the wire |
| [Flasher](flasher.md) | `tower-flasher`: the UART and USB DFU bootloader engines, readout protection |

The other crates are documented in their rustdoc (`just doc` at the repo root).

## Testing

| Guide | What it covers |
|---|---|
| [Test strategy](test-strategy.md) | What the tests cover, the layers worth adding, what comes before the first users |
| [HIL harness](hil/hil.md) | `hil/`: the hardware-in-the-loop harness |
| [HIL bench](hil/bench.md) | The bench's inventory, cabling and setup |
| [Fuzzing](fuzzing.md) | `fuzz/`: the cargo-fuzz targets |

## Security

[Security model](security-model.md): the threat model, the mechanisms, the residual risks.
