# VM supervisor protocol fixtures

Golden frames for `docs/vm-supervisor-protocol.md`. Each file is exactly one
frame as it appears on the wire: compact JSON, fields in the document's order,
ending in a single LF. (The `invalid/` files that test framing break that on
purpose.)

- `requests/valid/`, `responses/valid/`: frames the decoder must accept.
- `requests/invalid/`, `responses/invalid/`: frames the decoder of that direction
  must refuse.
- `responses/refused-hello/`: frames that decode but that the handshake check
  must refuse (a wrong version, a `version_mismatch`, a reply that is not `hello`).

The Rust tests (`src/vm/proto.rs`) and the Swift launcher tests (ADR-0026 slice 3)
read these same files. Every directory is iterated, so a new file is picked up
without editing a test.

**Byte-exact versus semantic.** The Rust tests require every `valid/` fixture to
round-trip byte for byte: decode, then encode, gives the same bytes. Another
encoder need only produce the same JSON value. Swift's `JSONEncoder` may order
keys differently or escape `/`, and that is fine; it must still write a single
line, with an LF, with no `null`.

**Closed sets.** A new state, stop reason, error code, operation or field is a
new protocol version, and so new fixtures under a new version, not a change to a
value here.
