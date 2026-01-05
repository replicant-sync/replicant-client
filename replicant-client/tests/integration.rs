//! Integration tests for Rust client ↔ Phoenix server communication.
//!
//! Run with:
//!   REPLICANT_API_KEY="rpa_..." REPLICANT_API_SECRET="rps_..." \
//!   RUN_INTEGRATION_TESTS=1 cargo test --test integration
//!
//! Requires Phoenix server running on localhost:4000 and valid API credentials.

#[path = "phoenix_integration/mod.rs"]
mod phoenix_integration;
