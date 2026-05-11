//! Reference consumer used to smoke-test the C producer example.
//!
//! Binds the AF_UNIX socket the C producer would target by default
//! (derived from LOCAL_PSK via `derive_band_id`), prints the first
//! Stream packet it receives — including parsed StreamHeader and
//! DmabufDescriptor — and exits.

use std::sync::Arc;

use anyhow::Result;
use telesthete::wire::{DmabufDescriptor, StreamFlags, StreamHeader, STREAM_HEADER_LEN};
use telesthete::{
    derive_band_id, derive_key, ChannelType, UnixTransport, LOCAL_PSK, SOCKET_DIR_ENV,
    SOCKET_DIR_FALLBACK,
};

#[tokio::main]
async fn main() -> Result<()> {
    let psk = LOCAL_PSK.as_bytes();
    let band = derive_band_id(psk);
    let key = derive_key(psk);

    let dir = std::env::var(SOCKET_DIR_ENV).unwrap_or_else(|_| SOCKET_DIR_FALLBACK.to_string());
    let mut path = std::path::PathBuf::from(dir);
    path.push("telesthete");
    std::fs::create_dir_all(&path)?;
    let mut hex = String::with_capacity(36);
    for b in band.iter() {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    path.push(format!("{hex}.sock"));
    let _ = std::fs::remove_file(&path);

    println!("consumer: binding {}", path.display());
    let transport = Arc::new(UnixTransport::bind(&path, key, band).await?);
    let mut rx = transport.route(ChannelType::Stream).await;
    let _join = transport.spawn_recv_loop();

    let inbound = rx
        .recv()
        .await
        .ok_or_else(|| anyhow::anyhow!("recv loop closed before first packet"))?;

    let payload = &inbound.payload;
    if payload.len() < STREAM_HEADER_LEN {
        anyhow::bail!("short payload {}", payload.len());
    }
    let (hdr, rest) = StreamHeader::parse(payload)?;
    println!(
        "Stream ch={} hdr={hdr:?}",
        inbound.header.channel_id
    );
    println!("  fds received: {}", inbound.fds.len());
    if hdr.flags.contains(StreamFlags::DMABUF) {
        let desc = DmabufDescriptor::parse(rest)?;
        println!("  dmabuf {desc:#?}");
    }
    Ok(())
}
