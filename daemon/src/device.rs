//! Stable device identity, persisted to `<data_dir>/device.json`.
//!
//! See docs/12-identity-and-connectivity.md#device-identity and
//! docs/05-persistence.md. Nothing in the MVP consumes this yet; it exists so
//! multi-device clients do not reshape every payload later.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
}

/// Loads `<data_dir>/device.json` if present, otherwise generates one and
/// writes it. `device_id` is a ULID generated once and never changed;
/// `device_name` defaults to the hostname and is user-editable thereafter.
pub fn load_or_create(data_dir: &Path) -> Result<Device> {
    let path = data_dir.join("device.json");

    if let Ok(existing) = fs::read_to_string(&path) {
        return serde_json::from_str(&existing)
            .with_context(|| format!("parsing {}", path.display()));
    }

    let device = Device {
        device_id: Ulid::new().to_string(),
        device_name: hostname(),
        platform: platform(),
    };

    let json = serde_json::to_string_pretty(&device)?;
    fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;

    Ok(device)
}

fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Best-effort hostname lookup. No hostname crate is in the pinned dependency
/// list (docs/02-stack-decisions.md), so this uses the already-pinned `libc`
/// on Unix and the `COMPUTERNAME` environment variable on Windows, falling
/// back to a fixed default rather than failing device.json generation.
fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: buf is a valid, appropriately sized C string buffer; gethostname
        // writes at most buf.len() bytes and null-terminates on success.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(name) = std::str::from_utf8(&buf[..end]) {
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    #[cfg(windows)]
    {
        if let Ok(name) = std::env::var("COMPUTERNAME") {
            if !name.is_empty() {
                return name;
            }
        }
    }
    "teleport-host".to_string()
}
