//! Native Feishu browser window for QR sign-in and browser-session reuse.
//!
//! Dioxus desktop's own WebView wrapper intentionally intercepts every
//! top-level `http`/`https` navigation and opens it in the system browser.
//! Consequently a Dioxus `new_window` followed by `load_url(feishu_url)` can
//! never display Feishu inside that child window. This module instead queues
//! browser commands and executes them from Dioxus' tao event-loop hook, where
//! it can create a plain Wry WebView whose navigation remains embedded.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use dioxus::desktop::tao::dpi::LogicalSize;
use dioxus::desktop::tao::event::{Event, WindowEvent};
use dioxus::desktop::tao::event_loop::EventLoopWindowTarget;
use dioxus::desktop::tao::window::{Window, WindowBuilder};
use dioxus::desktop::wry::{WebView, WebViewBuilder};

use crate::feishu_oauth_flow::FEISHU_WEB_LOGIN_URL;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeishuBrowserEvent {
    Navigating { url: String },
    LoggedIn { url: String },
    Failed { reason: String },
    Closed,
}

enum FeishuBrowserCommand {
    Open(String),
    Hide,
    Close,
}

struct NativeFeishuBrowser {
    window: Window,
    webview: WebView,
}

thread_local! {
    static COMMANDS: RefCell<VecDeque<FeishuBrowserCommand>> = const { RefCell::new(VecDeque::new()) };
    static BROWSER: RefCell<Option<NativeFeishuBrowser>> = const { RefCell::new(None) };
}

static EVENTS: OnceLock<Mutex<VecDeque<FeishuBrowserEvent>>> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static LOGGED_IN: AtomicBool = AtomicBool::new(false);

fn events() -> &'static Mutex<VecDeque<FeishuBrowserEvent>> {
    EVENTS.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn publish(event: FeishuBrowserEvent) {
    if let Ok(mut queue) = events().lock() {
        queue.push_back(event);
    }
}

/// The Feishu Web messenger URL proves that the browser has an authenticated
/// user session. Login/account pages and the generic messenger entry do not.
pub fn looks_like_logged_in_feishu_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("https://")
        && (lower.contains(".feishu.cn/next/messenger")
            || lower.contains(".larksuite.com/next/messenger"))
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

/// Request an embedded navigation. The actual native window is created by
/// [`handle_feishu_browser_event`] on tao's main event-loop thread.
pub fn open_feishu_login_window(url: String) {
    if url.trim().is_empty() {
        return;
    }
    ACTIVE.store(true, Ordering::Release);
    COMMANDS.with(|queue| {
        queue
            .borrow_mut()
            .push_back(FeishuBrowserCommand::Open(url))
    });
    // Wake tao so its custom event handler drains the command promptly.
    dioxus::desktop::window().request_redraw();
}

pub fn hide_feishu_login_window() {
    COMMANDS.with(|queue| queue.borrow_mut().push_back(FeishuBrowserCommand::Hide));
    dioxus::desktop::window().request_redraw();
}

pub fn close_feishu_login_window() {
    COMMANDS.with(|queue| queue.borrow_mut().push_back(FeishuBrowserCommand::Close));
    dioxus::desktop::window().request_redraw();
}

fn open_native_browser<T: 'static>(target: &EventLoopWindowTarget<T>, url: String) {
    let reused = BROWSER.with(|slot| {
        let mut browser = slot.borrow_mut();
        let Some(browser) = browser.as_mut() else {
            return false;
        };
        browser.window.set_visible(true);
        browser.window.set_focus();
        match browser.webview.load_url(&url) {
            Ok(()) => {
                tracing::info!("[OTP-FEISHU] native browser re-navigated inside embedded WebView");
                true
            }
            Err(error) => {
                let reason = format!("飞书页面加载失败：{error}");
                tracing::warn!("[OTP-FEISHU] native browser re-navigation failed: {error}");
                publish(FeishuBrowserEvent::Failed { reason });
                false
            }
        }
    });
    if reused {
        return;
    }

    let window = match WindowBuilder::new()
        .with_title(crate::i18n::t("feishu.browser_title"))
        .with_inner_size(LogicalSize::new(480.0, 700.0))
        .with_min_inner_size(LogicalSize::new(360.0, 480.0))
        .with_resizable(true)
        .with_always_on_top(true)
        .build(target)
    {
        Ok(window) => window,
        Err(error) => {
            let reason = format!("飞书窗口创建失败：{error}");
            tracing::warn!("[OTP-FEISHU] native browser window creation failed: {error}");
            ACTIVE.store(false, Ordering::Release);
            publish(FeishuBrowserEvent::Failed { reason });
            return;
        }
    };

    let webview = match WebViewBuilder::new()
        .with_url(&url)
        .with_back_forward_navigation_gestures(true)
        .with_navigation_handler(|url| {
            let logged_in = looks_like_logged_in_feishu_url(&url);
            if logged_in {
                LOGGED_IN.store(true, Ordering::Release);
                tracing::info!("[OTP-FEISHU] Feishu Web session authenticated");
                publish(FeishuBrowserEvent::LoggedIn { url });
            } else {
                publish(FeishuBrowserEvent::Navigating { url });
            }
            true
        })
        .build(&window)
    {
        Ok(webview) => webview,
        Err(error) => {
            let reason = format!("飞书浏览器创建失败：{error}");
            tracing::warn!("[OTP-FEISHU] native Wry WebView creation failed: {error}");
            ACTIVE.store(false, Ordering::Release);
            publish(FeishuBrowserEvent::Failed { reason });
            return;
        }
    };

    tracing::info!(
        "[OTP-FEISHU] native embedded browser open — navigating to {}",
        if url == FEISHU_WEB_LOGIN_URL {
            "Feishu Web login"
        } else {
            "Feishu OAuth"
        }
    );
    BROWSER.with(|slot| *slot.borrow_mut() = Some(NativeFeishuBrowser { window, webview }));
}

/// Dioxus application event hook. `rusterm-app` installs this on its desktop
/// config so all native-window work stays on the platform UI thread.
pub fn handle_feishu_browser_event<T: 'static>(
    event: &Event<'_, T>,
    target: &EventLoopWindowTarget<T>,
) {
    if let Event::WindowEvent {
        window_id, event, ..
    } = event
    {
        let owns_window = BROWSER.with(|slot| {
            slot.borrow()
                .as_ref()
                .is_some_and(|browser| browser.window.id() == *window_id)
        });
        if owns_window && matches!(event, WindowEvent::CloseRequested | WindowEvent::Destroyed) {
            BROWSER.with(|slot| slot.borrow_mut().take());
            ACTIVE.store(false, Ordering::Release);
            publish(FeishuBrowserEvent::Closed);
        }
    }

    let commands = COMMANDS.with(|queue| queue.borrow_mut().drain(..).collect::<Vec<_>>());
    for command in commands {
        match command {
            FeishuBrowserCommand::Open(url) => open_native_browser(target, url),
            FeishuBrowserCommand::Hide => {
                BROWSER.with(|slot| {
                    if let Some(browser) = slot.borrow().as_ref() {
                        browser.window.set_visible(false);
                    }
                });
            }
            FeishuBrowserCommand::Close => {
                BROWSER.with(|slot| slot.borrow_mut().take());
                publish(FeishuBrowserEvent::Closed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
