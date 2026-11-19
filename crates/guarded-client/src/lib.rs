//! A wrapper client for one guarded multisig account that orchestrates the interaction between
//! `miden-client` and Guardian.

#[allow(clippy::all, clippy::pedantic, dead_code)]
mod generated {
    tonic::include_proto!("guardian");
}
