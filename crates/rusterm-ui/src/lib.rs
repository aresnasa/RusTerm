pub mod analytics;
pub mod app;
pub mod command_correction;
pub mod comparison;
pub mod components;
pub mod feishu_browser;
pub mod feishu_oauth_flow;
pub mod feishu_oauth_listener;
pub mod i18n;
pub mod keybindings;
pub mod layout;
pub mod layout_state;
pub mod relay_tunnel;
pub mod skin;
pub mod state;
pub mod transfers;
pub mod zmodem;

pub use app::App;

/// Global [`AppState`] signal — mirrors the unlocked runtime state.
pub static APP_STATE: dioxus::prelude::GlobalSignal<state::AppState> =
    dioxus::prelude::GlobalSignal::new(state::AppState::default);

/// App-wide terminal input senders — session id → keyed pad byte queue.
/// Exposed here so help routines (Feishu OTP fan-out, relay) can pick a
/// sender without prop-drilling the component-local `Signal<HashMap<…>>`
/// through every helper.
pub static INPUT_SENDERS: dioxus::prelude::GlobalSignal<
    std::collections::HashMap<String, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
> = dioxus::prelude::GlobalSignal::new(std::collections::HashMap::new);

/// One-shot signal toggled by the settings dialog's "扫码登录" button.
/// The App component polls this and start/stops the OAuth flow; it then
/// resets it to `false` so the next click re-fires.
pub static FEISHU_AUTH_REQUESTED: dioxus::prelude::GlobalSignal<bool> =
    dioxus::prelude::GlobalSignal::new(|| false);
