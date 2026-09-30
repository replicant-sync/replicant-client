//! Sans-IO sync engine core: pure per-document rules and the connection state machine.

pub mod backoff;
pub mod doc;
pub(crate) mod doc_upload;
#[cfg(test)]
mod fuzz_tests;
pub mod hash;
pub mod list_merge;
pub mod machine;
#[cfg(test)]
mod patch_fixture_tests;
pub mod types;
