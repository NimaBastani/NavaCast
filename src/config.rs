use crate::error::Error;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default = "default_stream_listen")]
    pub stream_listen: String,
    #[serde(default = "default_http_listen")]
    pub http_listen: String,
    #[serde(default)]
    pub public_listen: String,
    #[serde(default = "default_stream_url")]
    pub stream_public_url: String,
    #[serde(default = "default_http_url")]
    pub http_public_url: String,
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    #[serde(default = "default_upload")]
    pub max_upload_bytes: u64,
    #[serde(default = "default_source_timeout")]
    pub source_timeout_sec: u64,
    #[serde(default = "default_max_ip")]
    pub max_conns_per_ip: usize,
    #[serde(default = "default_metaint")]
    pub meta_int: usize,
    #[serde(default = "default_fade")]
    pub fade_ms: u64,
    #[serde(default)]
    pub tz_offset_min: i32,
    /// Allow a relay to pull loopback, link-local, or private addresses.
    #[serde(default)]
    pub allow_private_relay: bool,
    /// Cap on every recording file under data_dir, in bytes.
    #[serde(default = "default_recording_bytes")]
    pub max_recording_bytes: u64,
}

fn default_data_dir() -> String { "data".into() }
fn default_stream_listen() -> String { ":8000".into() }
fn default_http_listen() -> String { "127.0.0.1:8088".into() }
fn default_stream_url() -> String { "http://127.0.0.1:8000".into() }
fn default_http_url() -> String { "http://127.0.0.1:8088".into() }
fn default_upload() -> u64 { 64 << 20 }
fn default_source_timeout() -> u64 { 15 }
fn default_max_ip() -> usize { 16 }
fn default_metaint() -> usize { 16_000 }
fn default_fade() -> u64 { 250 }
fn default_recording_bytes() -> u64 { crate::record::DEFAULT_MAX_BYTES }

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            stream_listen: default_stream_listen(),
            http_listen: default_http_listen(),
            public_listen: String::new(),
            stream_public_url: default_stream_url(),
            http_public_url: default_http_url(),
            trusted_proxies: vec!["127.0.0.1".into(), "::1".into()],
            max_upload_bytes: default_upload(),
            source_timeout_sec: default_source_timeout(),
            max_conns_per_ip: default_max_ip(),
            meta_int: default_metaint(),
            fade_ms: default_fade(),
            tz_offset_min: 0,
            allow_private_relay: false,
            max_recording_bytes: default_recording_bytes(),
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self, Error> {
        let mut cfg = if let Some(path) = path {
            let text = fs::read_to_string(path)?;
            serde_json::from_str(&text).map_err(|e| Error::Invalid(format!("config: {e}")))?
        } else if Path::new("navad.json").exists() {
            let text = fs::read_to_string("navad.json")?;
            serde_json::from_str(&text).map_err(|e| Error::Invalid(format!("config: {e}")))?
        } else {
            Config::default()
        };
        if cfg.trusted_proxies.is_empty() {
            cfg.trusted_proxies = vec!["127.0.0.1".into(), "::1".into()];
        }
        override_env(&mut cfg);
        cfg.stream_public_url = cfg.stream_public_url.trim_end_matches('/').to_string();
        cfg.http_public_url = cfg.http_public_url.trim_end_matches('/').to_string();
        if cfg.meta_int < 256 {
            cfg.meta_int = 16_000;
        }
        if cfg.source_timeout_sec == 0 {
            cfg.source_timeout_sec = 15;
        }
        if cfg.max_conns_per_ip == 0 {
            cfg.max_conns_per_ip = 16;
        }
        if cfg.max_upload_bytes == 0 {
            cfg.max_upload_bytes = 64 << 20;
        }
        if cfg.max_recording_bytes == 0 {
            cfg.max_recording_bytes = default_recording_bytes();
        }
        Ok(cfg)
    }

    pub fn normalize_listen(addr: &str) -> String {
        if let Some(port) = addr.strip_prefix(':') {
            format!("0.0.0.0:{port}")
        } else {
            addr.to_string()
        }
    }
}

fn override_env(cfg: &mut Config) {
    if let Ok(v) = std::env::var("NAVA_DATA_DIR") {
        if !v.is_empty() { cfg.data_dir = v; }
    }
    if let Ok(v) = std::env::var("NAVA_STREAM_LISTEN") {
        if !v.is_empty() { cfg.stream_listen = v; }
    }
    if let Ok(v) = std::env::var("NAVA_HTTP_LISTEN") {
        if !v.is_empty() { cfg.http_listen = v; }
    }
    if let Ok(v) = std::env::var("NAVA_PUBLIC_LISTEN") {
        cfg.public_listen = v;
    }
    if let Ok(v) = std::env::var("NAVA_STREAM_PUBLIC_URL") {
        if !v.is_empty() { cfg.stream_public_url = v; }
    }
    if let Ok(v) = std::env::var("NAVA_HTTP_PUBLIC_URL") {
        if !v.is_empty() { cfg.http_public_url = v; }
    }
    if let Ok(v) = std::env::var("NAVA_ALLOW_PRIVATE_RELAY") {
        let on = matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
        cfg.allow_private_relay = on;
    }
    if let Ok(v) = std::env::var("NAVA_MAX_UPLOAD_BYTES") {
        if let Ok(n) = v.parse() {
            if n > 0 { cfg.max_upload_bytes = n; }
        }
    }
    if let Ok(v) = std::env::var("NAVA_MAX_RECORDING_BYTES") {
        if let Ok(n) = v.parse() {
            if n > 0 { cfg.max_recording_bytes = n; }
        }
    }
}

pub fn admin_from_env() -> (Option<String>, Option<String>) {
    let user = std::env::var("NAVA_ADMIN_USER").ok().filter(|s| !s.is_empty());
    let pass = std::env::var("NAVA_ADMIN_PASSWORD").ok().filter(|s| !s.is_empty());
    (user, pass)
}
