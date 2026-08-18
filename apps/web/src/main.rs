use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use askama::Template;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
    X_FRAME_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use braindrain_core::format_relative_time;
use braindrain_daemon::{BrainDrainDaemon, CachedProviderState, DaemonStatus, ServiceBackend};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const DEFAULT_LISTEN: &str = "127.0.0.1:8080";
const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const PAGE_REFRESH_SECONDS: u64 = 60;
const STYLE: &str = include_str!("../assets/style.css");

#[derive(Clone)]
struct AppState {
    daemon: BrainDrainDaemon<ServiceBackend>,
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
    allowed_origin: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct PageQuery {
    provider: Option<String>,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    providers: Vec<ProviderNav>,
    selected: Option<ProviderView>,
    refreshing: bool,
    updated: String,
    page_refresh_seconds: u64,
}

struct ProviderNav {
    id: String,
    title: String,
    selected: bool,
    has_error: bool,
}

struct ProviderView {
    id: String,
    title: String,
    plan: Option<String>,
    email: Option<String>,
    error: Option<String>,
    windows: Vec<WindowView>,
    balances: Vec<BalanceView>,
    reset_credits: Vec<ResetCreditView>,
    empty: bool,
}

struct WindowView {
    label: String,
    percent: String,
    percent_value: f64,
    reset: Option<String>,
    reset_exact: Option<String>,
}

struct BalanceView {
    label: String,
    value: String,
}

struct ResetCreditView {
    granted: String,
    expires: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let state = AppState {
        daemon: BrainDrainDaemon::new(ServiceBackend),
        refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
        allowed_origin: std::env::var("BRAINDRAIN_WEB_ORIGIN").ok(),
    };
    spawn_refresh_loop(state.clone());

    let app = app(state);
    if let Some(path) = std::env::var_os("BRAINDRAIN_WEB_UNIX_SOCKET").map(PathBuf::from) {
        return serve_unix(path, app).await;
    }

    let listen = std::env::var("BRAINDRAIN_WEB_LISTEN")
        .unwrap_or_else(|_| DEFAULT_LISTEN.to_owned())
        .parse::<SocketAddr>()
        .context("BRAINDRAIN_WEB_LISTEN must be a socket address")?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("failed to listen on {listen}"))?;
    println!("BrainDrain web listening on http://{listen}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("web server failed")
}

#[cfg(unix)]
async fn serve_unix(path: PathBuf, app: Router) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    remove_stale_socket(&path)?;
    let listener = tokio::net::UnixListener::bind(&path)
        .with_context(|| format!("failed to listen on {}", path.display()))?;
    println!("BrainDrain web listening on unix://{}", path.display());
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("web server failed");
    let _ = std::fs::remove_file(&path);
    result
}

#[cfg(not(unix))]
async fn serve_unix(_path: PathBuf, _app: Router) -> anyhow::Result<()> {
    anyhow::bail!("BRAINDRAIN_WEB_UNIX_SOCKET is only supported on Unix")
}

fn remove_stale_socket(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove stale socket {}", path.display()))
        }
    }
}

fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/refresh", post(refresh_all))
        .route("/style.css", get(style))
        .route("/healthz", get(health))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn index(
    State(state): State<AppState>,
    Query(query): Query<PageQuery>,
) -> Result<Html<String>, StatusCode> {
    let status = state.daemon.status().await;
    let template = page_model(status, query.provider.as_deref());
    template
        .render()
        .map(Html)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn refresh_all(
    State(state): State<AppState>,
    Query(query): Query<PageQuery>,
    headers: HeaderMap,
) -> Response {
    if !origin_allowed(state.allowed_origin.as_deref(), headers.get("origin")) {
        return StatusCode::FORBIDDEN.into_response();
    }
    tokio::spawn(async move { refresh_once(&state).await });
    redirect_to_provider(query.provider.as_deref()).into_response()
}

async fn style() -> impl IntoResponse {
    ([(CONTENT_TYPE, "text/css; charset=utf-8")], STYLE)
}

async fn health() -> &'static str {
    "ok\n"
}

async fn security_headers(request: Request<axum::body::Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'self'; img-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    response
}

fn spawn_refresh_loop(state: AppState) {
    tokio::spawn(async move {
        refresh_once(&state).await;
        let mut interval = tokio::time::interval(REFRESH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            refresh_once(&state).await;
        }
    });
}

async fn refresh_once(state: &AppState) {
    let _guard = state.refresh_lock.lock().await;
    let _ = state.daemon.refresh_all().await;
    for provider in state.daemon.status().await.providers {
        if let Some(error) = provider.error {
            eprintln!(
                "BrainDrain refresh failed for {}: {error}",
                provider.provider
            );
        }
    }
}

fn origin_allowed(expected: Option<&str>, actual: Option<&HeaderValue>) -> bool {
    match expected {
        None => true,
        Some(expected) => actual.and_then(|value| value.to_str().ok()) == Some(expected),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

fn page_model(status: DaemonStatus, requested_provider: Option<&str>) -> IndexTemplate {
    page_model_at(status, requested_provider, OffsetDateTime::now_utc())
}

fn page_model_at(
    status: DaemonStatus,
    requested_provider: Option<&str>,
    now: OffsetDateTime,
) -> IndexTemplate {
    let selected_id = requested_provider
        .filter(|requested| {
            status
                .providers
                .iter()
                .any(|state| state.provider == *requested)
        })
        .map(str::to_owned)
        .or_else(|| status.providers.first().map(|state| state.provider.clone()));

    let providers = status
        .providers
        .iter()
        .map(|state| ProviderNav {
            id: state.provider.clone(),
            title: provider_title(&state.provider).to_owned(),
            selected: selected_id.as_deref() == Some(state.provider.as_str()),
            has_error: state.error.is_some(),
        })
        .collect();

    let selected = selected_id.as_deref().and_then(|id| {
        status
            .providers
            .iter()
            .find(|state| state.provider == id)
            .map(|state| provider_view(state, now))
    });
    let refreshing = status.providers.iter().any(|state| state.refreshing);
    let updated = status
        .providers
        .iter()
        .filter_map(|state| state.last_success_at)
        .max()
        .map(format_updated)
        .unwrap_or_else(|| "Not updated".to_owned());

    IndexTemplate {
        providers,
        selected,
        refreshing,
        updated,
        page_refresh_seconds: PAGE_REFRESH_SECONDS,
    }
}

fn provider_view(state: &CachedProviderState, now: OffsetDateTime) -> ProviderView {
    let snapshot = state.snapshot.as_ref();
    let usage = snapshot.map(|snapshot| &snapshot.usage);
    let windows = usage
        .map(|usage| {
            usage
                .windows
                .iter()
                .map(|window| WindowView {
                    label: window.label.clone(),
                    percent: format!("{:.0}%", window.used_percent),
                    percent_value: window.used_percent.clamp(0.0, 100.0),
                    reset: window
                        .resets_at
                        .map(|timestamp| format_relative_time(timestamp, now)),
                    reset_exact: window.resets_at.map(format_exact),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let balances = usage
        .map(|usage| {
            usage
                .balances
                .iter()
                .map(|balance| BalanceView {
                    label: balance.label.clone(),
                    value: format_number(balance.remaining, &balance.unit),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let reset_credits = usage
        .map(|usage| {
            usage
                .reset_credits
                .iter()
                .map(|credit| ResetCreditView {
                    granted: credit
                        .granted_at
                        .map(format_exact)
                        .unwrap_or_else(|| "Unknown".to_owned()),
                    expires: credit
                        .expires_at
                        .map(format_exact)
                        .unwrap_or_else(|| "Unknown".to_owned()),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let empty = windows.is_empty() && balances.is_empty() && reset_credits.is_empty();

    ProviderView {
        id: state.provider.clone(),
        title: provider_title(&state.provider).to_owned(),
        plan: snapshot
            .and_then(|snapshot| snapshot.identity.as_ref())
            .and_then(|identity| identity.plan.clone()),
        email: snapshot
            .and_then(|snapshot| snapshot.identity.as_ref())
            .and_then(|identity| identity.email.clone()),
        error: state
            .error
            .as_ref()
            .map(|_| "Refresh failed; last good data remains available.".to_owned()),
        windows,
        balances,
        reset_credits,
        empty,
    }
}

fn provider_title(id: &str) -> &str {
    match id {
        "openai" => "OpenAI",
        "claude" => "Claude Code",
        "cursor" => "Cursor",
        "kimi" => "Kimi Code",
        "zai" => "z.ai",
        "opencode-go" => "OpenCode Go",
        other => other,
    }
}

fn redirect_to_provider(provider: Option<&str>) -> Redirect {
    provider
        .filter(|provider| {
            provider
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
        .map(|provider| Redirect::to(&format!("/?provider={provider}")))
        .unwrap_or_else(|| Redirect::to("/"))
}

fn format_updated(timestamp: OffsetDateTime) -> String {
    let format = time::macros::format_description!("[hour]:[minute] UTC");
    timestamp
        .format(format)
        .map(|time| format!("Updated {time}"))
        .unwrap_or_else(|_| "Updated".to_owned())
}

fn format_exact(timestamp: OffsetDateTime) -> String {
    timestamp
        .format(&Rfc3339)
        .unwrap_or_else(|_| "Unknown".to_owned())
}

fn format_number(value: f64, unit: &str) -> String {
    if value.fract().abs() < f64::EPSILON {
        format!("{value:.0} {unit}")
    } else {
        format!("{value:.2} {unit}")
    }
}

#[cfg(test)]
mod tests {
    use braindrain_core::{
        AccountIdentity, BalanceSnapshot, ProviderId, ProviderSnapshot, ProviderSource, RateWindow,
        ResetCreditSnapshot, UsageSnapshot,
    };

    use super::*;

    #[test]
    fn page_mirrors_provider_usage_and_escapes_content() {
        let now = OffsetDateTime::now_utc();
        let status = DaemonStatus {
            version: "test".to_owned(),
            providers: vec![CachedProviderState {
                provider: "zai".to_owned(),
                snapshot: Some(ProviderSnapshot {
                    provider: ProviderId::zai(),
                    source: ProviderSource::Web,
                    usage: UsageSnapshot {
                        windows: vec![RateWindow {
                            id: "weekly".to_owned(),
                            label: "Weekly <quota>".to_owned(),
                            used_percent: 42.4,
                            duration: None,
                            resets_at: Some(now + Duration::from_secs(3_600)),
                        }],
                        balances: vec![BalanceSnapshot {
                            id: "tokens".to_owned(),
                            label: "Tokens".to_owned(),
                            remaining: 12.5,
                            unit: "credits".to_owned(),
                        }],
                        reset_credits: vec![ResetCreditSnapshot {
                            id: "credit".to_owned(),
                            granted_at: Some(now),
                            expires_at: Some(now + Duration::from_secs(86_400)),
                        }],
                    },
                    identity: Some(AccountIdentity {
                        email: Some("user@example.com".to_owned()),
                        plan: Some("Coding Plan".to_owned()),
                    }),
                    updated_at: now,
                }),
                error: Some("credential file /secret/provider.json is missing".to_owned()),
                refreshing: false,
                last_attempt_at: Some(now),
                last_success_at: Some(now),
            }],
        };

        let html = page_model_at(status, Some("zai"), now)
            .render()
            .expect("render page");
        assert!(html.contains("z.ai"));
        assert!(html.contains("Coding Plan"));
        assert!(html.contains("42%"));
        assert!(html.contains("12.50 credits"));
        assert!(html.contains("Weekly"));
        assert!(html.contains("quota"));
        assert!(!html.contains("Weekly <quota>"));
        assert!(html.contains("Resets in 1 hour"));
        assert!(html.contains("Refresh failed; last good data remains available."));
        assert!(!html.contains("/secret/provider.json"));
    }

    #[test]
    fn page_keeps_remaining_hours_on_multi_day_resets() {
        let now = OffsetDateTime::from_unix_timestamp(1_780_704_000).expect("valid now");
        let status = DaemonStatus {
            version: "test".to_owned(),
            providers: vec![CachedProviderState {
                provider: "claude".to_owned(),
                snapshot: Some(ProviderSnapshot {
                    provider: ProviderId::claude(),
                    source: ProviderSource::Cli,
                    usage: UsageSnapshot {
                        windows: vec![RateWindow {
                            id: "weekly".to_owned(),
                            label: "Weekly".to_owned(),
                            used_percent: 10.0,
                            duration: None,
                            resets_at: Some(now + Duration::from_secs(47 * 3_600)),
                        }],
                        balances: Vec::new(),
                        reset_credits: Vec::new(),
                    },
                    identity: None,
                    updated_at: now,
                }),
                error: None,
                refreshing: false,
                last_attempt_at: Some(now),
                last_success_at: Some(now),
            }],
        };

        let html = page_model_at(status, Some("claude"), now)
            .render()
            .expect("render page");
        assert!(html.contains("Resets in 1 day 23 hours"));
        assert!(!html.contains("Resets in 1 days"));
    }

    #[test]
    fn unknown_provider_selection_falls_back_to_first_provider() {
        let status = DaemonStatus {
            version: "test".to_owned(),
            providers: vec![CachedProviderState {
                provider: "openai".to_owned(),
                snapshot: None,
                error: None,
                refreshing: false,
                last_attempt_at: None,
                last_success_at: None,
            }],
        };

        let page = page_model(status, Some("missing"));
        assert_eq!(page.selected.expect("selected provider").id, "openai");
    }

    #[test]
    fn redirect_only_accepts_provider_identifier_characters() {
        let accepted = redirect_to_provider(Some("opencode-go")).into_response();
        assert_eq!(accepted.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            accepted.headers().get("location").expect("location"),
            "/?provider=opencode-go"
        );

        let rejected = redirect_to_provider(Some("https://example.com")).into_response();
        assert_eq!(rejected.status(), StatusCode::SEE_OTHER);
        assert_eq!(rejected.headers().get("location").expect("location"), "/");
    }

    #[test]
    fn configured_origin_is_required_for_refresh() {
        let origin = HeaderValue::from_static("https://braindrain.example.test");
        assert!(origin_allowed(None, None));
        assert!(origin_allowed(
            Some("https://braindrain.example.test"),
            Some(&origin)
        ));
        assert!(!origin_allowed(
            Some("https://braindrain.example.test"),
            None
        ));
        assert!(!origin_allowed(
            Some("https://other.example.test"),
            Some(&origin)
        ));
    }
}
