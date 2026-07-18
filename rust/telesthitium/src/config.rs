//! Hub configuration from the environment.
//!
//! Every tunable has a documented default; a malformed value falls back to the
//! default and is never fatal (conformance J1). Defaults are drawn from the
//! protocol constants where the spec defines them (§12.4).
//!
//! UDP (§9.1) is on by default. The other transports are opt-in via their bind
//! address / directory, since they need ports, privileges, or a runtime dir the
//! operator chooses: `HUB_WS_BIND`, `HUB_WT_BIND`, `HUB_UNIX_DIR`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use crate::registry::Limits;
use crate::tls::{self, HubCert, MAX_SELF_SIGNED_DAYS};

/// Default UDP bind (SPEC §9.1 uses no fixed port; 7474 is the project default).
pub const DEFAULT_UDP_BIND: &str = "0.0.0.0:7474";
/// `PEER_TIMEOUT` (§12.4) = 3× `KEEPALIVE_INTERVAL`. A peer silent this long is
/// considered gone, so it is the natural relay eviction threshold.
pub const DEFAULT_PEER_TTL_SECS: u64 = 15;
/// `KEEPALIVE_INTERVAL` (§12.4) — sweep at least once per keepalive.
pub const DEFAULT_PRUNE_SECS: u64 = 5;
pub const DEFAULT_MAX_BANDS: usize = 4096;
pub const DEFAULT_MAX_PEERS_PER_BAND: usize = 256;
pub const DEFAULT_CONN_QUEUE: usize = 1024;
pub const DEFAULT_UDP_VALIDATION_PACKETS: u32 = 2;

/// Resolved hub configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// UDP listen address, or `None` if UDP is disabled (`HUB_BIND=off`).
    pub udp_bind: Option<SocketAddr>,
    /// WSS/WS listen address (`HUB_WS_BIND`), or `None` to disable.
    pub ws_bind: Option<SocketAddr>,
    /// WS endpoint path (`HUB_WS_PATH`, default `/band`).
    pub ws_path: String,
    /// Terminate native TLS on the WS listener (`HUB_WS_TLS`). When false the
    /// listener is plain WS for a TLS-terminating reverse proxy.
    pub ws_tls: bool,
    /// WebTransport listen address (`HUB_WT_BIND`), or `None` to disable. Needs
    /// a TLS identity (QUIC mandates TLS 1.3).
    pub wt_bind: Option<SocketAddr>,
    /// AF_UNIX socket directory (`HUB_UNIX_DIR`), or `None` to disable.
    pub unix_dir: Option<PathBuf>,
    pub peer_ttl: Duration,
    pub prune_interval: Duration,
    /// Per-connection outbound queue depth (bounds a slow peer's memory, D3).
    pub conn_queue: usize,
    pub limits: Limits,
    /// Operator-supplied cert/key PEM paths (`HUB_TLS_CERT` / `HUB_TLS_KEY`).
    pub tls_cert_path: Option<PathBuf>,
    pub tls_key_path: Option<PathBuf>,
    /// SANs for an auto-generated self-signed identity (`HUB_TLS_SANS`).
    pub tls_sans: Vec<String>,
}

/// Parse `s` if present and valid, else `default`. Lenient: never panics.
fn parse_or<T: FromStr>(s: Option<String>, default: T) -> T {
    match s {
        Some(v) => v.trim().parse().unwrap_or(default),
        None => default,
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn env_bool(name: &str) -> bool {
    matches!(
        env(name).map(|v| v.to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Optional socket address: `off`/empty disables; a bad value warns + disables.
fn opt_addr(name: &str) -> Option<SocketAddr> {
    match env(name) {
        None => None,
        Some(v) if v.trim().eq_ignore_ascii_case("off") => None,
        Some(v) => match v.trim().parse() {
            Ok(a) => Some(a),
            Err(_) => {
                tracing::warn!(var = name, value = %v, "invalid address, disabling");
                None
            }
        },
    }
}

/// Resolve the `HUB_BIND` directive. `None` = UDP disabled; `Some` = bind
/// address (falling back to the default on empty/invalid input). Trims before
/// every comparison so `" off"` disables rather than failing open. Pure, for
/// testability.
fn resolve_udp_bind(raw: Option<&str>) -> Option<SocketAddr> {
    let default = || DEFAULT_UDP_BIND.parse().ok();
    match raw.map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case("off") => None,
        Some("") => default(),
        Some(v) => match v.parse() {
            Ok(addr) => Some(addr),
            Err(_) => {
                tracing::warn!(value = %v, "invalid HUB_BIND, using default");
                default()
            }
        },
        None => default(),
    }
}

impl Config {
    /// Build config from the environment, warning (not failing) on bad values.
    pub fn from_env() -> Self {
        let udp_bind = resolve_udp_bind(std::env::var("HUB_BIND").ok().as_deref());

        let unix_dir = match env("HUB_UNIX_DIR") {
            Some(v) if v.eq_ignore_ascii_case("off") => None,
            Some(v) => Some(PathBuf::from(v)),
            None => None,
        };

        let tls_sans = env("HUB_TLS_SANS")
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            // A value like "," yields no SANs; fall back to the default rather
            // than an empty list that could make identity generation fail (J1).
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec!["localhost".to_string()]);

        Config {
            udp_bind,
            ws_bind: opt_addr("HUB_WS_BIND"),
            ws_path: env("HUB_WS_PATH").unwrap_or_else(|| crate::ws::DEFAULT_WS_PATH.to_string()),
            ws_tls: env_bool("HUB_WS_TLS"),
            wt_bind: opt_addr("HUB_WT_BIND"),
            unix_dir,
            peer_ttl: Duration::from_secs(parse_or(env("HUB_PEER_TTL_SECS"), DEFAULT_PEER_TTL_SECS)),
            prune_interval: Duration::from_secs(parse_or(
                env("HUB_PRUNE_SECS"),
                DEFAULT_PRUNE_SECS,
            )),
            conn_queue: parse_or(env("HUB_CONN_QUEUE"), DEFAULT_CONN_QUEUE),
            limits: Limits {
                max_bands: parse_or(env("HUB_MAX_BANDS"), DEFAULT_MAX_BANDS),
                max_peers_per_band: parse_or(
                    env("HUB_MAX_PEERS_PER_BAND"),
                    DEFAULT_MAX_PEERS_PER_BAND,
                ),
                udp_validation_packets: parse_or(
                    env("HUB_UDP_VALIDATION_PACKETS"),
                    DEFAULT_UDP_VALIDATION_PACKETS,
                ),
            },
            tls_cert_path: env("HUB_TLS_CERT").map(PathBuf::from),
            tls_key_path: env("HUB_TLS_KEY").map(PathBuf::from),
            tls_sans,
        }
    }

    /// Whether any transport that needs a TLS identity is enabled.
    pub fn needs_tls(&self) -> bool {
        self.wt_bind.is_some() || (self.ws_bind.is_some() && self.ws_tls)
    }

    /// Resolve the TLS identity: an operator cert/key pair if configured, else a
    /// fresh self-signed ECDSA P-256 identity (§9.6) for `tls_sans`.
    pub fn tls_identity(&self) -> anyhow::Result<HubCert> {
        match (&self.tls_cert_path, &self.tls_key_path) {
            (Some(c), Some(k)) => {
                let cert_pem = std::fs::read_to_string(c)?;
                let key_pem = std::fs::read_to_string(k)?;
                Ok(tls::from_pem(&cert_pem, &key_pem)?)
            }
            _ => {
                let sans: Vec<&str> = self.tls_sans.iter().map(String::as_str).collect();
                Ok(tls::self_signed(&sans, MAX_SELF_SIGNED_DAYS)?)
            }
        }
    }

    fn default_bind() -> Option<SocketAddr> {
        DEFAULT_UDP_BIND.parse().ok()
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            udp_bind: Self::default_bind(),
            ws_bind: None,
            ws_path: crate::ws::DEFAULT_WS_PATH.to_string(),
            ws_tls: false,
            wt_bind: None,
            unix_dir: None,
            peer_ttl: Duration::from_secs(DEFAULT_PEER_TTL_SECS),
            prune_interval: Duration::from_secs(DEFAULT_PRUNE_SECS),
            conn_queue: DEFAULT_CONN_QUEUE,
            limits: Limits::default(),
            tls_cert_path: None,
            tls_key_path: None,
            tls_sans: vec!["localhost".to_string()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_parsing_is_lenient() {
        // J1 — junk falls back to the default; valid parses; absent uses default.
        assert_eq!(parse_or::<u64>(Some("banana".into()), 15), 15);
        assert_eq!(parse_or::<u64>(Some("42".into()), 15), 42);
        assert_eq!(parse_or::<u64>(Some("  7 ".into()), 15), 7);
        assert_eq!(parse_or::<u64>(None, 15), 15);
    }

    #[test]
    fn hub_bind_off_is_honored_despite_whitespace() {
        // #3 regression — " off" must disable UDP, not fail open to 0.0.0.0.
        let dflt = DEFAULT_UDP_BIND.parse().ok();
        assert_eq!(resolve_udp_bind(Some(" off")), None);
        assert_eq!(resolve_udp_bind(Some("OFF\n")), None);
        assert_eq!(resolve_udp_bind(Some("\toff ")), None);
        assert_eq!(resolve_udp_bind(Some("127.0.0.1:9")), "127.0.0.1:9".parse().ok());
        assert_eq!(resolve_udp_bind(Some("  ")), dflt); // empty -> default
        assert_eq!(resolve_udp_bind(Some("garbage")), dflt); // invalid -> default
        assert_eq!(resolve_udp_bind(None), dflt);
    }

    #[test]
    fn default_ttl_is_peer_timeout() {
        // C4 — the relay TTL default equals the protocol's PEER_TIMEOUT (§12.4).
        assert_eq!(DEFAULT_PEER_TTL_SECS, 15);
    }

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.udp_bind, Some("0.0.0.0:7474".parse().unwrap()));
        assert!(c.ws_bind.is_none() && c.wt_bind.is_none() && c.unix_dir.is_none());
        assert!(!c.needs_tls());
        assert!(c.limits.max_bands > 0 && c.limits.max_peers_per_band > 0);
    }

    #[test]
    fn self_signed_identity_resolves() {
        let c = Config::default();
        let cert = c.tls_identity().unwrap();
        assert_eq!(cert.sha256.len(), 32);
    }
}
