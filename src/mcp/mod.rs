//! MCP (Model Context Protocol) server implementation.
//!
//! Wire layer: newline-delimited JSON-RPC 2.0 over stdio.
//! Negotiated protocol version: see [`PROTOCOL_VERSION`].

pub mod jsonrpc;
pub mod resources;
pub mod server;
pub mod tools;
pub mod transport;

/// MCP protocol version this server speaks. The `initialize` handler
/// negotiates: if the client requests this exact version, we echo it
/// back; otherwise we still respond with this version and let the
/// client decide whether to proceed (per MCP spec the client may
/// disconnect on mismatch).
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Server identity advertised in the `initialize` response.
pub const SERVER_NAME: &str = "mneme";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
