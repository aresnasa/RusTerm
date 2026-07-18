use rusterm_core::logging::init_logging;
use rusterm_core::window_state::WindowState;

fn main() {
    // Initialize logging. The returned guard MUST live for the entire process
    // lifetime — dropping it early would flush-and-close the non-blocking
    // writer, dropping any in-flight log events on shutdown.
    //
    // What gets logged: app lifecycle, session lifecycle (by opaque ID),
    // connection outcomes, performance counters, crash-level errors.
    // What NEVER gets logged: terminal I/O, credentials, encryption keys,
    // user info, hostnames, home-directory paths. See
    // `crates/rusterm-core/src/logging.rs` and `docs/LOGGING.md`.
    let _log_guard = init_logging();

    tracing::info!("RusTerm starting version={}", env!("CARGO_PKG_VERSION"));
    tracing::info!("log_dir={}", rusterm_core::logging::log_dir().display());

    // --- Window state persistence ---
    // Load the user's last window geometry (size + position + maximized) so
    // the window opens where they left it. The state is stored as a small JSON
    // file (`window_state.json`) in the same directory as settings.json,
    // deliberately NOT encrypted (so the window can restore correctly even
    // before the master password is entered). On first launch (no state file)
    // we fall back to 1200x800.
    //
    // The live window state is polled + persisted by the App component's
    // use_future (see rusterm_ui::App), so here we just restore the saved
    // geometry — the save loop is on the UI side.
    let saved_state = WindowState::load();
    let (saved_width, saved_height, saved_maximized) = match &saved_state {
        Some(s) => {
            tracing::info!(
                "Restoring window state: {}x{} @ ({},{}) maximized={}",
                s.width,
                s.height,
                s.x,
                s.y,
                s.maximized
            );
            (s.width, s.height, s.maximized)
        }
        None => {
            tracing::info!("No saved window state, using defaults (1200x800)");
            (1200.0, 800.0, false)
        }
    };

    let mut window = dioxus::desktop::WindowBuilder::new()
        .with_title("RusTerm")
        .with_min_inner_size(dioxus::desktop::LogicalSize::new(600.0, 400.0))
        .with_resizable(true)
        .with_maximizable(true);

    // Apply saved size if we have one. Otherwise use the 1200x800 default.
    if saved_state.as_ref().is_some_and(|s| s.has_size()) {
        window =
            window.with_inner_size(dioxus::desktop::LogicalSize::new(saved_width, saved_height));
    } else {
        window = window.with_inner_size(dioxus::desktop::LogicalSize::new(1200.0, 800.0));
    }

    // Apply saved position if we have one. We skip this on macOS because the
    // window manager controls window placement and the menu bar can overlap —
    // letting tao pick the default position is safer there.
    #[cfg(not(target_os = "macos"))]
    if let Some(s) = saved_state.as_ref() {
        if s.has_position() {
            window = window.with_position(dioxus::desktop::LogicalPosition::new(s.x, s.y));
        }
    }

    // If the user last closed the app maximized, re-maximize on launch.
    if saved_maximized {
        window = window.with_maximized(true);
    }

    let head_html = r#"<meta name="viewport" content="width=device-width,initial-scale=1.0">
<style>
html,body{margin:0;padding:0;width:100%;height:100%;overflow:hidden;background:#1a1b26;}
#main{width:100%;height:100%;overflow:hidden;}
.conn-item:hover{background:#24283b!important;}
.conn-item{cursor:pointer;transition:background 0.1s;}
*:focus{outline:none !important;box-shadow:none !important;}
*[tabindex]:focus{outline:none !important;box-shadow:none !important;}
::selection{background:rgba(122,162,247,0.3);color:#c0caf5;}
::-moz-selection{background:rgba(122,162,247,0.3);color:#c0caf5;}
*[id^="terminal-input-"]::-webkit-scrollbar{display:none;}
</style>"#;

    let cfg = dioxus::desktop::Config::new()
        .with_window(window)
        .with_background_color((26, 27, 38, 255))
        .with_custom_head(head_html.to_string());

    dioxus::LaunchBuilder::new()
        .with_cfg(cfg)
        .launch(rusterm_ui::App);
}
