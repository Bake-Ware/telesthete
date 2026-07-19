//! Telesthitium — the reference relay hub for the Telesthete wire protocol
//! (SPEC §10).
//!
//! The hub matches peers by the cleartext `band_id` and bridges opaque
//! ciphertext between them across every transport §10 names: UDP (§9.1),
//! WebSocket/WSS (§9.3), WebTransport/HTTP-3 (§9.6), and AF_UNIX (§9.4). It
//! holds no PSK and cannot decrypt — it reads only the cleartext header fields
//! (`band_id`, `channel_type`, `channel_id`).
//!
//! The claim-by-claim conformance inventory is in `CONFORMANCE.md`.
//!
//! ## Architecture
//! - [`frame`] — cleartext header inspection for routing (§1).
//! - [`registry`] — the transport-agnostic relay core: bands, peers, TTL, caps.
//! - [`config`] — environment-driven configuration.
//! - [`udp`] — the UDP transport (§9.1).
//!
//! The canonical unit of relay is one complete Telesthete frame; each transport
//! de-frames on ingress and re-frames on egress, so the [`registry`] moves only
//! whole frames and never inspects payloads.

pub mod config;
pub mod frame;
pub mod registry;
pub mod tls;
pub mod udp;
pub mod unix;
pub mod ws;
pub mod wt;

pub use config::Config;
pub use frame::{route_info, RouteInfo};
pub use registry::{Limits, PeerKey, RegError, Registry, Sink};
pub use tls::HubCert;
