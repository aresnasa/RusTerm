//! Loopback listener for the Feishu OAuth callback (issue #129).
//!
//! Feishu's OAuth redirect allowlist only matches a *fixed* redirect URI (no
//! port patterns), so each QR popup pins the exact listener port while
//! binding. To keep the UI resilient when ports are occupied, [`bind_listener`]
//! scans [`FIRST_PORT`]..=[`LAST_PORT`] and returns the port it actually
//! bound; the authorize URL is then built against that port. Realistically
//! the first port is free — Feishu only ever needs the single allowlist entry
//! `http://127.0.0.1:8878/oauth/feishu/callback`.
//!
//! The listener is deliberately tiny: it parses `code` / `state` / `error`
//! from the one callback route, pushes a [`FeishuOAuthEvent`]-shaped delivery
//! through a callback trait, and answers with a friendly \"you can close this
//! page\" response. All crypto (PKCE exchange) and storage happen in the UI
//! process, never here.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::get;
use serde::Deserialize;
use tokio::net::TcpListener;

/// Preferred OAuth callback port (Feishu redirect-allowlist entry).
pub const FIRST_PORT: u16 = 8878;
/// Last port tried before giving up (8878..=8888).
pub const LAST_PORT: u16 = 8888;
/// Callback route path, shared by the router and the redirect URI builder.
pub const CALLBACK_PATH: &str = "/oauth/feishu/callback";

/// Delivery for one OAuth callback, as seen by the app event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeishuOAuthCallback {
    pub state: String,
    pub result: Result<String, String>,
}

/// Sink for completed OAuth callbacks. The UI installs the implementation;
/// the listener never touches Dioxus signals directly.
pub trait OAuthSink: Send + Sync {
    fn deliver(&self, cb: FeishuOAuthCallback);
}

/// Outcome of a bind attempt, reported through the handshake channel.
type BindReport = Result<(u16, tokio::sync::oneshot::Sender<()>), String>;

/// The bound listener handle. Kept alive by the caller for the app's
/// lifetime; dropping it aborts the accept loop.
pub struct ListenerHandle {
    pub port: u16,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl ListenerHandle {
    /// The redirect URI Feishu must call; exactly what the Feishu app's
    /// redirect allowlist must contain.
    pub fn redirect_uri(&self) -> String {
        redirect_uri(self.port)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stop accepting connections. Fire-and-forget like `stop_relay`.
    pub fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Build the redirect URI for a concrete port.
pub fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}{CALLBACK_PATH}")
}

#[derive(Debug, Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

struct ListenerState {
    sink: Arc<dyn OAuthSink>,
}

const SUCCESS_PAGE: &str = r#"<!doctype html>
<html lang="zh"><head><meta charset="utf-8"><title>RusTerm</title></head>
<body style="font-family:system-ui,sans-serif;background:#1a1b26;color:#c0caf5;display:flex;align-items:center;justify-content:center;min-height:90vh;margin:0;">
<div style="text-align:center;">
<h1 style="font-size:20px;">授权成功</h1>
<p>飞书扫码登录已完成，请回到 RusTerm。本页面现在可以关闭。</p>
<p style="font-size:12px;color:#565f89;">Feishu sign-in complete — you may close this page.</p>
</div></body></html>"#;

const FAILURE_CSS: &str = "font-family:system-ui,sans-serif;background:#1a1b26;color:#f7768e;display:flex;align-items:center;justify-content:center;min-height:90vh;margin:0;";

fn failure_page(reason: &str) -> String {
    // `reason` originates from Feishu's `error_description` or from our own
    // fixed strings; HTML-escape before embedding.
    let escaped = reason
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    format!(
        r#"<!doctype html>
<html lang="zh"><head><meta charset="utf-8"><title>RusTerm</title></head>
<body style="{FAILURE_CSS}">
<div style="text-align:center;">
<h1 style="font-size:20px;">授权失败</h1>
<p>{escaped}</p>
<p style="font-size:12px;color:#565f89;">Feishu authorization failed.</p>
</div></body></html>"#
    )
}

async fn oauth_callback(
    State(listener): State<Arc<ListenerState>>,
    Query(params): Query<CallbackParams>,
) -> (StatusCode, Html<String>) {
    let Some(state) = params.state.filter(|s| !s.trim().is_empty()) else {
        tracing::warn!("[OTP-FEISHU] callback missing state parameter; dropped");
        return (
            StatusCode::BAD_REQUEST,
            Html(failure_page("missing OAuth state")),
        );
    };
    let result = if let Some(err) = params.error {
        let desc = params
            .error_description
            .filter(|d| !d.trim().is_empty())
            .unwrap_or(err);
        Err(desc)
    } else if let Some(code) = params.code.filter(|c| !c.trim().is_empty()) {
        Ok(code)
    } else {
        Err("missing authorization code".to_string())
    };
    let ok = result.is_ok();
    tracing::info!(
        "[OTP-FEISHU] OAuth callback received (state len={}, ok={})",
        state.len(),
        ok
    );
    listener.sink.deliver(FeishuOAuthCallback { state, result });
    if ok {
        (StatusCode::OK, Html(SUCCESS_PAGE.to_string()))
    } else {
        (
            StatusCode::OK,
            Html(failure_page("authorization was not completed")),
        )
    }
}

/// Bind the OAuth listener on the first free loopback port and spawn the
/// accept loop onto `runtime`. Synchronous like `start_relay`: the bind is
/// effectively instant, and the handshake channel makes a wedged runtime
/// fail within 10s instead of freezing the UI thread.
pub fn bind_listener(
    sink: Arc<dyn OAuthSink>,
    runtime: &tokio::runtime::Handle,
) -> Result<ListenerHandle, String> {
    let state = Arc::new(ListenerState { sink });
    let (result_tx, result_rx) = std::sync::mpsc::channel::<BindReport>();
    runtime.spawn(async move {
        let mut bound = None;
        let mut last_err = String::from("no ports tried");
        for port in FIRST_PORT..=LAST_PORT {
            match TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await {
                Ok(l) => {
                    bound = Some((port, l));
                    break;
                }
                Err(e) => last_err = format!("127.0.0.1:{port}: {e}"),
            }
        }
        let Some((port, listener)) = bound else {
            let _ = result_tx.send(Err(format!(
                "no free loopback port for the Feishu OAuth listener in \
                 {FIRST_PORT}..={LAST_PORT} (last error: {last_err})"
            )));
            return;
        };
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        if result_tx.send(Ok((port, shutdown_tx))).is_err() {
            return;
        }
        let app = Router::new()
            .route(CALLBACK_PATH, get(oauth_callback))
            .with_state(state);
        tracing::info!("[OTP-FEISHU] OAuth listener bound on 127.0.0.1:{port}");
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = (&mut shutdown_rx).await;
            })
            .await;
    });
    match result_rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(Ok((port, shutdown))) => Ok(ListenerHandle {
            port,
            shutdown: Some(shutdown),
        }),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(format!("OAuth listener start timeout: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct VecSink(std::sync::Mutex<Vec<FeishuOAuthCallback>>);
    impl OAuthSink for VecSink {
        fn deliver(&self, cb: FeishuOAuthCallback) {
            self.0.lock().unwrap().push(cb);
        }
    }

    #[test]
    fn redirect_uri_matches_allowlist_format() {
        assert_eq!(
            redirect_uri(8878),
            "http://127.0.0.1:8878/oauth/feishu/callback"
        );
        assert_eq!(
            ListenerHandle {
                port: 8878,
                shutdown: None,
            }
            .redirect_uri(),
            "http://127.0.0.1:8878/oauth/feishu/callback"
        );
    }

    #[test]
    fn failure_page_escapes_html() {
        let page = failure_page("<script>alert(1)</script>&\"x\"");
        assert!(!page.contains("<script>"));
        assert!(page.contains("&lt;script&gt;"));
        assert!(page.contains("&amp;"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn listener_binds_and_delivers_callback() {
        let sink = Arc::new(VecSink(std::sync::Mutex::new(Vec::new())));
        let handle = bind_listener(sink.clone(), &tokio::runtime::Handle::current())
            .expect("listener binds on a loopback port");
        assert!((FIRST_PORT..=LAST_PORT).contains(&handle.port()));
        let url = format!(
            "http://127.0.0.1:{}{}?code=test-code&state=test-state",
            handle.port(),
            CALLBACK_PATH
        );
        let resp = reqwest::get(&url).await.expect("callback request succeeds");
        assert!(resp.status().is_success());
        let body = resp.text().await.unwrap();
        assert!(body.contains("授权成功"), "success HTML missing: {body}");
        let delivered = sink.0.lock().unwrap().clone();
        assert_eq!(
            delivered,
            vec![FeishuOAuthCallback {
                state: "test-state".into(),
                result: Ok("test-code".into()),
            }]
        );
        handle.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn listener_reports_oauth_error() {
        let sink = Arc::new(VecSink(std::sync::Mutex::new(Vec::new())));
        let handle = bind_listener(sink.clone(), &tokio::runtime::Handle::current())
            .expect("listener binds");
        let url = format!(
            "http://127.0.0.1:{}{}?state=abc&error=access_denied&error_description=user+refused",
            handle.port(),
            CALLBACK_PATH
        );
        let resp = reqwest::get(&url).await.unwrap();
        assert!(resp.status().is_success());
        let body = resp.text().await.unwrap();
        assert!(body.contains("授权失败"), "failure HTML missing: {body}");
        let delivered = sink.0.lock().unwrap().clone();
        assert_eq!(
            delivered,
            vec![FeishuOAuthCallback {
                state: "abc".into(),
                result: Err("user refused".into()),
            }]
        );
        handle.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_listener_falls_back_to_next_port() {
        let sink = Arc::new(VecSink(std::sync::Mutex::new(Vec::new())));
        let first = bind_listener(sink.clone(), &tokio::runtime::Handle::current())
            .expect("first listener binds");
        let second = bind_listener(sink.clone(), &tokio::runtime::Handle::current())
            .expect("second listener binds on the next port");
        // `port+1` exactly only when nobody else grabbed the in-between
        // port; under parallel test execution a sibling test may hold it, so
        // assert the necessary contract instead: fallback, same range, and
        // no overlap with `first`.
        assert!((FIRST_PORT..=LAST_PORT).contains(&second.port()));
        assert!(second.port() > first.port());
        assert_ne!(second.port(), first.port());
        first.shutdown();
        second.shutdown();
    }
}
