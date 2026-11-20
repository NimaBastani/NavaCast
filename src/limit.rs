use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct IpCounter {
    inner: Mutex<HashMap<String, usize>>,
    max: usize,
}

impl IpCounter {
    pub fn new(max: usize) -> Self {
        Self { inner: Mutex::new(HashMap::new()), max: max.max(1) }
    }

    pub fn would_allow(&self, ip: &str) -> bool {
        let g = self.inner.lock().expect("ip");
        g.get(ip).copied().unwrap_or(0) < self.max
    }

    pub fn acquire(&self, ip: &str) -> bool {
        let mut g = self.inner.lock().expect("ip");
        let n = g.entry(ip.to_string()).or_insert(0);
        if *n >= self.max {
            return false;
        }
        *n += 1;
        true
    }

    pub fn release(&self, ip: &str) {
        let mut g = self.inner.lock().expect("ip");
        match g.get_mut(ip) {
            Some(n) if *n > 1 => *n -= 1,
            Some(_) => { g.remove(ip); }
            None => {}
        }
    }
}

pub struct FailWindow {
    inner: Mutex<HashMap<String, Vec<Instant>>>,
    max: usize,
    window: Duration,
}

impl FailWindow {
    pub fn new(max: usize, window: Duration) -> Self {
        Self { inner: Mutex::new(HashMap::new()), max, window }
    }

    pub fn allow(&self, key: &str) -> bool {
        let mut g = self.inner.lock().expect("fail");
        Self::prune(&mut g, key, self.window);
        g.get(key).map(|v| v.len()).unwrap_or(0) < self.max
    }

    pub fn fail(&self, key: &str) {
        let mut g = self.inner.lock().expect("fail");
        Self::prune(&mut g, key, self.window);
        g.entry(key.to_string()).or_default().push(Instant::now());
    }

    fn prune(g: &mut HashMap<String, Vec<Instant>>, key: &str, window: Duration) {
        let Some(v) = g.get_mut(key) else { return };
        let cut = Instant::now().checked_sub(window).unwrap_or_else(Instant::now);
        v.retain(|t| *t >= cut);
        if v.is_empty() {
            g.remove(key);
        }
    }
}

#[derive(Clone, Default)]
pub struct Trusted {
    exact: Vec<IpAddr>,
}

impl Trusted {
    pub fn parse(items: &[String]) -> Self {
        let mut exact = Vec::new();
        for it in items {
            if let Ok(ip) = it.parse::<IpAddr>() {
                exact.push(ip);
            }
        }
        Self { exact }
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.exact.iter().any(|x| *x == ip)
    }
}

/// Browser-facing origin for playlists and the dashboard.
/// `X-Forwarded-Host` and `X-Forwarded-Proto` apply only when the socket peer
/// is in `trusted`. An arbitrary client cannot spoof them.
pub fn public_origin(peer: &str, forwarded_host: Option<&str>, forwarded_proto: Option<&str>, configured: &str, trusted: &Trusted) -> String {
    let configured = configured.trim_end_matches('/').to_string();
    let host = peer_host(peer);
    let Ok(ip) = host.parse::<IpAddr>() else {
        return configured;
    };
    if !trusted.contains(ip) {
        return configured;
    }
    let raw = forwarded_host.unwrap_or("").split(',').next().unwrap_or("").trim();
    if !safe_forwarded_host(raw) {
        return configured;
    }
    let proto = forwarded_proto.unwrap_or("").split(',').next().unwrap_or("").trim();
    let proto = if proto == "http" || proto == "https" {
        proto
    } else if configured.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    format!("{proto}://{raw}")
}

fn safe_forwarded_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    if host.chars().any(|c| c.is_control() || matches!(c, ' ' | '/' | '\\' | '@' | '?' | '#')) {
        return false;
    }
    true
}

pub fn client_ip(peer: &str, forwarded: Option<&str>, trusted: &Trusted) -> String {
    let host = peer_host(peer);
    let Ok(ip) = host.parse::<IpAddr>() else {
        return host;
    };
    if !trusted.contains(ip) {
        return host;
    }
    let Some(xff) = forwarded else { return host };
    let first = xff.split(',').next().unwrap_or("").trim();
    if first.is_empty() {
        return host;
    }
    peer_host(first)
}

fn peer_host(peer: &str) -> String {
    if let Some(rest) = peer.strip_prefix('[') {
        if let Some((h, _)) = rest.split_once(']') {
            return h.to_string();
        }
    }
    if let Ok(v4) = peer.parse::<Ipv4Addr>() {
        return v4.to_string();
    }
    if let Ok(v6) = peer.parse::<Ipv6Addr>() {
        return v6.to_string();
    }
    if let Some((h, p)) = peer.rsplit_once(':') {
        if p.chars().all(|c| c.is_ascii_digit()) && !h.contains(':') {
            return h.to_string();
        }
    }
    peer.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarded_only_from_trusted_peer() {
        let t = Trusted::parse(&["127.0.0.1".into()]);
        assert_eq!(client_ip("127.0.0.1:9", Some("203.0.113.5"), &t), "203.0.113.5");
        assert_eq!(client_ip("198.51.100.8:9", Some("203.0.113.5"), &t), "198.51.100.8");
        assert_eq!(client_ip("10.0.0.1:9", None, &t), "10.0.0.1");
    }

    #[test]
    fn untrusted_forwarded_host_is_ignored() {
        let trusted = Trusted::parse(&["10.9.9.9".into()]);
        let configured = "https://radio.example.com";
        assert_eq!(
            public_origin("198.51.100.8:9", Some("evil.example"), Some("http"), configured, &trusted),
            "https://radio.example.com"
        );
        assert_eq!(
            public_origin("10.9.9.9:9", Some("radio.example.com"), Some("https"), configured, &trusted),
            "https://radio.example.com"
        );
        assert_eq!(
            public_origin("10.9.9.9:9", Some("evil.example\r\nHost: x"), Some("https"), configured, &trusted),
            "https://radio.example.com"
        );
    }

    #[test]
    fn ip_cap() {
        let c = IpCounter::new(1);
        assert!(c.acquire("a"));
        assert!(!c.acquire("a"));
        c.release("a");
        assert!(c.acquire("a"));
    }
}
