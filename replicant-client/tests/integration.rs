//! Integration tests for Rust client ↔ Phoenix server communication.
//!
//! Run with: RUN_INTEGRATION_TESTS=1 cargo test --test integration
//!
//! Requires Phoenix server running on localhost:4000

#[path = "phoenix_integration/mod.rs"]
mod phoenix_integration;
