//! Backend abstraction for the GUI: it talks to the standalone
//! `braindrain-daemon` over D-Bus. The daemon is D-Bus auto-activatable once
//! installed, so attempting to connect will start it on demand; if connection
//! fails the daemon is not installed and the UI should prompt the user to
//! install it.

use std::time::Duration;

use async_trait::async_trait;
use braindrain_daemon::{CachedProviderState, DaemonClient, DaemonStatus};

/// One async interface over the daemon. The UI never needs to branch on mode.
#[allow(dead_code)]
#[async_trait]
pub trait Backend: std::fmt::Debug + Send + Sync {
    async fn provider_ids(&self) -> anyhow::Result<Vec<String>>;
    async fn status(&self) -> anyhow::Result<DaemonStatus>;
    async fn snapshot(&self, provider: &str) -> anyhow::Result<CachedProviderState>;
    async fn all_snapshots(&self) -> anyhow::Result<Vec<CachedProviderState>>;
    async fn refresh(&self, provider: &str) -> anyhow::Result<CachedProviderState>;
    async fn refresh_all(&self) -> anyhow::Result<Vec<CachedProviderState>>;
}

/// Wraps `braindrain_daemon::DaemonClient`; deserializes the JSON `String`
/// replies the daemon sends over D-Bus into the typed structs.
#[derive(Debug)]
pub struct RemoteBackend {
    client: DaemonClient,
}

impl RemoteBackend {
    pub async fn connect() -> anyhow::Result<Self> {
        let client = DaemonClient::connect().await?;
        Ok(Self { client })
    }
}

#[async_trait]
impl Backend for RemoteBackend {
    async fn provider_ids(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.client.list_providers().await?)
    }

    async fn status(&self) -> anyhow::Result<DaemonStatus> {
        let json = self.client.status().await?;
        Ok(serde_json::from_str(&json)?)
    }

    async fn snapshot(&self, provider: &str) -> anyhow::Result<CachedProviderState> {
        let json = self.client.get_snapshot(provider).await?;
        Ok(serde_json::from_str(&json)?)
    }

    async fn all_snapshots(&self) -> anyhow::Result<Vec<CachedProviderState>> {
        let json = self.client.get_all_snapshots().await?;
        Ok(serde_json::from_str(&json)?)
    }

    async fn refresh(&self, provider: &str) -> anyhow::Result<CachedProviderState> {
        let json = self.client.refresh(provider).await?;
        Ok(serde_json::from_str(&json)?)
    }

    async fn refresh_all(&self) -> anyhow::Result<Vec<CachedProviderState>> {
        let json = self.client.refresh_all().await?;
        Ok(serde_json::from_str(&json)?)
    }
}

/// Attempt to connect to the daemon. Returns `Some` on success (the daemon is
/// installed and now running, possibly having just been D-Bus activated), or
/// `None` if the daemon is not installed.
///
/// `DaemonClient::connect()` only builds a `zbus::Proxy` — it doesn't verify
/// that the bus name is actually owned or activatable. That only surfaces on
/// the first method call, so we probe with `status()` here to decide whether
/// to treat the daemon as reachable. D-Bus activation is not instant (the
/// daemon process needs a moment to start and claim the bus name), so the
/// probe is retried a few times before giving up.
pub async fn resolve() -> Option<Box<dyn Backend>> {
    let backend = match RemoteBackend::connect().await {
        Ok(backend) => backend,
        Err(error) => {
            log::info!("daemon connect failed, treating as not installed: {error:?}");
            return None;
        }
    };

    for attempt in 1..=5 {
        match backend.status().await {
            Ok(_) => return Some(Box::new(backend)),
            Err(error) => {
                log::info!("daemon not reachable (attempt {attempt}): {error:?}");
                if attempt < 5 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
    None
}
