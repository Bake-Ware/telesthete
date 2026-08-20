//! Telesthete Hub (`telesthete-hub`) — the reference relay for the Telesthete
//! wire protocol, SPEC §10.
//!
//! Matches peers by the cleartext `band_id` and bridges opaque ciphertext
//! between them across UDP (§9.1), WSS (§9.3), WebTransport (§9.6), and AF_UNIX
//! (§9.4). Holds no PSK; cannot decrypt. See `CONFORMANCE.md` for the
//! claim-by-claim inventory and the library crate (`lib.rs`) for the internals.

use std::sync::Arc;

use telesthitium::{Config, Registry};
use tokio::sync::watch;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = Config::from_env();
    tracing::info!(
        udp = ?cfg.udp_bind,
        ws = ?cfg.ws_bind,
        wt = ?cfg.wt_bind,
        unix = ?cfg.unix_dir,
        peer_ttl_secs = cfg.peer_ttl.as_secs(),
        "telesthete-hub starting"
    );

    let registry = Arc::new(Registry::new(cfg.limits));

    // Prune sweep.
    {
        let registry = registry.clone();
        let ttl = cfg.peer_ttl;
        let interval = cfg.prune_interval;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                let (evicted, bands) = registry.prune(ttl);
                if evicted > 0 {
                    tracing::info!(evicted, bands, "prune cycle");
                }
            }
        });
    }

    // Resolve a TLS identity once if any secure transport needs it.
    let cert = if cfg.needs_tls() {
        let c = cfg.tls_identity()?;
        tracing::info!(cert_sha256 = %c.sha256_hex(), "TLS identity ready");
        Some(c)
    } else {
        None
    };

    // Fan the shutdown signal out to every transport.
    let (sd_tx, sd_rx) = watch::channel(false);
    let mut tasks = Vec::new();

    // Federation (SPEC §10 extension) — pool this hub's registry with linked
    // hubs. Entirely inert unless HUB_FED_LISTEN or HUB_FED_LINK is set, so a
    // standard single-hub deployment is byte-for-byte unchanged.
    if let Some(fed_cfg) = telesthitium::federation::FedConfig::from_env() {
        tracing::info!(
            listen = ?fed_cfg.listen,
            links = ?fed_cfg.links,
            inbound_active = fed_cfg.inbound_active,
            "federation enabled"
        );
        tasks.extend(telesthitium::federation::spawn(
            fed_cfg,
            registry.clone(),
            sd_rx.clone(),
        ));
    }

    if let Some(bind) = cfg.udp_bind {
        let (registry, mut rx, q) = (registry.clone(), sd_rx.clone(), cfg.conn_queue);
        tasks.push(tokio::spawn(async move {
            let sd = async {
                let _ = rx.wait_for(|v| *v).await;
            };
            if let Err(e) = telesthitium::udp::serve(bind, registry, q, sd).await {
                tracing::error!(error = %e, "udp transport failed");
            }
        }));
    }

    if let Some(bind) = cfg.ws_bind {
        let (registry, mut rx, q, path) =
            (registry.clone(), sd_rx.clone(), cfg.conn_queue, cfg.ws_path.clone());
        let tls = if cfg.ws_tls { cert.clone() } else { None };
        tasks.push(tokio::spawn(async move {
            let sd = async {
                let _ = rx.wait_for(|v| *v).await;
            };
            if let Err(e) = telesthitium::ws::serve(bind, registry, q, path, tls, sd).await {
                tracing::error!(error = %e, "ws transport failed");
            }
        }));
    }

    if let Some(bind) = cfg.wt_bind {
        let wt_cert = cert.clone().expect("needs_tls() guarantees a cert when wt is enabled");
        let (registry, mut rx, q) = (registry.clone(), sd_rx.clone(), cfg.conn_queue);
        tasks.push(tokio::spawn(async move {
            let sd = async {
                let _ = rx.wait_for(|v| *v).await;
            };
            if let Err(e) = telesthitium::wt::serve(bind, registry, q, wt_cert, sd).await {
                tracing::error!(error = %e, "webtransport transport failed");
            }
        }));
    }

    if let Some(dir) = cfg.unix_dir.clone() {
        let (registry, mut rx, q) = (registry.clone(), sd_rx.clone(), cfg.conn_queue);
        tasks.push(tokio::spawn(async move {
            let sd = async {
                let _ = rx.wait_for(|v| *v).await;
            };
            if let Err(e) = telesthitium::unix::serve(dir, registry, q, sd).await {
                tracing::error!(error = %e, "af_unix transport failed");
            }
        }));
    }

    if tasks.is_empty() {
        tracing::warn!("no transports enabled");
    }

    shutdown_signal().await;
    tracing::info!("shutdown signal received; stopping transports");
    let _ = sd_tx.send(true);
    for t in tasks {
        let _ = t.await;
    }

    tracing::info!("telesthete-hub stopped");
    Ok(())
}

/// Resolves on SIGINT or SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = ctrl_c => tracing::info!("SIGINT received"),
                    _ = term.recv() => tracing::info!("SIGTERM received"),
                }
            }
            Err(_) => {
                let _ = ctrl_c.await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}
