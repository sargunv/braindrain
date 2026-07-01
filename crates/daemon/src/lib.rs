use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use braindrain_core::{ProviderError, ProviderId, ProviderSnapshot};
use braindrain_service as service;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::{Mutex, RwLock};
use zbus::object_server::SignalEmitter;

pub const BUS_NAME: &str = "dev.sargunv.BrainDrain1";
pub const OBJECT_PATH: &str = "/dev/sargunv/BrainDrain1";
pub const INTERFACE_NAME: &str = "dev.sargunv.BrainDrain1";
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

pub type RefreshFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProviderSnapshot, ProviderError>> + Send + 'a>>;

pub trait RefreshBackend: Send + Sync + 'static {
    fn provider_ids(&self) -> Vec<ProviderId>;
    fn refresh_provider<'a>(&'a self, provider: &'a str) -> RefreshFuture<'a>;
}

#[derive(Debug, Clone, Default)]
pub struct ServiceBackend;

impl RefreshBackend for ServiceBackend {
    fn provider_ids(&self) -> Vec<ProviderId> {
        service::provider_ids()
    }

    fn refresh_provider<'a>(&'a self, provider: &'a str) -> RefreshFuture<'a> {
        Box::pin(async move {
            service::check_provider(provider)
                .await
                .map_err(provider_error_from_service)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedProviderState {
    pub provider: String,
    pub snapshot: Option<ProviderSnapshot>,
    pub error: Option<String>,
    pub refreshing: bool,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_attempt_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_success_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub providers: Vec<CachedProviderState>,
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("unsupported provider: {0}")]
    UnsupportedProvider(String),
    #[error("could not serialize daemon response: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("could not emit daemon signal: {0}")]
    Signal(String),
}

#[derive(Debug)]
struct Inner<B> {
    backend: B,
    provider_order: Vec<String>,
    states: RwLock<HashMap<String, CachedProviderState>>,
    in_flight: Mutex<HashSet<String>>,
}

#[derive(Debug)]
pub struct BrainDrainDaemon<B = ServiceBackend> {
    inner: Arc<Inner<B>>,
}

impl<B> Clone for BrainDrainDaemon<B> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<B: RefreshBackend> BrainDrainDaemon<B> {
    pub fn new(backend: B) -> Self {
        let provider_order = backend
            .provider_ids()
            .into_iter()
            .map(|provider| provider.as_str().to_owned())
            .collect::<Vec<_>>();
        let states = provider_order
            .iter()
            .map(|provider| (provider.clone(), empty_provider_state(provider)))
            .collect();

        Self {
            inner: Arc::new(Inner {
                backend,
                provider_order,
                states: RwLock::new(states),
                in_flight: Mutex::new(HashSet::new()),
            }),
        }
    }

    pub fn provider_ids(&self) -> Vec<String> {
        self.inner.provider_order.clone()
    }

    pub async fn status(&self) -> DaemonStatus {
        DaemonStatus {
            version: DAEMON_VERSION.to_owned(),
            providers: self.all_states().await,
        }
    }

    pub async fn provider_state(&self, provider: &str) -> Result<CachedProviderState, DaemonError> {
        let provider = self.canonical_provider(provider)?;
        let states = self.inner.states.read().await;
        states
            .get(&provider)
            .cloned()
            .ok_or(DaemonError::UnsupportedProvider(provider))
    }

    pub async fn all_states(&self) -> Vec<CachedProviderState> {
        let states = self.inner.states.read().await;
        self.inner
            .provider_order
            .iter()
            .filter_map(|provider| states.get(provider).cloned())
            .collect()
    }

    pub async fn refresh_provider(
        &self,
        provider: &str,
    ) -> Result<CachedProviderState, DaemonError> {
        let provider = self.canonical_provider(provider)?;
        let started = self.begin_refresh(&provider).await;
        if !started {
            return self.provider_state(&provider).await;
        }

        let result = self.inner.backend.refresh_provider(&provider).await;
        let state = self.finish_refresh(&provider, result).await;
        self.inner.in_flight.lock().await.remove(&provider);
        Ok(state)
    }

    pub async fn refresh_all(&self) -> Vec<Result<CachedProviderState, DaemonError>> {
        let mut states = Vec::with_capacity(self.inner.provider_order.len());
        for provider in &self.inner.provider_order {
            states.push(self.refresh_provider(provider).await);
        }
        states
    }

    pub async fn status_json(&self) -> Result<String, DaemonError> {
        Ok(serde_json::to_string(&self.status().await)?)
    }

    pub async fn provider_state_json(&self, provider: &str) -> Result<String, DaemonError> {
        Ok(serde_json::to_string(
            &self.provider_state(provider).await?,
        )?)
    }

    pub async fn all_states_json(&self) -> Result<String, DaemonError> {
        Ok(serde_json::to_string(&self.all_states().await)?)
    }

    pub async fn refresh_provider_json(&self, provider: &str) -> Result<String, DaemonError> {
        Ok(serde_json::to_string(
            &self.refresh_provider(provider).await?,
        )?)
    }

    pub async fn refresh_all_json(&self) -> Result<String, DaemonError> {
        let states = self
            .refresh_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        Ok(serde_json::to_string(&states)?)
    }

    async fn begin_refresh(&self, provider: &str) -> bool {
        let mut in_flight = self.inner.in_flight.lock().await;
        if !in_flight.insert(provider.to_owned()) {
            return false;
        }

        let mut states = self.inner.states.write().await;
        let state = states
            .entry(provider.to_owned())
            .or_insert_with(|| empty_provider_state(provider));
        state.refreshing = true;
        state.last_attempt_at = Some(OffsetDateTime::now_utc());

        true
    }

    async fn finish_refresh(
        &self,
        provider: &str,
        result: Result<ProviderSnapshot, ProviderError>,
    ) -> CachedProviderState {
        let mut states = self.inner.states.write().await;
        let state = states
            .entry(provider.to_owned())
            .or_insert_with(|| empty_provider_state(provider));
        state.refreshing = false;

        match result {
            Ok(snapshot) => {
                state.snapshot = Some(snapshot);
                state.error = None;
                state.last_success_at = Some(OffsetDateTime::now_utc());
            }
            Err(error) => {
                state.error = Some(error.to_string());
            }
        }

        state.clone()
    }

    fn canonical_provider(&self, provider: &str) -> Result<String, DaemonError> {
        let provider = service::normalize_provider_id(provider);
        let provider = provider.as_str();
        self.inner
            .provider_order
            .iter()
            .find(|known| known.as_str() == provider)
            .cloned()
            .ok_or_else(|| DaemonError::UnsupportedProvider(provider.to_owned()))
    }
}

#[derive(Debug, Clone)]
pub struct BrainDrainDbus<B = ServiceBackend> {
    daemon: BrainDrainDaemon<B>,
}

impl<B> BrainDrainDbus<B> {
    pub fn new(daemon: BrainDrainDaemon<B>) -> Self {
        Self { daemon }
    }
}

#[zbus::interface(name = "dev.sargunv.BrainDrain1")]
impl<B: RefreshBackend> BrainDrainDbus<B> {
    async fn version(&self) -> String {
        DAEMON_VERSION.to_owned()
    }

    async fn list_providers(&self) -> Vec<String> {
        self.daemon.provider_ids()
    }

    async fn status(&self) -> zbus::fdo::Result<String> {
        self.daemon.status_json().await.map_err(dbus_error)
    }

    async fn get_snapshot(&self, provider: String) -> zbus::fdo::Result<String> {
        self.daemon
            .provider_state_json(&provider)
            .await
            .map_err(dbus_error)
    }

    async fn get_all_snapshots(&self) -> zbus::fdo::Result<String> {
        self.daemon.all_states_json().await.map_err(dbus_error)
    }

    async fn refresh(
        &self,
        provider: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<String> {
        let provider = self
            .daemon
            .provider_state(&provider)
            .await
            .map_err(dbus_error)?
            .provider;
        emit_provider_refresh_started(&emitter, &provider).await?;
        let state = self
            .daemon
            .refresh_provider(&provider)
            .await
            .map_err(dbus_error)?;
        emit_finished_signals(&emitter, &state).await?;
        serde_json::to_string(&state).map_err(dbus_error)
    }

    async fn refresh_all(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<String> {
        refresh_all_with_signals(&self.daemon, &emitter)
            .await
            .map_err(dbus_error)
    }

    #[zbus(signal)]
    async fn snapshot_changed(
        signal_emitter: &SignalEmitter<'_>,
        provider: &str,
        state_json: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn provider_refresh_started(
        signal_emitter: &SignalEmitter<'_>,
        provider: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn provider_refresh_finished(
        signal_emitter: &SignalEmitter<'_>,
        provider: &str,
        state_json: &str,
    ) -> zbus::Result<()>;
}

pub async fn run_service() -> anyhow::Result<()> {
    let daemon = BrainDrainDaemon::new(ServiceBackend);
    let interface = BrainDrainDbus::new(daemon.clone());
    let connection = zbus::connection::Builder::session()?
        .serve_at(OBJECT_PATH, interface)?
        .name(BUS_NAME)?
        .build()
        .await?;
    let emitter = SignalEmitter::new(&connection, OBJECT_PATH)?;

    tokio::spawn(run_refresh_loop(
        daemon,
        emitter.to_owned(),
        DEFAULT_REFRESH_INTERVAL,
    ));

    std::future::pending::<()>().await;
    Ok(())
}

async fn run_refresh_loop<B: RefreshBackend>(
    daemon: BrainDrainDaemon<B>,
    emitter: SignalEmitter<'static>,
    refresh_interval: Duration,
) {
    if let Err(error) = refresh_all_with_signals(&daemon, &emitter).await {
        eprintln!("initial refresh failed: {error}");
    }

    loop {
        tokio::time::sleep(refresh_interval).await;
        if let Err(error) = refresh_all_with_signals(&daemon, &emitter).await {
            eprintln!("periodic refresh failed: {error}");
        }
    }
}

async fn refresh_all_with_signals<B: RefreshBackend>(
    daemon: &BrainDrainDaemon<B>,
    emitter: &SignalEmitter<'_>,
) -> Result<String, DaemonError> {
    let mut states = Vec::new();
    for provider in daemon.provider_ids() {
        emit_provider_refresh_started(emitter, &provider)
            .await
            .map_err(dbus_error_to_daemon)?;
        let state = daemon.refresh_provider(&provider).await?;
        emit_finished_signals(emitter, &state)
            .await
            .map_err(dbus_error_to_daemon)?;
        states.push(state);
    }

    Ok(serde_json::to_string(&states)?)
}

async fn emit_finished_signals(
    emitter: &SignalEmitter<'_>,
    state: &CachedProviderState,
) -> zbus::fdo::Result<()> {
    let state_json = serde_json::to_string(state).map_err(dbus_error)?;
    emit_provider_refresh_finished(emitter, &state.provider, &state_json).await?;

    if state.error.is_none() && state.snapshot.is_some() {
        emit_snapshot_changed(emitter, &state.provider, &state_json).await?;
    }

    Ok(())
}

async fn emit_snapshot_changed(
    emitter: &SignalEmitter<'_>,
    provider: &str,
    state_json: &str,
) -> zbus::fdo::Result<()> {
    emitter
        .emit(INTERFACE_NAME, "SnapshotChanged", &(provider, state_json))
        .await
        .map_err(dbus_error)
}

async fn emit_provider_refresh_started(
    emitter: &SignalEmitter<'_>,
    provider: &str,
) -> zbus::fdo::Result<()> {
    emitter
        .emit(INTERFACE_NAME, "ProviderRefreshStarted", &(provider,))
        .await
        .map_err(dbus_error)
}

async fn emit_provider_refresh_finished(
    emitter: &SignalEmitter<'_>,
    provider: &str,
    state_json: &str,
) -> zbus::fdo::Result<()> {
    emitter
        .emit(
            INTERFACE_NAME,
            "ProviderRefreshFinished",
            &(provider, state_json),
        )
        .await
        .map_err(dbus_error)
}

#[derive(Debug)]
pub struct DaemonClient {
    proxy: zbus::Proxy<'static>,
}

impl DaemonClient {
    pub async fn connect() -> zbus::Result<Self> {
        let connection = zbus::Connection::session().await?;
        let proxy =
            zbus::Proxy::new_owned(connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME).await?;
        Ok(Self { proxy })
    }

    pub async fn version(&self) -> zbus::Result<String> {
        self.proxy.call("Version", &()).await
    }

    pub async fn list_providers(&self) -> zbus::Result<Vec<String>> {
        self.proxy.call("ListProviders", &()).await
    }

    pub async fn status(&self) -> zbus::Result<String> {
        self.proxy.call("Status", &()).await
    }

    pub async fn get_snapshot(&self, provider: &str) -> zbus::Result<String> {
        self.proxy.call("GetSnapshot", &(provider,)).await
    }

    pub async fn get_all_snapshots(&self) -> zbus::Result<String> {
        self.proxy.call("GetAllSnapshots", &()).await
    }

    pub async fn refresh(&self, provider: &str) -> zbus::Result<String> {
        self.proxy.call("Refresh", &(provider,)).await
    }

    pub async fn refresh_all(&self) -> zbus::Result<String> {
        self.proxy.call("RefreshAll", &()).await
    }
}

fn empty_provider_state(provider: &str) -> CachedProviderState {
    CachedProviderState {
        provider: provider.to_owned(),
        snapshot: None,
        error: None,
        refreshing: false,
        last_attempt_at: None,
        last_success_at: None,
    }
}

fn provider_error_from_service(error: service::ServiceError) -> ProviderError {
    match error {
        service::ServiceError::Provider(error) => error,
        service::ServiceError::UnsupportedProvider { provider } => ProviderError::Unsupported(
            service::ServiceError::UnsupportedProvider { provider }.to_string(),
        ),
        service::ServiceError::Credential(message) => ProviderError::Unsupported(message),
    }
}

fn dbus_error(error: impl std::fmt::Display) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(error.to_string())
}

fn dbus_error_to_daemon(error: zbus::fdo::Error) -> DaemonError {
    DaemonError::Signal(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use braindrain_core::{ProviderSource, RefreshContext, UsageSnapshot};

    use super::*;

    #[derive(Debug, Clone)]
    struct MockBackend {
        results: Arc<Mutex<Vec<Result<ProviderSnapshot, ProviderError>>>>,
        calls: Arc<AtomicUsize>,
        delay: Duration,
    }

    impl MockBackend {
        fn new(results: Vec<Result<ProviderSnapshot, ProviderError>>) -> Self {
            Self {
                results: Arc::new(Mutex::new(results)),
                calls: Arc::new(AtomicUsize::new(0)),
                delay: Duration::from_millis(0),
            }
        }

        fn with_delay(mut self, delay: Duration) -> Self {
            self.delay = delay;
            self
        }
    }

    impl RefreshBackend for MockBackend {
        fn provider_ids(&self) -> Vec<ProviderId> {
            vec![ProviderId::openai()]
        }

        fn refresh_provider<'a>(&'a self, _provider: &'a str) -> RefreshFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(self.delay).await;
                self.results.lock().await.remove(0)
            })
        }
    }

    #[tokio::test]
    async fn successful_refresh_stores_snapshot_and_clears_error() {
        let snapshot = test_snapshot();
        let backend = MockBackend::new(vec![Ok(snapshot.clone())]);
        let daemon = BrainDrainDaemon::new(backend);

        let state = daemon
            .refresh_provider("openai")
            .await
            .expect("refresh succeeds");

        assert_eq!(state.snapshot, Some(snapshot));
        assert_eq!(state.error, None);
        assert!(!state.refreshing);
        assert!(state.last_attempt_at.is_some());
        assert!(state.last_success_at.is_some());
    }

    #[tokio::test]
    async fn failed_refresh_preserves_last_successful_snapshot() {
        let snapshot = test_snapshot();
        let backend = MockBackend::new(vec![
            Ok(snapshot.clone()),
            Err(ProviderError::Network("offline".to_owned())),
        ]);
        let daemon = BrainDrainDaemon::new(backend);

        daemon
            .refresh_provider("openai")
            .await
            .expect("first refresh succeeds");
        let state = daemon
            .refresh_provider("openai")
            .await
            .expect("failed provider response still updates state");

        assert_eq!(state.snapshot, Some(snapshot));
        assert_eq!(state.error, Some("network error: offline".to_owned()));
        assert!(!state.refreshing);
    }

    #[tokio::test]
    async fn duplicate_in_flight_refresh_coalesces() {
        let snapshot = test_snapshot();
        let backend = MockBackend::new(vec![Ok(snapshot)]).with_delay(Duration::from_millis(100));
        let calls = Arc::clone(&backend.calls);
        let daemon = BrainDrainDaemon::new(backend);

        let first = tokio::spawn({
            let daemon = daemon.clone();
            async move { daemon.refresh_provider("openai").await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        let second = daemon
            .refresh_provider("openai")
            .await
            .expect("coalesced refresh state");
        let first = first
            .await
            .expect("first task joins")
            .expect("first refresh succeeds");

        assert!(second.refreshing);
        assert_eq!(first.error, None);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    fn test_snapshot() -> ProviderSnapshot {
        ProviderSnapshot {
            provider: ProviderId::openai(),
            source: ProviderSource::Cli,
            usage: UsageSnapshot::empty(),
            identity: None,
            updated_at: RefreshContext::default().now,
        }
    }
}
