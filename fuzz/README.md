# fuzz

cargo-fuzz targets for the hardware-free crates (the protocol, the key-value store, the shell and
radio kernels). Its own workspace, on nightly. [The guide](../docs/fuzzing.md): setup, running, the
targets, seeds, from a crash to a regression test.

`just fuzz <target> [secs]`, `just fuzz-all` (from the repo root).
