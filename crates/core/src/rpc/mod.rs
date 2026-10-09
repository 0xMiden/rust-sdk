//! Defines the RPC types shared by the client and the store implementations.

pub mod encryption;

mod limits;
pub use limits::{RPC_LIMITS_STORE_SETTING, RpcLimits};
