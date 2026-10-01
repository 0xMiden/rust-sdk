# Client Protobuf Types

Protobuf schemas and conversions for the miden-client types.

- Schemas for input notes, output notes, transactions and the protocol types they contain, under `proto/client`
- The `ProtobufValue` trait, with `encode`, `decode` and `decode_unchecked`
- Protocol objects reuse the messages and conversions from `miden-objects`

## Quick Start

Add to `Cargo.toml`:

```toml
miden-client-proto = { version = "0.17.0-rc.5" }
```

`decode` runs all checks on the value. `decode_unchecked` can skip the expensive checks, so use it only for bytes from a trusted source, such as a store that this crate wrote.

## Usage

Every type that implements `ProtobufValue` goes through `encode` and `decode`:

```rust
use miden_client::transaction::TransactionStatus;
use miden_client_proto::{ProtoDecodeError, decode, encode};

fn round_trip(status: &TransactionStatus) -> Result<TransactionStatus, ProtoDecodeError> {
    let bytes = encode(status);
    decode(&bytes)
}
```

An output note state is encoded without the script of its recipient, so a store can keep each script once. The reader passes the script back when it decodes the state:

```rust
let bytes = encode_output_note_state_without_script(note.state());
let state = decode_output_note_state_without_script(&bytes, script)?;
```

## Schema changes

Stored bytes must stay readable after a schema change:

- Do not change or reuse a field number. Reserve the number of a removed field.
- A message field can be absent in bytes from an older version. Read a required field through `required`, which returns an error that names the field.
- A scalar field without `optional` reads as zero when it is absent. Declare a new scalar field as `optional` when the reader must detect that it is absent.

## License
This project is licensed under the MIT License. See the [LICENSE](../../LICENSE) file for details.
