//! Domain types for RPC requests and responses.
//!
//! These types do not depend on the wire format. The conversions from and to the generated protobuf
//! messages live in the private `conversions` module of the `rpc` module.

pub mod account;
pub mod account_vault;
pub mod limits;
pub mod note;
pub mod nullifier;
pub mod status;
pub mod storage_map;
pub mod sync;
pub mod transaction;
