//! Unified backend abstraction: the GUI talks to either a running
//! `braindrain-daemon` over D-Bus (preferred) or an in-process embedded
//! daemon (fallback) through a single async trait.

use async_trait::async_trait;
use braindrain_daemon::{
    BrainDrainDaemon, CachedProviderState, DaemonClient, DaemonStatus, ServiceBackend,
};
/// Which backend is actually serving requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendMode {
    /// Talking to the standalone `braindrain-daemon` over the session bus.
    Remote,
    /// Running an in-process `BrainDrainDaemon` (no D-Bus involved).
    Embedded,
}

/// One async interface for both backend variants. The UI never branches on
/// mode for data access — only for status display.
#[allow(dead_code)]
#[async_trait]
pub trait Backend: std::fmt::Debug + Send + Sync {
    fn mode(&self) -> BackendMode;
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
    fn mode(&self) -> BackendMode {
        BackendMode::Remote
    }

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

/// Wraps an in-process `BrainDrainDaemon<ServiceBackend>`, exposing the same
/// trait surface. The daemon struct already returns typed values, so no JSON
/// round-trip is needed.
#[derive(Debug)]
pub struct EmbeddedBackend {
    daemon: BrainDrainDaemon<ServiceBackend>,
}

impl EmbeddedBackend {
    pub fn new() -> Self {
        Self {
            daemon: BrainDrainDaemon::new(ServiceBackend),
        }
    }
}

impl Default for EmbeddedBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Backend for EmbeddedBackend {
    fn mode(&self) -> BackendMode {
        BackendMode::Embedded
    }

    async fn provider_ids(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.daemon.provider_ids())
    }

    async fn status(&self) -> anyhow::Result<DaemonStatus> {
        Ok(self.daemon.status().await)
    }

    async fn snapshot(&self, provider: &str) -> anyhow::Result<CachedProviderState> {
        Ok(self.daemon.provider_state(provider).await?)
    }

    async fn all_snapshots(&self) -> anyhow::Result<Vec<CachedProviderState>> {
        Ok(self.daemon.all_states().await)
    }

    async fn refresh(&self, provider: &str) -> anyhow::Result<CachedProviderState> {
        Ok(self.daemon.refresh_provider(provider).await?)
    }

    async fn refresh_all(&self) -> anyhow::Result<Vec<CachedProviderState>> {
        let results = self.daemon.refresh_all().await;
        let states = results.into_iter().collect::<Result<Vec<_>, _>>()?;
        Ok(states)
    }
}

/// Resolve a backend at startup: prefer the running standalone daemon, fall
/// back to an embedded in-process daemon if the session-bus connect fails.
pub async fn resolve() -> (Box<dyn Backend>, BackendMode) {
    match RemoteBackend::connect().await {
        Ok(backend) => (Box::new(backend), BackendMode::Remote),
        Err(_) => {
            let embedded = EmbeddedBackend::new();
            let mode = embedded.mode();
            (Box::new(embedded), mode)
        }
    }
}
