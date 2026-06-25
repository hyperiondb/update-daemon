use async_trait::async_trait;
use renew_core::HealthChecker;
use std::time::Duration;

/// Health checker backed by reqwest: a URL is healthy if it returns a 2xx within
/// the timeout. Any transport error or non-2xx counts as unhealthy.
pub struct HttpHealthChecker {
    client: reqwest::Client,
}

impl HttpHealthChecker {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for HttpHealthChecker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HealthChecker for HttpHealthChecker {
    async fn check(&self, url: &str, timeout: Duration) -> bool {
        match self.client.get(url).timeout(timeout).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(e) => {
                tracing::warn!(url, error = %e, "health probe failed");
                false
            }
        }
    }
}
