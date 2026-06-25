use async_trait::async_trait;
use std::collections::HashSet;
use std::time::Duration;

/// Probes a single URL and reports whether it is healthy (2xx within timeout).
#[async_trait]
pub trait HealthChecker: Send + Sync {
    async fn check(&self, url: &str, timeout: Duration) -> bool;
}

/// Extract the bare host from a URL: strip scheme, path, and port.
/// `http://10.0.0.2:3010/health` -> `10.0.0.2`.
pub fn url_host(url: &str) -> String {
    let mut s = url;
    if let Some(idx) = s.find("://") {
        s = &s[idx + 3..];
    }
    // drop everything from the first path / query / fragment separator
    if let Some(idx) = s.find(['/', '?', '#']) {
        s = &s[..idx];
    }
    // drop userinfo
    if let Some(idx) = s.rfind('@') {
        s = &s[idx + 1..];
    }
    // drop port, but keep bracketed IPv6 literals intact
    if s.starts_with('[') {
        if let Some(end) = s.find(']') {
            return s[1..end].to_string();
        }
    }
    if let Some(idx) = s.rfind(':') {
        s = &s[..idx];
    }
    s.to_string()
}

/// Drop any peer URL that points back at this host, so identical peer lists can
/// be deployed to every node without a node counting itself toward quorum.
pub fn filter_self<'a>(peers: &'a [String], self_hosts: &HashSet<String>) -> Vec<&'a String> {
    peers
        .iter()
        .filter(|url| {
            let host = url_host(url);
            !host.is_empty() && !self_hosts.contains(&host)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuorumResult {
    pub healthy: usize,
    pub total: usize,
    pub required: usize,
}

impl QuorumResult {
    pub fn met(&self) -> bool {
        self.healthy >= self.required
    }
}

/// Run the peer quorum check. Self-referential peers are filtered first; if no
/// peers remain (or none configured), the quorum is trivially satisfied — this
/// mirrors sync.sh, where an empty/all-self peer list skips the gate.
pub async fn check_peer_quorum(
    checker: &dyn HealthChecker,
    peers: &[String],
    self_hosts: &HashSet<String>,
    required: usize,
    timeout: Duration,
) -> QuorumResult {
    let filtered = filter_self(peers, self_hosts);
    if filtered.is_empty() {
        return QuorumResult {
            healthy: 0,
            total: 0,
            required: 0,
        };
    }

    let mut healthy = 0;
    for url in &filtered {
        if checker.check(url, timeout).await {
            healthy += 1;
        }
    }
    QuorumResult {
        healthy,
        total: filtered.len(),
        required,
    }
}

#[cfg(any(test, feature = "testkit"))]
pub use fake::FakeHealthChecker;

#[cfg(any(test, feature = "testkit"))]
mod fake {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Health checker driven by a fixed url->healthy map; unknown urls are down.
    #[derive(Default)]
    pub struct FakeHealthChecker {
        states: HashMap<String, bool>,
        probed: Mutex<Vec<String>>,
    }

    impl FakeHealthChecker {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn up(mut self, url: &str) -> Self {
            self.states.insert(url.to_string(), true);
            self
        }

        pub fn down(mut self, url: &str) -> Self {
            self.states.insert(url.to_string(), false);
            self
        }

        pub fn probed(&self) -> Vec<String> {
            self.probed.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl HealthChecker for FakeHealthChecker {
        async fn check(&self, url: &str, _timeout: Duration) -> bool {
            self.probed.lock().unwrap().push(url.to_string());
            self.states.get(url).copied().unwrap_or(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn host_extraction() {
        assert_eq!(url_host("http://10.0.0.2:3010/"), "10.0.0.2");
        assert_eq!(url_host("https://example.com/health"), "example.com");
        assert_eq!(url_host("http://127.0.0.1:3010"), "127.0.0.1");
        assert_eq!(url_host("http://[::1]:3010/"), "::1");
        assert_eq!(url_host("http://user@host:80/x"), "host");
    }

    #[test]
    fn filters_self_out() {
        let peers = vec![
            "http://10.0.0.2:3010/".to_string(),
            "http://10.0.0.3:3010/".to_string(),
            "http://127.0.0.1:3010/".to_string(),
        ];
        let self_hosts = set(&["127.0.0.1", "10.0.0.3", "localhost"]);
        let kept = filter_self(&peers, &self_hosts);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0], "http://10.0.0.2:3010/");
    }

    #[tokio::test]
    async fn quorum_met() {
        let checker = FakeHealthChecker::new()
            .up("http://10.0.0.2:3010/")
            .up("http://10.0.0.3:3010/");
        let peers = vec![
            "http://10.0.0.2:3010/".to_string(),
            "http://10.0.0.3:3010/".to_string(),
        ];
        let res = check_peer_quorum(&checker, &peers, &set(&[]), 2, Duration::from_secs(1)).await;
        assert!(res.met());
        assert_eq!(res.healthy, 2);
    }

    #[tokio::test]
    async fn quorum_not_met() {
        let checker = FakeHealthChecker::new()
            .up("http://10.0.0.2:3010/")
            .down("http://10.0.0.3:3010/");
        let peers = vec![
            "http://10.0.0.2:3010/".to_string(),
            "http://10.0.0.3:3010/".to_string(),
        ];
        let res = check_peer_quorum(&checker, &peers, &set(&[]), 2, Duration::from_secs(1)).await;
        assert!(!res.met());
        assert_eq!(res.healthy, 1);
    }

    #[tokio::test]
    async fn all_self_skips_gate() {
        let checker = FakeHealthChecker::new();
        let peers = vec!["http://127.0.0.1:3010/".to_string()];
        let res = check_peer_quorum(
            &checker,
            &peers,
            &set(&["127.0.0.1"]),
            2,
            Duration::from_secs(1),
        )
        .await;
        // No non-self peers remain -> trivially met.
        assert!(res.met());
        assert_eq!(res.total, 0);
    }
}
