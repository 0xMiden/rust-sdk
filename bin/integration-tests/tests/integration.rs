//! Integration test wrappers
//!
//! This module includes auto-generated test wrappers from the build script. The actual test
//! implementations are generated in OUT_DIR and included here.

include!(concat!(env!("OUT_DIR"), "/integration_tests.rs"));
