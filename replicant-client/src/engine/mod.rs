//! Sans-IO sync engine core: pure per-document rules and the connection state machine.

pub mod backoff;
pub mod doc;
pub mod doc_upload;
#[cfg(test)]
mod fuzz_tests;
pub mod hash;
pub mod machine;
pub mod types;
