//! PSK provisioning (spec §3). Load a 32-byte key from config: hex or `base64:` line.

use std::path::PathBuf;

pub fn default_path() -> PathBuf {
    if let Ok(p) = std::env::var("TELESTHETE_PSK_FILE") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
        });
    base.join("telesthete").join("psk")
}

pub fn load(path: Option<&str>) -> Result<[u8; 32], String> {
    let p = path.map(PathBuf::from).unwrap_or_else(default_path);
    let raw = std::fs::read_to_string(&p).map_err(|e| format!("psk {}: {e}", p.display()))?;
    parse(raw.trim())
}

pub fn parse(s: &str) -> Result<[u8; 32], String> {
    let bytes = if let Some(b64) = s.strip_prefix("base64:") {
        b64_decode(b64.trim())?
    } else {
        hex_decode(s)?
    };
    if bytes.len() != 32 {
        return Err(format!("psk must be 32 bytes, got {}", bytes.len()));
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(&bytes);
    Ok(k)
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return Err("hex psk odd length".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "bad hex in psk".to_string()))
        .collect()
}

fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut rev = [255u8; 256];
    for (i, &c) in T.iter().enumerate() {
        rev[c as usize] = i as u8;
    }
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for &c in s.as_bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = rev[c as usize];
        if v == 255 {
            return Err("bad base64 in psk".into());
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}
