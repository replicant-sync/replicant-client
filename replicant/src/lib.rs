//! Replicant - Offline-first document synchronization
//!
//! This crate provides a unified API for the Replicant sync system.
//!
//! # Example
//!
//! ```ignore
//! use replicant::Client;
//!
//! let client = Client::new("sqlite:data.db", "wss://server/ws", ...)?;
//! client.create_document(json)?;
//! ```

// Re-export client types
pub use replicant_client::Client;

// Server is now Elixir/Phoenix - see replicant-server/ in repo root

// Re-export core types that external applications may need
pub use replicant_core::errors::SyncError;
pub use replicant_core::models::Document;
pub use replicant_core::protocol::{ClientMessage, ServerMessage};
pub use replicant_core::SyncResult;
