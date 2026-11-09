# tower-sdk: the TOWER firmware SDK

The Embassy firmware SDK for the TOWER Core Module (STM32L083CZ): the `tower` library (`src/`),
the examples (`examples/`) and the TOWER IoT Kit apps (`apps/`). It is its own Cargo workspace, so
run cargo here (or `just fw <recipe>` from the repo root), never with `--manifest-path`.

- [The SDK guide](../docs/firmware/sdk.md): quick start, the module layout, low power, examples
  and apps, building and flashing.
- The subsystem guides in [`docs/firmware/`](../docs/firmware/) (console, radio, storage, taf,
  gateway, LCD, LED strip, PIR) and [the documentation index](../docs/README.md).

`just examples`, `just apps`, `just build app radio_button`, `just flash example blinky`, `just ci`.
