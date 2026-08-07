//! Local Chrome/Edge launcher for Feishu QR sign-in and session reuse.
//!
//! RusTerm deliberately uses a dedicated persistent browser profile instead of
//! the user's default profile. This keeps Feishu cookies reusable while also
//! satisfying Chromium's requirement that remote debugging must not use the
//! default profile. CDP is used only through the loopback interface; no cookie,
//! token, message, or OTP value is logged.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tungstenite::{Message, WebSocket, client};

const DEVTOOLS_ACTIVE_PORT: &str = "DevToolsActivePort";
const DEVTOOLS_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DEVTOOLS_POLL_INTERVAL: Duration = Duration::from_millis(250);
const DEVTOOLS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const BROWSER_SESSION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DOM_WAIT_TIMEOUT: Duration = Duration::from_secs(8);
const OTP_REPLY_TIMEOUT: Duration = Duration::from_secs(25);
const AUTOMATION_POLL_INTERVAL: Duration = Duration::from_millis(150);
const MAX_CDP_HTTP_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedBrowser {
    Chrome,
    Edge,
}

impl SupportedBrowser {
    fn display_name(self) -> &'static str {
        match self {
            Self::Chrome => "Google Chrome",
            Self::Edge => "Microsoft Edge",
        }
    }

    fn profile_name(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Edge => "edge",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserLaunchPlan {
    browser: SupportedBrowser,
    executable: PathBuf,
    profile_dir: PathBuf,
    url: String,
}

impl BrowserLaunchPlan {
    fn new(
        browser: SupportedBrowser,
        executable: PathBuf,
        data_dir: &Path,
        url: String,
    ) -> Result<Self, String> {
        if !is_allowed_feishu_url(&url) {
            return Err("飞书登录地址无效，只允许打开官方 HTTPS 页面".to_string());
        }
        Ok(Self {
            browser,
            executable,
            profile_dir: data_dir
                .join("rusterm")
                .join("feishu-browser")
                .join(browser.profile_name()),
            url,
        })
    }

    fn arguments(&self) -> Vec<OsString> {
        vec![
            OsString::from(format!(
                "--user-data-dir={}",
                self.profile_dir.to_string_lossy()
            )),
            OsString::from("--remote-debugging-port=0"),
            OsString::from("--no-first-run"),
            OsString::from("--no-default-browser-check"),
            OsString::from("--no-startup-window"),
            // Chrome 111+ rejects DevTools websocket handshakes whose Origin
            // is not allow-listed; RusTerm's client sends none, but keep this
            // as insurance for any client that does.
            OsString::from("--remote-allow-origins=*"),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeishuBrowserEvent {
    Starting {
        browser: SupportedBrowser,
    },
    WaitingForScan,
    Navigating {
        url: String,
    },
    LoggedIn {
        url: String,
    },
    Failed {
        reason: String,
    },
    OtpRequestStarted {
        session: String,
        cycle_started: Instant,
    },
    OtpSendReady {
        session: String,
        request_id: u64,
        cycle_started: Instant,
    },
    OtpReply {
        session: String,
        cycle_started: Instant,
        body: String,
    },
    OtpFailed {
        session: String,
        cycle_started: Instant,
        reason: String,
    },
    Closed,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct CdpTarget {
    #[serde(default)]
    id: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "type", default)]
    target_type: String,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    web_socket_debugger_url: String,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
struct Point {
    x: f64,
    y: f64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct BotCandidate {
    name: String,
    has_robot_label: bool,
    center: Option<Point>,
}

#[derive(Debug, Deserialize)]
struct ComposerState {
    chat_name: String,
    placeholder: String,
    center: Option<Point>,
}

#[derive(Debug, Deserialize)]
struct MessageSnapshot {
    id: String,
    is_self: bool,
    body: String,
}

#[derive(Debug)]
struct AutomationFailure {
    reason: String,
    cdp_unavailable: bool,
}

impl AutomationFailure {
    fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            cdp_unavailable: true,
        }
    }

    fn dom(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            cdp_unavailable: false,
        }
    }
}

static EVENTS: OnceLock<Mutex<VecDeque<FeishuBrowserEvent>>> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static LOGGED_IN: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static NEXT_OTP_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static OPEN_LOCK: Mutex<()> = Mutex::new(());
static OTP_LOCK: Mutex<()> = Mutex::new(());
static CURRENT_PROFILE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static SEND_APPROVALS: OnceLock<Mutex<HashMap<u64, mpsc::SyncSender<bool>>>> = OnceLock::new();

fn events() -> &'static Mutex<VecDeque<FeishuBrowserEvent>> {
    EVENTS.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn publish(event: FeishuBrowserEvent) {
    if let Ok(mut queue) = events().lock() {
        queue.push_back(event);
    }
}

fn https_host_and_path(url: &str) -> Option<(String, String)> {
    let lower = url.trim().to_ascii_lowercase();
    let rest = lower.strip_prefix("https://")?;
    let authority_end = rest
        .char_indices()
        .find_map(|(index, ch)| matches!(ch, '/' | '?' | '#').then_some(index))
        .unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let host = authority.split(':').next()?.trim_end_matches('.');
    if host.is_empty() {
        return None;
    }
    let path_and_query = &rest[authority_end..];
    let path_end = path_and_query
        .char_indices()
        .find_map(|(index, ch)| matches!(ch, '?' | '#').then_some(index))
        .unwrap_or(path_and_query.len());
    Some((host.to_string(), path_and_query[..path_end].to_string()))
}

fn is_allowed_feishu_url(url: &str) -> bool {
    let Some((host, _)) = https_host_and_path(url) else {
        return false;
    };
    host == "feishu.cn"
        || host.ends_with(".feishu.cn")
        || host == "larksuite.com"
        || host.ends_with(".larksuite.com")
}

/// A tenant Messenger URL proves that the dedicated browser profile has an
/// authenticated Feishu session. Login/account pages and the generic entry do
/// not prove authentication.
pub fn looks_like_logged_in_feishu_url(url: &str) -> bool {
    let Some((host, path)) = https_host_and_path(url) else {
        return false;
    };
    (host.ends_with(".feishu.cn") || host.ends_with(".larksuite.com"))
        && path.starts_with("/next/messenger")
}

pub fn is_feishu_browser_active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

pub fn is_feishu_web_session_logged_in() -> bool {
    LOGGED_IN.load(Ordering::Acquire)
}

pub fn drain_feishu_browser_events() -> Vec<FeishuBrowserEvent> {
    events()
        .lock()
        .map(|mut queue| queue.drain(..).collect())
        .unwrap_or_default()
}

fn select_browser_from_paths(
    chrome: Option<PathBuf>,
    edge: Option<PathBuf>,
) -> Result<(SupportedBrowser, PathBuf), String> {
    if let Some(path) = chrome {
        return Ok((SupportedBrowser::Chrome, path));
    }
    if let Some(path) = edge {
        return Ok((SupportedBrowser::Edge, path));
    }
    Err(
        "未找到 Google Chrome 或 Microsoft Edge。请先安装其中一个浏览器后重试扫码登录。"
            .to_string(),
    )
}

fn env_executable(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

fn first_file(paths: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    paths.into_iter().find(|path| path.is_file())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn executable_on_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        for name in names {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn chrome_executable() -> Option<PathBuf> {
    env_executable("RUSTERM_CHROME_PATH").or_else(|| {
        first_file([PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )])
    })
}

#[cfg(target_os = "macos")]
fn edge_executable() -> Option<PathBuf> {
    env_executable("RUSTERM_EDGE_PATH").or_else(|| {
        first_file([PathBuf::from(
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        )])
    })
}

#[cfg(target_os = "windows")]
fn chrome_executable() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for variable in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        if let Some(root) = std::env::var_os(variable) {
            candidates.push(
                PathBuf::from(root)
                    .join("Google")
                    .join("Chrome")
                    .join("Application")
                    .join("chrome.exe"),
            );
        }
    }
    env_executable("RUSTERM_CHROME_PATH").or_else(|| first_file(candidates))
}

#[cfg(target_os = "windows")]
fn edge_executable() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for variable in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        if let Some(root) = std::env::var_os(variable) {
            candidates.push(
                PathBuf::from(root)
                    .join("Microsoft")
                    .join("Edge")
                    .join("Application")
                    .join("msedge.exe"),
            );
        }
    }
    env_executable("RUSTERM_EDGE_PATH").or_else(|| first_file(candidates))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn chrome_executable() -> Option<PathBuf> {
    env_executable("RUSTERM_CHROME_PATH").or_else(|| {
        executable_on_path(&[
            "google-chrome-stable",
            "google-chrome",
            "chromium-browser",
            "chromium",
        ])
    })
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn edge_executable() -> Option<PathBuf> {
    env_executable("RUSTERM_EDGE_PATH")
        .or_else(|| executable_on_path(&["microsoft-edge-stable", "microsoft-edge"]))
}

fn launch_plan(url: String) -> Result<BrowserLaunchPlan, String> {
    let (browser, executable) = select_browser_from_paths(chrome_executable(), edge_executable())?;
    let data_dir = dirs::data_dir().ok_or_else(|| "无法确定 RusTerm 数据目录".to_string())?;
    BrowserLaunchPlan::new(browser, executable, &data_dir, url)
}

fn current_profile() -> &'static Mutex<Option<PathBuf>> {
    CURRENT_PROFILE.get_or_init(|| Mutex::new(None))
}

/// Open Feishu in a local Chrome/Edge window backed by RusTerm's dedicated,
/// persistent profile. Chrome is preferred when both supported browsers are
/// installed. Browser startup and CDP monitoring happen off the UI thread.
pub fn open_feishu_login_window(url: String) {
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    let plan = match launch_plan(url) {
        Ok(plan) => plan,
        Err(reason) => {
            ACTIVE.store(false, Ordering::Release);
            publish(FeishuBrowserEvent::Failed { reason });
            return;
        }
    };

    if let Err(error) = fs::create_dir_all(&plan.profile_dir) {
        let reason = format!("无法创建飞书浏览器会话目录：{error}");
        tracing::warn!("[OTP-FEISHU] dedicated browser profile creation failed: {error}");
        ACTIVE.store(false, Ordering::Release);
        publish(FeishuBrowserEvent::Failed { reason });
        return;
    }

    if let Ok(mut profile) = current_profile().lock() {
        *profile = Some(plan.profile_dir.clone());
    }
    ACTIVE.store(true, Ordering::Release);
    publish(FeishuBrowserEvent::Starting {
        browser: plan.browser,
    });

    thread::spawn(move || open_browser_session(plan, generation));
}

fn open_browser_session(plan: BrowserLaunchPlan, generation: u64) {
    let Ok(_open_guard) = OPEN_LOCK.lock() else {
        finish_with_failure(generation, "浏览器会话锁不可用，请重试。");
        return;
    };
    if generation != GENERATION.load(Ordering::Acquire) {
        return;
    }

    let port = match working_devtools_port(&plan.profile_dir) {
        Some(port) => port,
        None => {
            let browser_name = plan.browser.display_name();
            let mut command = Command::new(&plan.executable);
            command
                .args(plan.arguments())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Err(error) = command.spawn() {
                let reason = format!("{browser_name} 启动失败：{error}");
                tracing::warn!("[OTP-FEISHU] supported browser launch failed: {error}");
                finish_with_failure(generation, &reason);
                return;
            }
            tracing::info!("[OTP-FEISHU] started dedicated {browser_name} session");

            let port = match wait_for_devtools_port(&plan.profile_dir) {
                Ok(port) => port,
                Err(last_error) => {
                    tracing::warn!(error = %last_error, "[OTP-FEISHU] browser started but CDP port unreachable");
                    finish_with_failure(
                        generation,
                        "浏览器已启动，但无法连接本地 CDP 调试端口。请关闭该 RusTerm 专用浏览器窗口后重试。",
                    );
                    return;
                }
            };
            port
        }
    };

    // If a newer open request arrived while a newly spawned browser was
    // exposing its debugging port, leave target creation to that request.
    if generation != GENERATION.load(Ordering::Acquire) {
        return;
    }

    let kept_logged_in = match ensure_single_feishu_target(port, &plan.url) {
        Ok(kept_logged_in) => kept_logged_in,
        Err(reason) => {
            finish_with_failure(generation, &reason);
            return;
        }
    };
    LOGGED_IN.store(kept_logged_in, Ordering::Release);
    publish(FeishuBrowserEvent::WaitingForScan);

    drop(_open_guard);
    monitor_browser_session(port, generation);
}

/// An external browser cannot be hidden safely. Authentication completion only
/// updates the UI state; the browser and persistent session remain available.
pub fn hide_feishu_login_window() {
    ACTIVE.store(false, Ordering::Release);
}

/// Stop monitoring the current sign-in attempt without killing Chrome/Edge.
pub fn close_feishu_login_window() {
    GENERATION.fetch_add(1, Ordering::AcqRel);
    ACTIVE.store(false, Ordering::Release);
    publish(FeishuBrowserEvent::Closed);
}

fn send_approvals() -> &'static Mutex<HashMap<u64, mpsc::SyncSender<bool>>> {
    SEND_APPROVALS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve the final UI-owned guard immediately before browser automation
/// presses Enter. Unknown or stale request IDs are ignored.
pub fn approve_feishu_otp_send(request_id: u64, approved: bool) {
    let sender = send_approvals()
        .lock()
        .ok()
        .and_then(|mut approvals| approvals.remove(&request_id));
    if let Some(sender) = sender {
        let _ = sender.send(approved);
    }
}

/// Ask the named Feishu bot for an OTP-related reply in the current authenticated
/// Messenger target. `search_keys` are tried in order against the palette search
/// box (a short pinyin key ranks the bot better than its full name in tenants
/// where pinyin lookup is enabled, while the full name works everywhere); the
/// first key that produces a unique exact-name robot candidate wins. Candidate
/// selection always requires an exact `bot_name` match. The request runs away
/// from the UI thread and retains the originating tty cycle so stale replies
/// cannot affect a later prompt.
pub fn request_feishu_otp(
    session: String,
    bot_name: String,
    search_keys: Vec<String>,
    request_text: String,
    code_pattern: String,
    cycle_started: Instant,
) {
    let request_id = NEXT_OTP_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    publish(FeishuBrowserEvent::OtpRequestStarted {
        session: session.clone(),
        cycle_started,
    });
    thread::spawn(move || {
        let approval = || {
            let (sender, receiver) = mpsc::sync_channel(1);
            send_approvals()
                .lock()
                .map_err(|_| AutomationFailure::dom("飞书发送审批锁不可用，请重试。"))?
                .insert(request_id, sender);
            publish(FeishuBrowserEvent::OtpSendReady {
                session: session.clone(),
                request_id,
                cycle_started,
            });
            let approved = receiver
                .recv_timeout(Duration::from_secs(3))
                .unwrap_or(false);
            if let Ok(mut approvals) = send_approvals().lock() {
                approvals.remove(&request_id);
            }
            if approved {
                Ok(())
            } else {
                Err(AutomationFailure::dom(
                    "终端已离开二次密码提示，未发送动态口令。",
                ))
            }
        };
        let result = match OTP_LOCK.lock() {
            Ok(_guard) => automate_feishu_otp(
                &bot_name,
                &search_keys,
                &request_text,
                &code_pattern,
                approval,
            ),
            Err(_) => Err(AutomationFailure::dom("飞书自动化会话锁不可用，请重试。")),
        };
        match result {
            Ok(body) => publish(FeishuBrowserEvent::OtpReply {
                session,
                cycle_started,
                body,
            }),
            Err(failure) => {
                if failure.cdp_unavailable {
                    LOGGED_IN.store(false, Ordering::Release);
                }
                publish(FeishuBrowserEvent::OtpFailed {
                    session,
                    cycle_started,
                    reason: failure.reason,
                });
            }
        }
    });
}

fn probe_devtools_port(profile_dir: &Path) -> Result<u16, String> {
    let contents = fs::read_to_string(profile_dir.join(DEVTOOLS_ACTIVE_PORT))
        .map_err(|error| format!("DevToolsActivePort unreadable: {error}"))?;
    let port = parse_devtools_port(&contents)
        .ok_or_else(|| "DevToolsActivePort has no valid port".to_string())?;
    fetch_cdp_targets(port).map(|_| port)
}

fn working_devtools_port(profile_dir: &Path) -> Option<u16> {
    probe_devtools_port(profile_dir).ok()
}

fn wait_for_devtools_port(profile_dir: &Path) -> Result<u16, String> {
    let deadline = Instant::now() + DEVTOOLS_CONNECT_TIMEOUT;
    loop {
        match probe_devtools_port(profile_dir) {
            Ok(port) => return Ok(port),
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(error);
                }
            }
        }
        thread::sleep(DEVTOOLS_POLL_INTERVAL);
    }
}

fn monitor_browser_session(port: u16, generation: u64) {
    let session_deadline = Instant::now() + BROWSER_SESSION_TIMEOUT;
    let mut last_url = String::new();
    let mut consecutive_failures = 0u8;
    loop {
        if generation != GENERATION.load(Ordering::Acquire) {
            return;
        }
        match fetch_cdp_targets(port) {
            Ok(targets) => {
                consecutive_failures = 0;
                if let Some(target) = targets.iter().find(|target| {
                    target.target_type == "page" && looks_like_logged_in_feishu_url(&target.url)
                }) {
                    LOGGED_IN.store(true, Ordering::Release);
                    if generation == GENERATION.load(Ordering::Acquire) {
                        ACTIVE.store(false, Ordering::Release);
                        tracing::info!("[OTP-FEISHU] Feishu Web session authenticated");
                        publish(FeishuBrowserEvent::LoggedIn {
                            url: target.url.clone(),
                        });
                    }
                    return;
                }
                if let Some(target) = targets.iter().find(|target| {
                    target.target_type == "page" && is_allowed_feishu_url(&target.url)
                }) && target.url != last_url
                {
                    last_url.clone_from(&target.url);
                    publish(FeishuBrowserEvent::Navigating {
                        url: target.url.clone(),
                    });
                }
            }
            Err(_) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                if consecutive_failures >= 8 {
                    if generation == GENERATION.load(Ordering::Acquire) {
                        ACTIVE.store(false, Ordering::Release);
                        LOGGED_IN.store(false, Ordering::Release);
                        publish(FeishuBrowserEvent::Closed);
                    }
                    return;
                }
            }
        }
        if Instant::now() >= session_deadline {
            finish_with_failure(generation, "飞书扫码登录等待超时，请重新授权后再试。");
            return;
        }
        thread::sleep(DEVTOOLS_POLL_INTERVAL);
    }
}

fn finish_with_failure(generation: u64, reason: &str) {
    if generation == GENERATION.load(Ordering::Acquire) {
        ACTIVE.store(false, Ordering::Release);
        tracing::warn!("[OTP-FEISHU] {reason}");
        publish(FeishuBrowserEvent::Failed {
            reason: reason.to_string(),
        });
    }
}

fn parse_devtools_port(contents: &str) -> Option<u16> {
    contents.lines().next()?.trim().parse().ok()
}

fn percent_encode_url(url: &str) -> String {
    let mut encoded = String::with_capacity(url.len());
    for byte in url.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

fn cdp_http_request(port: u16, method: &str, path: &str) -> Result<Vec<u8>, String> {
    if !matches!(method, "GET" | "PUT") || !path.starts_with('/') {
        return Err("Invalid CDP HTTP request".to_string());
    }
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let mut stream = TcpStream::connect_timeout(&address.into(), DEVTOOLS_REQUEST_TIMEOUT)
        .map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(DEVTOOLS_REQUEST_TIMEOUT))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(DEVTOOLS_REQUEST_TIMEOUT))
        .map_err(|error| error.to_string())?;
    let request =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|error| error.to_string())?;

    let (status, body) = read_http_response(&mut stream)?;
    if !(200..300).contains(&status) {
        return Err(format!("CDP HTTP request failed with status {status}"));
    }
    Ok(body)
}

/// Read an HTTP/1.1 response honoring `Content-Length`: Chrome's DevTools HTTP
/// server keeps the connection open after sending the full body, so blocking
/// on EOF would stall until the read timeout. The body is read exactly
/// `Content-Length` bytes when present; otherwise fall back to a bounded
/// read-to-EOF as before.
fn read_http_response(reader: &mut impl Read) -> Result<(u16, Vec<u8>), String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() as u64 >= MAX_CDP_HTTP_RESPONSE_BYTES {
            return Err("HTTP response headers exceed limit".to_string());
        }
        let read = reader
            .read(&mut chunk)
            .map_err(|error| format!("read: {error}"))?;
        if read == 0 {
            return Err("CDP returned an invalid HTTP response".to_string());
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]);
    let (status, content_length) = parse_http_head(&head)?;
    let body = match content_length {
        Some(length) => {
            if length as u64 > MAX_CDP_HTTP_RESPONSE_BYTES {
                return Err("CDP HTTP response exceeded 2 MiB".to_string());
            }
            buffer.drain(..head_end);
            while buffer.len() < length {
                let read = reader
                    .read(&mut chunk)
                    .map_err(|error| format!("read: {error}"))?;
                if read == 0 {
                    return Err("read: unexpected EOF before HTTP body completed".to_string());
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            buffer.truncate(length);
            buffer
        }
        None => {
            let mut body = buffer.split_off(head_end);
            let remaining = (MAX_CDP_HTTP_RESPONSE_BYTES + 1).saturating_sub(body.len() as u64);
            (&mut *reader)
                .take(remaining)
                .read_to_end(&mut body)
                .map_err(|error| format!("read: {error}"))?;
            if body.len() as u64 > MAX_CDP_HTTP_RESPONSE_BYTES {
                return Err("CDP HTTP response exceeded 2 MiB".to_string());
            }
            body
        }
    };
    Ok((status, body))
}

/// Parse an HTTP response head into its status code and optional
/// `Content-Length`. Header names are matched case-insensitively.
fn parse_http_head(head: &str) -> Result<(u16, Option<usize>), String> {
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let invalid = || "CDP returned an invalid HTTP status".to_string();
    if !status_line.starts_with("HTTP/") {
        return Err(invalid());
    }
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(invalid)?;
    let mut content_length = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }
    Ok((status, content_length))
}

fn fetch_cdp_targets(port: u16) -> Result<Vec<CdpTarget>, String> {
    let body = cdp_http_request(port, "GET", "/json/list")?;
    serde_json::from_slice(&body).map_err(|error| error.to_string())
}

fn activate_cdp_target(port: u16, target_id: &str) -> Result<(), String> {
    let path = format!("/json/activate/{}", percent_encode_url(target_id));
    cdp_http_request(port, "GET", &path).map(|_| ())
}

fn close_cdp_target(port: u16, target_id: &str) -> Result<(), String> {
    let path = format!("/json/close/{}", percent_encode_url(target_id));
    cdp_http_request(port, "GET", &path).map(|_| ())
}

fn create_cdp_target(port: u16, url: &str) -> Result<CdpTarget, String> {
    let path = format!("/json/new?{}", percent_encode_url(url));
    let body = cdp_http_request(port, "PUT", &path)?;
    serde_json::from_slice(&body).map_err(|error| error.to_string())
}

fn select_feishu_target(targets: &[CdpTarget]) -> Option<(usize, Vec<String>)> {
    let candidates = targets
        .iter()
        .enumerate()
        .filter(|(_, target)| target.target_type == "page" && is_allowed_feishu_url(&target.url))
        .collect::<Vec<_>>();
    let keep_index = candidates
        .iter()
        .find(|(_, target)| looks_like_logged_in_feishu_url(&target.url))
        .or_else(|| candidates.first())?
        .0;
    let duplicate_ids = candidates
        .into_iter()
        .filter(|(index, _)| *index != keep_index)
        .filter_map(|(_, target)| (!target.id.is_empty()).then(|| target.id.clone()))
        .collect();
    Some((keep_index, duplicate_ids))
}

fn is_www_feishu_messenger_url(url: &str) -> bool {
    matches!(
        https_host_and_path(url),
        Some((host, path))
            if host == "www.feishu.cn" && matches!(path.as_str(), "/messenger" | "/messenger/")
    )
}

fn should_navigate_reused_target(target_url: &str, requested_url: &str) -> bool {
    !(looks_like_logged_in_feishu_url(target_url) && is_www_feishu_messenger_url(requested_url))
}

fn ensure_single_feishu_target(port: u16, requested_url: &str) -> Result<bool, String> {
    let targets = fetch_cdp_targets(port)?;
    if let Some((keep_index, duplicate_ids)) = select_feishu_target(&targets) {
        let target = targets[keep_index].clone();
        for duplicate_id in duplicate_ids {
            close_cdp_target(port, &duplicate_id)?;
        }
        activate_cdp_target(port, &target.id)?;
        if should_navigate_reused_target(&target.url, requested_url) {
            navigate_cdp_target(&target, requested_url)?;
            return Ok(false);
        }
        return Ok(looks_like_logged_in_feishu_url(&target.url));
    }

    let target = create_cdp_target(port, requested_url)?;
    activate_cdp_target(port, &target.id)?;
    Ok(false)
}

fn navigate_cdp_target(target: &CdpTarget, url: &str) -> Result<(), String> {
    if target.web_socket_debugger_url.is_empty() {
        return Err("飞书页面缺少 CDP 调试地址".to_string());
    }
    let mut client =
        CdpClient::connect(&target.web_socket_debugger_url).map_err(|failure| failure.reason)?;
    client
        .command("Page.navigate", json!({ "url": url }))
        .map_err(|failure| failure.reason)?;
    Ok(())
}

struct CdpClient {
    socket: WebSocket<TcpStream>,
    next_id: u64,
}

impl CdpClient {
    fn connect(web_socket_url: &str) -> Result<Self, AutomationFailure> {
        let address = parse_loopback_websocket_address(web_socket_url)?;
        let stream = TcpStream::connect_timeout(&address, DEVTOOLS_REQUEST_TIMEOUT)
            .map_err(|_| AutomationFailure::unavailable("无法连接飞书页面的 CDP 调试目标。"))?;
        stream
            .set_read_timeout(Some(DEVTOOLS_REQUEST_TIMEOUT))
            .map_err(|_| AutomationFailure::unavailable("无法配置 CDP 读取超时。"))?;
        stream
            .set_write_timeout(Some(DEVTOOLS_REQUEST_TIMEOUT))
            .map_err(|_| AutomationFailure::unavailable("无法配置 CDP 写入超时。"))?;
        let (socket, _) = client(web_socket_url, stream)
            .map_err(|_| AutomationFailure::unavailable("飞书页面的 CDP 握手失败。"))?;
        Ok(Self { socket, next_id: 1 })
    }

    fn command(&mut self, method: &str, params: Value) -> Result<Value, AutomationFailure> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let payload = json!({ "id": id, "method": method, "params": params }).to_string();
        self.socket.send(Message::text(payload)).map_err(|_| {
            AutomationFailure::unavailable("CDP 命令发送失败，飞书页面可能已关闭。")
        })?;

        let deadline = Instant::now() + DEVTOOLS_REQUEST_TIMEOUT;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(AutomationFailure::unavailable(
                    "CDP 响应超时，飞书页面可能已关闭。",
                ));
            }
            self.socket
                .get_mut()
                .set_read_timeout(Some(deadline.saturating_duration_since(now)))
                .map_err(|_| AutomationFailure::unavailable("无法配置 CDP 命令超时。"))?;
            let message = self.socket.read().map_err(|_| {
                AutomationFailure::unavailable("CDP 响应超时，飞书页面可能已关闭。")
            })?;
            if message.is_ping() {
                self.socket.flush().map_err(|_| {
                    AutomationFailure::unavailable("CDP 心跳响应失败，飞书页面可能已关闭。")
                })?;
                continue;
            }
            if message.is_close() {
                return Err(AutomationFailure::unavailable(
                    "飞书页面的 CDP 连接已关闭。",
                ));
            }
            let Ok(text) = message.to_text() else {
                continue;
            };
            let Ok(response) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if response.get("error").is_some() {
                return Err(AutomationFailure::dom("浏览器自动化命令执行失败。"));
            }
            return response
                .get("result")
                .cloned()
                .ok_or_else(|| AutomationFailure::dom("CDP 响应缺少命令结果。"));
        }
    }

    fn evaluate<T: DeserializeOwned>(&mut self, expression: &str) -> Result<T, AutomationFailure> {
        let result = self.command(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
            }),
        )?;
        if result.get("exceptionDetails").is_some() {
            return Err(AutomationFailure::dom("飞书页面脚本执行失败。"));
        }
        let value = result
            .pointer("/result/value")
            .cloned()
            .ok_or_else(|| AutomationFailure::dom("飞书页面脚本未返回结果。"))?;
        serde_json::from_value(value)
            .map_err(|_| AutomationFailure::dom("飞书页面脚本返回了无效结果。"))
    }

    fn click(&mut self, point: Point) -> Result<(), AutomationFailure> {
        self.command(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mousePressed",
                "x": point.x,
                "y": point.y,
                "button": "left",
                "clickCount": 1,
            }),
        )?;
        self.command(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseReleased",
                "x": point.x,
                "y": point.y,
                "button": "left",
                "clickCount": 1,
            }),
        )?;
        Ok(())
    }

    fn clear_focused_editor(&mut self) -> Result<(), AutomationFailure> {
        self.command(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyDown", "commands": ["selectAll"] }),
        )?;
        self.command("Input.dispatchKeyEvent", json!({ "type": "keyUp" }))?;
        self.command(
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyDown",
                "key": "Backspace",
                "code": "Backspace",
                "windowsVirtualKeyCode": 8,
                "nativeVirtualKeyCode": 8,
            }),
        )?;
        self.command(
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp",
                "key": "Backspace",
                "code": "Backspace",
                "windowsVirtualKeyCode": 8,
                "nativeVirtualKeyCode": 8,
            }),
        )?;
        Ok(())
    }

    fn insert_text(&mut self, text: &str) -> Result<(), AutomationFailure> {
        self.command("Input.insertText", json!({ "text": text }))?;
        Ok(())
    }

    fn press_enter(&mut self) -> Result<(), AutomationFailure> {
        for event_type in ["keyDown", "keyUp"] {
            self.command(
                "Input.dispatchKeyEvent",
                json!({
                    "type": event_type,
                    "key": "Enter",
                    "code": "Enter",
                    "windowsVirtualKeyCode": 13,
                    "nativeVirtualKeyCode": 13,
                }),
            )?;
        }
        Ok(())
    }
}

fn parse_loopback_websocket_address(url: &str) -> Result<SocketAddr, AutomationFailure> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| AutomationFailure::unavailable("CDP 调试地址不是本地 WebSocket。"))?;
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port_text) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, port) = rest
            .split_once("]:")
            .ok_or_else(|| AutomationFailure::unavailable("CDP 调试地址无效。"))?;
        (host, port)
    } else {
        authority
            .rsplit_once(':')
            .ok_or_else(|| AutomationFailure::unavailable("CDP 调试地址无效。"))?
    };
    let port = port_text
        .parse::<u16>()
        .map_err(|_| AutomationFailure::unavailable("CDP 调试端口无效。"))?;
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into());
    }
    let ip = host
        .parse::<IpAddr>()
        .map_err(|_| AutomationFailure::unavailable("CDP 调试地址不是 loopback。"))?;
    if !ip.is_loopback() {
        return Err(AutomationFailure::unavailable(
            "拒绝连接非 loopback 的 CDP 调试地址。",
        ));
    }
    Ok(SocketAddr::new(ip, port))
}

fn wait_for_point(
    client: &mut CdpClient,
    expression: &str,
    failure_reason: &str,
) -> Result<Point, AutomationFailure> {
    let deadline = Instant::now() + DOM_WAIT_TIMEOUT;
    loop {
        if let Some(point) = client.evaluate::<Option<Point>>(expression)? {
            return Ok(point);
        }
        if Instant::now() >= deadline {
            return Err(AutomationFailure::dom(failure_reason));
        }
        thread::sleep(AUTOMATION_POLL_INTERVAL);
    }
}

fn json_string(text: &str) -> String {
    Value::String(text.to_string()).to_string()
}

/// Locates the search editor. Prefers the known Lark DOM id, falls back to any
/// contenteditable inside a modal dialog, then to the currently focused
/// editable element (clicking the navbar search entry focuses the editor).
fn search_editor_finder() -> String {
    "(document.querySelector('#search_bar_editor [contenteditable=true]') \
        || document.querySelector('[role=dialog] [contenteditable=true]') \
        || document.querySelector('.dialog [contenteditable=true]') \
        || ((document.activeElement && document.activeElement.isContentEditable \
            && document.activeElement.getBoundingClientRect().width > 0) ? document.activeElement : null))"
        .to_string()
}

/// Locates the chat composer. Prefers the known Lark editor class, falls back
/// to the first visible contenteditable that is not inside the search dialog.
fn composer_finder() -> String {
    "(document.querySelector('[contenteditable=true].innerdocbody') \
        || Array.from(document.querySelectorAll('[contenteditable=true]')) \
            .find((el) => { \
                const rect = el.getBoundingClientRect(); \
                return rect.width > 0 && rect.height > 0 \
                    && !el.closest('[role=dialog], .dialog, #search_bar_editor, .appNavbar'); \
            }) || null)"
        .to_string()
}

fn element_focus_script(finders_expr: &str) -> String {
    "(() => { const el = __FINDERS__; if (!el) return false; el.focus(); return true; })()"
        .replace("__FINDERS__", finders_expr)
}

fn element_text_script(finders_expr: &str) -> String {
    "(() => { const el = __FINDERS__; if (!el) return null; \
        return (el.innerText || el.textContent || '').replace(/\\u200B/g, '').trim(); })()"
        .replace("__FINDERS__", finders_expr)
}

/// Script that focuses the located editor, selects its contents, and inserts
/// `text` via `document.execCommand('insertText')`. Unlike the raw CDP
/// `Input.insertText`, this runs the browser's real editing pipeline, which
/// fires the beforeinput/input events Feishu's custom editor listens to — the
/// raw CDP call was observed to silently drop text in the live app. Handles
/// plain INPUT/TEXTAREA controls via the native value setter. Returns
/// 'ok' | 'mismatch' | 'missing' after a read-back verification.
fn text_insert_script(finders_expr: &str, text: &str) -> String {
    "(() => {\
        const el = __FINDERS__;\
        if (!el) return 'missing';\
        const tag = (el.tagName || '').toUpperCase();\
        el.focus();\
        if (tag === 'INPUT' || tag === 'TEXTAREA') {\
            const proto = tag === 'INPUT' ? HTMLInputElement.prototype : HTMLTextAreaElement.prototype;\
            const descriptor = Object.getOwnPropertyDescriptor(proto, 'value');\
            if (descriptor && descriptor.set) descriptor.set.call(el, __TEXT__);\
            else el.value = __TEXT__;\
            el.dispatchEvent(new Event('input', { bubbles: true }));\
            el.dispatchEvent(new Event('change', { bubbles: true }));\
            return el.value === __TEXT__ ? 'ok' : 'mismatch';\
        }\
        // Contenteditable path: fully clear any stale text first (automation\
        // retries and Feishu's own state can leave ghost content which would\
        // otherwise be silently appended to), then place the DOM selection on\
        // the editor so execCommand's beforeinput lands at a known location.\
        if (el.isContentEditable) {\
            const selection = window.getSelection();\
            const range = document.createRange();\
            range.selectNodeContents(el);\
            selection.removeAllRanges();\
            selection.addRange(range);\
            document.execCommand('delete', false);\
            selection.removeAllRanges();\
            const caret = document.createRange();\
            caret.selectNodeContents(el);\
            caret.collapse(false);\
            selection.addRange(caret);\
        }\
        // Feishu's Slate editor caches its internal state across palette\
        // open/close cycles, and plain execCommand('insertText') then leaves\
        // the DOM updated but the Slate state stale — no onChange fires, so\
        // the search never runs. Wrapping the insert in synthetic\
        // compositionstart/end events forces Slate down its IME code path,\
        // which always propagates the change. Harmless when Slate was already\
        // in sync (fresh mount).\
        el.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true, cancelable: true, data: '' }));\
        el.dispatchEvent(new CompositionEvent('compositionupdate', { bubbles: true, cancelable: true, data: __TEXT__ }));\
        const inserted = document.execCommand('insertText', false, __TEXT__);\
        el.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, cancelable: true, data: __TEXT__ }));\
        if (!inserted) {\
            // Some editors reject execCommand; fall back to a direct DOM\
            // mutation plus synthetic input events, which Feishu's Slate\
            // instance still observes.\
            el.textContent = __TEXT__;\
            el.dispatchEvent(new InputEvent('input', { bubbles: true, cancelable: true, inputType: 'insertText', data: __TEXT__ }));\
        }\
        const current = (el.innerText || el.textContent || '').replace(/\\u200B/g, '').trim();\
        return current === __EXPECTED__ ? 'ok' : 'mismatch';\
    })()"
    .replace("__FINDERS__", finders_expr)
    .replace("__TEXT__", &json_string(text))
    .replace("__EXPECTED__", &json_string(text.trim()))
}

/// Verified, retrying text entry. Live runs showed the bare CDP
/// `Input.insertText` intermittently landing nowhere (empty search box / wrong
/// composer content) depending on which element held focus at that instant, so
/// every write is now followed by a DOM read-back and retried, alternating
/// between the in-page execCommand pipeline and the CDP input pipeline (each
/// attempt re-clears the editor first, so retries cannot duplicate text).
fn type_into_editor(
    client: &mut CdpClient,
    finders_expr: &str,
    text: &str,
    failure_reason: &str,
) -> Result<(), AutomationFailure> {
    let exec_script = text_insert_script(finders_expr, text);
    let focus_script = element_focus_script(finders_expr);
    let text_script = element_text_script(finders_expr);
    let mut last_state = "missing".to_string();
    for attempt in 0..6 {
        if attempt > 0 {
            thread::sleep(AUTOMATION_POLL_INTERVAL);
        }
        if attempt % 3 == 2 {
            // Fallback: focus via DOM, then drive the native CDP input pipeline.
            let focused: bool = client.evaluate(&focus_script)?;
            if focused {
                client.clear_focused_editor()?;
                client.insert_text(text)?;
                thread::sleep(AUTOMATION_POLL_INTERVAL);
                let current: Option<String> = client.evaluate(&text_script)?;
                if current.as_deref() == Some(text.trim()) {
                    return Ok(());
                }
                last_state = "cdp-mismatch".to_string();
                continue;
            }
        }
        let state: String = client.evaluate(&exec_script)?;
        last_state = state.clone();
        tracing::info!(
            "[OTP-FEISHU] text entry attempt {} -> {} (text len={})",
            attempt,
            state,
            text.chars().count()
        );
        if state == "ok" {
            return Ok(());
        }
    }
    tracing::warn!(
        "[OTP-FEISHU] editor text entry failed after retries (state={last_state}, text len={})",
        text.chars().count()
    );
    Err(AutomationFailure::dom(failure_reason))
}

fn unique_exact_bot_candidate(
    candidates: &[BotCandidate],
    bot_name: &str,
) -> Result<Point, String> {
    let matches = candidates
        .iter()
        .filter(|candidate| candidate.name == bot_name && candidate.has_robot_label)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [candidate] => candidate
            .center
            .ok_or_else(|| "完全匹配的机器人结果当前不可点击。".to_string()),
        [] => Err("未找到名称完全匹配且标记为机器人的飞书联系人。".to_string()),
        _ => Err("找到多个名称完全匹配的机器人，已停止发送。".to_string()),
    }
}

fn message_snapshots(client: &mut CdpClient) -> Result<Vec<MessageSnapshot>, AutomationFailure> {
    client.evaluate(
        r#"(() => Array.from(document.querySelectorAll('.messageItem-wrapper[data-id]')).map(wrapper => ({
            id: wrapper.getAttribute('data-id') || '',
            is_self: !!wrapper.querySelector('.message-self'),
            body: (wrapper.querySelector('.message-content')?.innerText || '').trim()
        })).filter(message => message.id))()"#,
    )
}

/// Build the ordered list of palette search keys: each trimmed, deduplicated,
/// and falling back to the bot's own name when the caller gave no usable key.
///
/// The palette treats every entry as an alternative lookup key. A short pinyin
/// key (e.g. "zxa") ranks the target bot ahead of look-alike entries such as
/// `OTP-智小安` in tenants where Feishu indexes pinyin aliases, but that
/// tenant-level index is not always enabled, so the full bot name is kept as
/// a fallback that always resolves. Selection of the actual candidate still
/// requires an exact `bot_name` match, so a fuzzy search can never divert the
/// flow to the wrong bot.
fn build_search_keys(bot_name: &str, search_keys: &[String]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for key in search_keys {
        let trimmed = key.trim();
        if trimmed.is_empty() {
            continue;
        }
        if keys.iter().any(|seen| seen == trimmed) {
            continue;
        }
        keys.push(trimmed.to_string());
    }
    if keys.is_empty() {
        keys.push(bot_name.trim().to_string());
    }
    keys
}

/// Per-key budget while polling for palette candidates. Feishu renders a
/// skeleton for ~400 ms before results land, and 3 s comfortably covers the
/// search round-trip; keeping the per-key budget short lets a failing alias
/// (e.g. "zxa" in a tenant without pinyin index) yield quickly to the next
/// key instead of burning the whole budget on the first attempt.
const SEARCH_KEY_CANDIDATES_TIMEOUT: Duration = Duration::from_secs(3);

fn automate_feishu_otp(
    bot_name: &str,
    search_keys: &[String],
    request_text: &str,
    code_pattern: &str,
    approve_send: impl FnOnce() -> Result<(), AutomationFailure>,
) -> Result<String, AutomationFailure> {
    if bot_name.trim().is_empty() || request_text.trim().is_empty() {
        return Err(AutomationFailure::dom("机器人名称和请求内容不能为空。"));
    }
    let profile_dir = current_profile()
        .lock()
        .ok()
        .and_then(|profile| profile.clone())
        .ok_or_else(|| AutomationFailure::unavailable("当前没有可用的飞书浏览器会话。"))?;
    let port = working_devtools_port(&profile_dir)
        .ok_or_else(|| AutomationFailure::unavailable("飞书浏览器的 CDP 端口不可用。"))?;
    let targets = fetch_cdp_targets(port)
        .map_err(|_| AutomationFailure::unavailable("无法读取飞书浏览器页面列表。"))?;
    let target = targets
        .iter()
        .find(|target| {
            target.target_type == "page"
                && looks_like_logged_in_feishu_url(&target.url)
                && !target.web_socket_debugger_url.is_empty()
        })
        .ok_or_else(|| AutomationFailure::unavailable("未找到已登录的飞书 Messenger 页面。"))?;
    activate_cdp_target(port, &target.id)
        .map_err(|_| AutomationFailure::unavailable("无法激活飞书 Messenger 页面。"))?;
    let mut client = CdpClient::connect(&target.web_socket_debugger_url)?;
    client.command("Runtime.enable", json!({}))?;

    let search_entry = wait_for_point(
        &mut client,
        r#"(() => {
            const element = document.querySelector('.appNavbar-search-input');
            if (!element) return null;
            const rect = element.getBoundingClientRect();
            if (rect.width <= 0 || rect.height <= 0) return null;
            return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
        })()"#,
        "未找到飞书搜索入口。",
    )?;
    client.click(search_entry)?;

    // Primary strategy uses the known Lark result-card classes. The fallback
    // locates a bare "机器人" badge, walks up to its small surrounding result
    // box, and matches an exact-name element inside it — entries under a
    // "历史记录"/history container are excluded so stale history chips cannot
    // masquerade as live results. Both still require name == bot_name and the
    // robot label, and uniqueness is enforced later.
    let candidates_expression = r#"(() => {
        const out = [];
        const seen = new Set();
        const record = (name, hasLabel, rect) => {
            if (!rect || rect.width <= 0 || rect.height <= 0) return;
            const key = Math.round(rect.x / 4) + '/' + Math.round(rect.y / 4);
            if (seen.has(key)) return;
            seen.add(key);
            out.push({
                name,
                has_robot_label: !!hasLabel,
                center: { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 }
            });
        };
        document.querySelectorAll('.bot-chatter-info-name').forEach((nameElement) => {
            const card = nameElement.closest('.bot-result-card');
            const rect = card ? card.getBoundingClientRect() : null;
            const hasLabel = !!card && Array.from(card.querySelectorAll('*'))
                .some((element) => (element.textContent || '').trim() === '机器人');
            record((nameElement.textContent || '').trim(), hasLabel, rect);
        });
        if (!out.some((candidate) => candidate.has_robot_label)) {
            const botName = __BOT__;
            const inHistory = (el) => {
                let node = el;
                for (let i = 0; i < 8 && node; i += 1) {
                    const cls = String(node.className || '').toLowerCase();
                    if (cls.indexOf('history') !== -1 || cls.indexOf('recent') !== -1) return true;
                    for (const child of node.children) {
                        const text = (child.textContent || '').trim();
                        if (text === '历史记录' || text === '搜索历史' || text === '历史搜索') return true;
                    }
                    node = node.parentElement;
                }
                return false;
            };
            const labels = Array.from(document.querySelectorAll('span, div, p, li'))
                .filter((el) => el.childElementCount === 0 && (el.textContent || '').trim() === '机器人');
            for (const label of labels) {
                if (inHistory(label)) continue;
                let box = label.parentElement;
                for (let i = 0; i < 5 && box; i += 1) {
                    const boxText = (box.textContent || '').trim();
                    if (boxText.length > 3 && boxText.length <= 80) {
                        const nameElement = Array.from(box.querySelectorAll('*'))
                            .find((el) => el.childElementCount === 0 && (el.textContent || '').trim() === botName);
                        if (nameElement) {
                            record(botName, true, nameElement.getBoundingClientRect());
                            break;
                        }
                    }
                    box = box.parentElement;
                }
            }
        }
        return out;
    })()"#
    .replace("__BOT__", &json_string(bot_name));

    // Search-key fallback chain: try each alias in order. For every key the
    // palette editor is re-located and re-typed (palette re-renders stale the
    // DOM references), then candidates are polled for a short per-key budget.
    // The first key that yields a usable exact bot match wins.
    let keys = build_search_keys(bot_name, search_keys);
    let mut bot_point: Option<Point> = None;
    for (key_index, key) in keys.iter().enumerate() {
        let search_editor = wait_for_point(
            &mut client,
            r#"(() => {
                const element = document.querySelector('#search_bar_editor [contenteditable=true]');
                if (!element) return null;
                const rect = element.getBoundingClientRect();
                if (rect.width <= 0 || rect.height <= 0) return null;
                return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
            })()"#,
            "未找到飞书搜索输入框。",
        )?;
        client.click(search_editor)?;
        type_into_editor(
            &mut client,
            &search_editor_finder(),
            key,
            "未能在飞书搜索框中输入机器人名称。",
        )?;
        tracing::info!(
            "[OTP-FEISHU][SEARCH] key '{}' ({}/{}) typed, polling candidates",
            key,
            key_index + 1,
            keys.len()
        );

        let key_deadline = Instant::now() + SEARCH_KEY_CANDIDATES_TIMEOUT;
        loop {
            let candidates: Vec<BotCandidate> = client.evaluate(&candidates_expression)?;
            if candidates
                .iter()
                .any(|candidate| candidate.name == bot_name && candidate.has_robot_label)
            {
                // Settle briefly so the click center is stable, then enforce
                // exact name + robot label + uniqueness before clicking.
                thread::sleep(Duration::from_millis(300));
                let settled: Vec<BotCandidate> = client.evaluate(&candidates_expression)?;
                match unique_exact_bot_candidate(&settled, bot_name) {
                    Ok(point) => {
                        bot_point = Some(point);
                        tracing::info!(
                            "[OTP-FEISHU][SEARCH] key '{}' hit exact bot candidate",
                            key
                        );
                    }
                    Err(reason) => {
                        tracing::warn!(
                            "[OTP-FEISHU][SEARCH] key '{}' candidate rejected: {}",
                            key,
                            reason
                        );
                    }
                }
                break;
            }
            if Instant::now() >= key_deadline {
                tracing::info!(
                    "[OTP-FEISHU][SEARCH] key '{}' ({}/{}) miss, trying next key",
                    key,
                    key_index + 1,
                    keys.len()
                );
                break;
            }
            thread::sleep(AUTOMATION_POLL_INTERVAL);
        }

        if bot_point.is_some() {
            break;
        }

        // Key missed. Feishu keeps recent searches under a history list; an
        // entry exactly matching this key is a cheap way to reach the result
        // list without relying on the flaky editor insert path.
        let history_click_script = r#"(() => {
            const key = __KEY__;
            const entry = Array.from(document.querySelectorAll(
                '.search-history-list-item .history-item-content'
            )).find((el) => (el.textContent || '').trim() === key);
            if (!entry) return false;
            const rect = entry.getBoundingClientRect();
            if (rect.width <= 0 || rect.height <= 0) return false;
            entry.click();
            return true;
        })()"#
        .replace("__KEY__", &json_string(key));
        let clicked: bool = client.evaluate(&history_click_script).unwrap_or(false);
        if clicked {
            tracing::info!(
                "[OTP-FEISHU][SEARCH] clicked search-history entry for key '{}'",
                key
            );
            thread::sleep(Duration::from_millis(500));
            let candidates: Vec<BotCandidate> = client.evaluate(&candidates_expression)?;
            if let Ok(point) = unique_exact_bot_candidate(&candidates, bot_name) {
                bot_point = Some(point);
                tracing::info!(
                    "[OTP-FEISHU][SEARCH] history entry for key '{}' reached exact bot candidate",
                    key
                );
            }
        }
    }

    let bot_point = bot_point.ok_or_else(|| {
        AutomationFailure::dom("未找到名称完全匹配且标记为机器人的飞书联系人。")
    })?;
    client.click(bot_point)?;
    tracing::info!("[OTP-FEISHU] bot result card clicked");

    let expected_placeholder = format!("发送给 {bot_name}");
    let composer_deadline = Instant::now() + DOM_WAIT_TIMEOUT;
    let composer = loop {
        let state: ComposerState = client.evaluate(
            r#"(() => {
                const editor = document.querySelector('[contenteditable=true].innerdocbody');
                const chatName = (document.querySelector('.chatWindow_chatName')?.textContent || '').trim();
                if (!editor) return { chat_name: chatName, placeholder: '', center: null };
                const rect = editor.getBoundingClientRect();
                const placeholder = editor.getAttribute('placeholder')
                    || editor.getAttribute('data-placeholder')
                    || editor.getAttribute('aria-placeholder')
                    || (editor.querySelector('.editor__custom--placeholder-content')?.textContent || '').trim()
                    || '';
                return {
                    chat_name: chatName,
                    placeholder,
                    center: rect.width > 0 && rect.height > 0
                        ? { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 }
                        : null
                };
            })()"#,
        )?;
        if state.chat_name == bot_name
            && state.placeholder == expected_placeholder
            && state.center.is_some()
        {
            break state;
        }
        if Instant::now() >= composer_deadline {
            return Err(AutomationFailure::dom(
                "未能确认名称和发送框均匹配目标机器人。",
            ));
        }
        thread::sleep(AUTOMATION_POLL_INTERVAL);
    };

    let baseline = message_snapshots(&mut client)?
        .into_iter()
        .map(|message| message.id)
        .collect::<HashSet<_>>();
    client.click(composer.center.expect("composer center checked"))?;
    type_into_editor(
        &mut client,
        &composer_finder(),
        request_text,
        "发送框内容校验失败，未发送请求。",
    )?;
    let chat_name: String = client.evaluate(
        r#"(() => (document.querySelector('.chatWindow_chatName')?.textContent || '').trim())()"#,
    )?;
    if chat_name != bot_name {
        return Err(AutomationFailure::dom(
            "发送前目标会话已变化，未发送动态口令。",
        ));
    }
    // Keep the tty-owned approval immediately adjacent to Enter. Any slower
    // DOM/CDP verification above must complete before the UI validates that
    // this exact OTP cycle still owns the current `2nd Password:` prompt.
    approve_send()?;
    client.press_enter()?;

    let reply_deadline = Instant::now() + OTP_REPLY_TIMEOUT;
    let mut sent_message_id: Option<String> = None;
    loop {
        let snapshots = message_snapshots(&mut client)?;
        if sent_message_id.is_none() {
            sent_message_id = snapshots
                .iter()
                .find(|message| {
                    !baseline.contains(&message.id)
                        && message.is_self
                        && message.body == request_text
                })
                .map(|message| message.id.clone());
        }
        if let Some(sent_id) = sent_message_id.as_deref()
            && let Some(sent_index) = snapshots.iter().position(|message| message.id == sent_id)
            && let Some(reply) = snapshots.iter().skip(sent_index + 1).find(|message| {
                !baseline.contains(&message.id)
                    && !message.is_self
                    && !message.body.is_empty()
                    && rusterm_ssh::feishu_otp::parse_otp_reply(&message.body, code_pattern)
                        .is_some()
            })
        {
            return Ok(reply.body.clone());
        }
        if Instant::now() >= reply_deadline {
            let reason = if sent_message_id.is_some() {
                "等待机器人新回复超时。"
            } else {
                "未确认飞书消息发送成功。"
            };
            return Err(AutomationFailure::dom(reason));
        }
        thread::sleep(AUTOMATION_POLL_INTERVAL);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::feishu_oauth_flow::FEISHU_WEB_LOGIN_URL;

    fn target(id: &str, url: &str) -> CdpTarget {
        CdpTarget {
            id: id.to_string(),
            url: url.to_string(),
            target_type: "page".to_string(),
            web_socket_debugger_url: format!("ws://127.0.0.1:9222/devtools/page/{id}"),
        }
    }

    #[test]
    fn chrome_is_preferred_when_both_browsers_are_installed() {
        let selected = select_browser_from_paths(
            Some(PathBuf::from("/browser/chrome")),
            Some(PathBuf::from("/browser/edge")),
        )
        .unwrap();
        assert_eq!(selected.0, SupportedBrowser::Chrome);
        assert_eq!(selected.1, PathBuf::from("/browser/chrome"));
    }

    #[test]
    fn edge_is_used_when_chrome_is_unavailable() {
        let selected =
            select_browser_from_paths(None, Some(PathBuf::from("/browser/edge"))).unwrap();
        assert_eq!(selected.0, SupportedBrowser::Edge);
        assert_eq!(selected.1, PathBuf::from("/browser/edge"));
    }

    #[test]
    fn missing_supported_browser_has_an_actionable_error() {
        let error = select_browser_from_paths(None, None).unwrap_err();
        assert!(error.contains("Google Chrome"));
        assert!(error.contains("Microsoft Edge"));
    }

    #[test]
    fn launch_plan_uses_https_and_a_rusterm_owned_profile() {
        let data_dir = Path::new("/application-support");
        let plan = BrowserLaunchPlan::new(
            SupportedBrowser::Chrome,
            PathBuf::from("/browser/chrome"),
            data_dir,
            FEISHU_WEB_LOGIN_URL.to_string(),
        )
        .unwrap();
        assert!(plan.url.starts_with("https://"));
        assert_eq!(
            plan.profile_dir,
            data_dir.join("rusterm/feishu-browser/chrome")
        );
        assert!(plan.arguments().iter().any(|argument| {
            argument
                .to_string_lossy()
                .starts_with("--user-data-dir=/application-support/rusterm/feishu-browser/chrome")
        }));
        assert!(
            plan.arguments()
                .contains(&OsString::from("--remote-debugging-port=0"))
        );
        assert!(
            plan.arguments()
                .contains(&OsString::from("--no-startup-window"))
        );
        assert!(!plan.arguments().contains(&OsString::from("--new-window")));
        assert!(
            plan.arguments()
                .contains(&OsString::from("--remote-allow-origins=*"))
        );
        assert!(!plan.arguments().contains(&OsString::from(&plan.url)));
    }

    #[test]
    fn launch_plan_rejects_non_feishu_and_non_https_urls() {
        assert!(
            BrowserLaunchPlan::new(
                SupportedBrowser::Chrome,
                PathBuf::from("/browser/chrome"),
                Path::new("/data"),
                "http://www.feishu.cn/messenger/".to_string(),
            )
            .is_err()
        );
        assert!(
            BrowserLaunchPlan::new(
                SupportedBrowser::Chrome,
                PathBuf::from("/browser/chrome"),
                Path::new("/data"),
                "https://evil.example/next/messenger/".to_string(),
            )
            .is_err()
        );
    }

    #[test]
    fn devtools_active_port_uses_the_first_line() {
        assert_eq!(
            parse_devtools_port("43127\n/devtools/browser/example\n"),
            Some(43127)
        );
        assert_eq!(parse_devtools_port("invalid\n"), None);
    }

    #[test]
    fn target_selection_prefers_logged_in_page_and_deduplicates_feishu_pages() {
        let targets = vec![
            target(
                "oauth",
                "https://accounts.feishu.cn/open-apis/authen/v1/authorize",
            ),
            target("other", "https://example.com/"),
            target("logged-in", "https://tenant.feishu.cn/next/messenger/"),
            target("login", "https://www.feishu.cn/messenger/"),
        ];
        let (keep_index, mut duplicate_ids) = select_feishu_target(&targets).unwrap();
        duplicate_ids.sort();
        assert_eq!(keep_index, 2);
        assert_eq!(
            duplicate_ids,
            vec!["login".to_string(), "oauth".to_string()]
        );
    }

    #[test]
    fn target_selection_uses_first_allowed_page_without_logged_in_page() {
        let targets = vec![
            target("first", "https://accounts.feishu.cn/accounts/page/login"),
            target("second", "https://www.feishu.cn/messenger/"),
        ];
        let (keep_index, duplicate_ids) = select_feishu_target(&targets).unwrap();
        assert_eq!(keep_index, 0);
        assert_eq!(duplicate_ids, vec!["second".to_string()]);
    }

    #[test]
    fn strict_percent_encoding_encodes_url_delimiters_and_utf8() {
        assert_eq!(
            percent_encode_url("https://www.feishu.cn/messenger/?a=1&name=智小安"),
            "https%3A%2F%2Fwww.feishu.cn%2Fmessenger%2F%3Fa%3D1%26name%3D%E6%99%BA%E5%B0%8F%E5%AE%89"
        );
        assert_eq!(percent_encode_url("AZaz09-._~"), "AZaz09-._~");
    }

    #[test]
    fn search_keys_trim_dedupe_and_fall_back_to_bot_name() {
        let keys = vec!["zxa".to_string(), "智小安".to_string()];
        assert_eq!(
            build_search_keys("智小安", &keys),
            vec!["zxa".to_string(), "智小安".to_string()]
        );

        // Whitespace is trimmed; duplicates are dropped; empties are skipped.
        let keys = vec![
            "  zxa  ".to_string(),
            "zxa".to_string(),
            "".to_string(),
            "   ".to_string(),
            "智小安".to_string(),
        ];
        assert_eq!(
            build_search_keys("智小安", &keys),
            vec!["zxa".to_string(), "智小安".to_string()]
        );

        // An empty list falls back to the bot name so the bot can still be
        // found by its literal name.
        let empty: Vec<String> = Vec::new();
        assert_eq!(
            build_search_keys("智小安", &empty),
            vec!["智小安".to_string()]
        );

        // All-empty entries also fall back.
        let keys = vec!["  ".to_string(), "".to_string()];
        assert_eq!(
            build_search_keys("智小安", &keys),
            vec!["智小安".to_string()]
        );
    }

    #[test]
    fn exact_bot_candidate_must_be_unique_and_robot_labeled() {
        let exact = BotCandidate {
            name: "智小安".to_string(),
            has_robot_label: true,
            center: Some(Point { x: 10.0, y: 20.0 }),
        };
        let prefixed = BotCandidate {
            name: "OTP-智小安".to_string(),
            has_robot_label: true,
            center: Some(Point { x: 30.0, y: 40.0 }),
        };
        let non_robot = BotCandidate {
            name: "智小安".to_string(),
            has_robot_label: false,
            center: Some(Point { x: 50.0, y: 60.0 }),
        };
        assert_eq!(
            unique_exact_bot_candidate(&[prefixed.clone(), exact.clone(), non_robot], "智小安"),
            Ok(Point { x: 10.0, y: 20.0 })
        );
        assert!(unique_exact_bot_candidate(&[prefixed], "智小安").is_err());
        assert!(unique_exact_bot_candidate(&[exact.clone(), exact.clone()], "智小安").is_err());
        let hidden_duplicate = BotCandidate {
            center: None,
            ..exact.clone()
        };
        assert!(unique_exact_bot_candidate(&[exact, hidden_duplicate], "智小安").is_err());
    }

    #[test]
    fn logged_in_messenger_is_not_navigated_for_generic_messenger_request() {
        assert!(!should_navigate_reused_target(
            "https://tenant.feishu.cn/next/messenger/",
            "https://www.feishu.cn/messenger/"
        ));
        assert!(should_navigate_reused_target(
            "https://tenant.feishu.cn/next/messenger/",
            "https://accounts.feishu.cn/open-apis/authen/v1/authorize"
        ));
        assert!(should_navigate_reused_target(
            "https://accounts.feishu.cn/accounts/page/login",
            "https://www.feishu.cn/messenger/"
        ));
    }

    #[test]
    fn only_tenant_messenger_urls_prove_login() {
        assert!(looks_like_logged_in_feishu_url(
            "https://tenant.feishu.cn/next/messenger/"
        ));
        assert!(looks_like_logged_in_feishu_url(
            "https://tenant.larksuite.com/next/messenger/"
        ));
        assert!(!looks_like_logged_in_feishu_url(FEISHU_WEB_LOGIN_URL));
        assert!(!looks_like_logged_in_feishu_url(
            "https://accounts.feishu.cn/accounts/page/login"
        ));
        assert!(!looks_like_logged_in_feishu_url(
            "https://evil.example/next/messenger/"
        ));
        assert!(!looks_like_logged_in_feishu_url(
            "https://evil.example/?next=.feishu.cn/next/messenger/"
        ));
        assert!(!looks_like_logged_in_feishu_url(
            "https://evil.example@tenant.feishu.cn/next/messenger/"
        ));
    }

    #[test]
    fn parse_http_head_extracts_status_and_length() {
        let (status, length) = parse_http_head(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=UTF-8\r\nContent-Length: 110\r\n\r\n",
        )
        .unwrap();
        assert_eq!(status, 200);
        assert_eq!(length, Some(110));

        let (status, length) =
            parse_http_head("HTTP/1.1 200 OK\r\ncOnTeNt-LeNgTh: 42\r\n\r\n").unwrap();
        assert_eq!(status, 200);
        assert_eq!(length, Some(42));

        let (status, length) = parse_http_head("HTTP/1.1 404 Not Found\r\n\r\n").unwrap();
        assert_eq!(status, 404);
        assert_eq!(length, None);
    }

    #[test]
    fn parse_http_head_rejects_missing_status() {
        assert!(parse_http_head("this is not HTTP\r\n\r\n").is_err());
        assert!(parse_http_head("").is_err());
        assert!(parse_http_head("HTTP/1.1 not-a-number\r\n\r\n").is_err());
    }

    /// Yields `payload` in chunk-sized reads; once drained, either errors
    /// (`past_end_error`) or reports a clean EOF. The erroring variant proves
    /// a Content-Length response completes without ever awaiting EOF.
    struct ChunkedReader {
        payload: std::io::Cursor<Vec<u8>>,
        chunk: usize,
        error_past_end: bool,
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.payload.position() == self.payload.get_ref().len() as u64 {
                if self.error_past_end {
                    return Err(std::io::Error::other(
                        "response reader must not be polled past Content-Length",
                    ));
                }
                return Ok(0);
            }
            let limit = buffer.len().min(self.chunk);
            self.payload.read(&mut buffer[..limit])
        }
    }

    #[test]
    fn cdp_http_reads_content_length_body_without_eof() {
        let body = br#"[{"id":"1","type":"page"}]"#;
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=UTF-8\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut payload = head.into_bytes();
        payload.extend_from_slice(body);
        let mut reader = ChunkedReader {
            payload: std::io::Cursor::new(payload),
            chunk: usize::MAX,
            error_past_end: true,
        };
        let (status, received) = read_http_response(&mut reader).unwrap();
        assert_eq!(status, 200);
        assert_eq!(received, body);
    }

    #[test]
    fn cdp_http_reads_head_and_body_across_small_reads() {
        let body = b"{\"result\":[]}";
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
        let mut payload = head.into_bytes();
        payload.extend_from_slice(body);
        let mut reader = ChunkedReader {
            payload: std::io::Cursor::new(payload),
            chunk: 3,
            error_past_end: true,
        };
        let (status, received) = read_http_response(&mut reader).unwrap();
        assert_eq!(status, 200);
        assert_eq!(received, body);
    }

    #[test]
    fn cdp_http_reports_unexpected_eof_mid_body() {
        let payload = b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\n\r\nshort".to_vec();
        let mut reader = ChunkedReader {
            payload: std::io::Cursor::new(payload),
            chunk: usize::MAX,
            error_past_end: false,
        };
        let error = read_http_response(&mut reader).unwrap_err();
        assert!(error.contains("unexpected EOF"));
    }

    #[test]
    fn cdp_http_falls_back_to_eof_without_content_length() {
        let payload = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nTarget closed".to_vec();
        let mut reader = std::io::Cursor::new(payload);
        let (status, body) = read_http_response(&mut reader).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"Target closed");
    }
}
