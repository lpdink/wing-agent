//! Gateway WebSocket client.

pub mod chunk;
pub mod client;

pub use chunk::ChunkEnvelope;
pub use chunk::Limits;
pub use chunk::Reassembler;
pub use client::CloseReason;
pub use client::GatewayClient;
