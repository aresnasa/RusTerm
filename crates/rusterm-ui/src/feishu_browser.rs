//! Minimal embedded browser window for the Feishu sign-in (issue #130).
//!
//! Why an embedded browser instead of a self-rendered QR code: the OAuth
//! redirect must land on THIS machine's loopback listener
//! (`http://127.0.0.1:8878/...`). When the authorize URL itself is rendered
//! as a QR code and scanned, the whole flow — login AND redirect — happens
//! on the phone, where no listener exists, so the desktop never receives the
//! code. Instead we open a second wry WebView window that navigates to the
//! Feishu authorize page: Feishu renders its official QR there, the phone
//! scan authorizes the *desktop* browser context, and the redirect reaches
//! the loopback listener. The WebView's persistent cookie store also keeps
//! the Feishu session alive across app launches, so subsequent sign-ins
//! complete without re-scanning.
//!
//! All functions here must be called from within the Dioxus runtime (they
//! use `dioxus::desktop::window()` and the single-threaded executor); the
//! window handle is kept in a thread-local `Weak` so a user-closed window is
//! observed naturally (upgrade fails) without extra bookkeeping.

use std::cell::RefCell;
use std::rc::Rc;

use dioxus::desktop::{
    Config, LogicalSize, WeakDesktopContext, WindowBuilder, WindowCloseBehaviour,
};
use dioxus::prelude::*;

thread_local! {
    /// The one live sign-in window (if any). Weak so closing the window via
    /// its close button drops the `DesktopService` naturally.
    static LOGIN_WINDOW: RefCell<Option<WeakDesktopContext>> = const { RefCell::new(None) };
}

/// Placeholder rendered for the instant before `load_url` swaps the WebView
/// over to the Feishu accounts page.
#[component]
fn FeishuBrowserLoading() -> Element {
    rsx! {
        div {
            style: "
                display: flex; align-items: center; justify-content: center;
                width: 100vw; height: 100vh;
                background: #1a1b26; color: #787c99;
                font-family: 'Segoe UI', system-ui, sans-serif; font-size: 13px;
            ",
            { crate::i18n::t("feishu.browser_loading") }
        }
    }
}

/// Open (or re-focus + re-navigate) the embedded Feishu sign-in window and
/// point it at `url` (the OAuth authorize URL). No-op for an empty URL.
pub fn open_feishu_login_window(url: String) {
    if url.is_empty() {
        return;
    }

    // Reuse a still-open window: just re-navigate it. This is what "重新扫码"
    // hits after the nonce is rotated.
    let existing = LOGIN_WINDOW.with(|w| w.borrow().clone());
    if let Some(ctx) = existing.and_then(|weak| weak.upgrade()) {
        ctx.window.set_focus();
        match ctx.webview.load_url(&url) {
            Ok(()) => {
                tracing::info!("[OTP-FEISHU] embedded login window re-navigated");
                return;
            }
            Err(e) => {
                tracing::warn!("[OTP-FEISHU] embedded window re-navigation failed: {e}");
                ctx.close();
            }
        }
    }

    let window = WindowBuilder::new()
        .with_title(crate::i18n::t("feishu.browser_title"))
        .with_inner_size(LogicalSize::new(480.0, 700.0))
        .with_min_inner_size(LogicalSize::new(360.0, 480.0))
        .with_resizable(true)
        .with_always_on_top(true);
    let cfg = Config::new()
        .with_window(window)
        .with_background_color((26, 27, 38, 255))
        // The child window must actually close — the main window's
        // hide-on-close behaviour would leave zombie sign-in windows around.
        .with_close_behaviour(WindowCloseBehaviour::WindowCloses);

    let pending = dioxus::desktop::window().new_window(VirtualDom::new(FeishuBrowserLoading), cfg);
    spawn(async move {
        let Ok(ctx) = pending.try_resolve().await else {
            tracing::warn!("[OTP-FEISHU] embedded login window creation was cancelled");
            return;
        };
        match ctx.webview.load_url(&url) {
            Ok(()) => tracing::info!(
                "[OTP-FEISHU] embedded login window open — navigating to Feishu authorize page"
            ),
            Err(e) => tracing::warn!("[OTP-FEISHU] embedded window load_url failed: {e}"),
        }
        LOGIN_WINDOW.with(|w| *w.borrow_mut() = Some(Rc::downgrade(&ctx)));
    });
}

/// Close the embedded sign-in window if it is still open. Called once the
/// OAuth callback has been delivered (success or failure — the outcome is
/// reported by the main-window popup) and when the user cancels the flow.
pub fn close_feishu_login_window() {
    let taken = LOGIN_WINDOW.with(|w| w.borrow_mut().take());
    if let Some(ctx) = taken.and_then(|weak| weak.upgrade()) {
        ctx.close();
        tracing::info!("[OTP-FEISHU] embedded login window closed");
    }
}
