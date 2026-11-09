# tower-protocol

The wire contract every TOWER end builds from the same source (`no_std`): COBS + CRC-32 framing,
the console, management and radio schemas, the taf apps' property tables and the schema tags
derived from them.

[The guide](../../docs/protocol.md), including how to change the wire. Tests: `cargo test -p
tower-protocol`.
