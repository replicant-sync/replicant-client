//! Protocol v2 transport: wire payloads, Phoenix framing, and the websocket.

pub mod codec;
pub mod socket;
#[cfg(test)]
pub(crate) mod test_server;
pub mod wire;
