use rusterm_core::logging::init_logging;
use rusterm_core::window_state::WindowState;

use base64::Engine as _;

/// Original SVG copied by `build.rs` and embedded in the executable. Keeping
/// this separate from the rasterized PNG lets the WebView load an actual SVG
/// at runtime without relying on a filesystem-relative asset path.
const APP_ICON_SVG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/assets/gemini-svg.svg"));
const APP_ICON_PNG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/icon.png"));

fn app_icon_svg_data_url() -> String {
    format!(
        "data:image/svg+xml;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(APP_ICON_SVG)
    )
}

#[cfg(target_os = "macos")]
fn install_macos_application_icon(icon_bytes: &[u8]) -> Result<(), &'static str> {
    use objc2::{AllocAnyThread as _, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let main_thread = MainThreadMarker::new().ok_or("window callback is not on the main thread")?;
    let data = NSData::with_bytes(icon_bytes);
    let image = NSImage::initWithData(NSImage::alloc(), &data)
        .ok_or("AppKit could not decode the embedded PNG")?;
    let application = NSApplication::sharedApplication(main_thread);

    // SAFETY: AppKit requires a valid NSImage and main-thread access. Both are
    // guaranteed above; NSApplication retains the image after this call.
    unsafe { application.setApplicationIconImage(Some(&image)) };
    Ok(())
}

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

    let svg_icon_url = app_icon_svg_data_url();
    let head_html = [
        r#"<meta name="viewport" content="width=device-width,initial-scale=1.0">
<link rel="icon" type="image/svg+xml" href=""#,
        svg_icon_url.as_str(),
        r#"">
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
.sug-row:hover .sug-del{color:#9ece6a !important;}
.sug-row .sug-del:hover{color:#f7768e !important;}
</style>"#,
    ]
    .concat();

    let mut cfg = dioxus::desktop::Config::new()
        .with_window(window)
        .with_background_color((26, 27, 38, 255))
        .with_custom_head(head_html)
        // Close behaviour: the window HIDES (not closes) when the user clicks
        // the close button. This lets the App component intercept the
        // `CloseRequested` wry event via `use_wry_event_handler` and show the
        // "是否确实要关闭本软件？" confirmation dialog. The dialog's "确认"
        // button flips the close behaviour to `WindowCloses` (via
        // `DesktopContext::set_close_behavior`) and calls `window.close()` to
        // actually exit; "取消" just hides the dialog (the window has already
        // been hidden by dioxus, so a `use_future` re-shows it). When the user
        // has previously unchecked "下次关闭时不再询问" + picked "确认", the
        // wry handler flips the behaviour to `WindowCloses` directly so the
        // app exits without showing the dialog.
        .with_close_behaviour(dioxus::desktop::WindowCloseBehaviour::WindowHides);

    // Window icon. The PNG embedded below is rasterized at build time
    // from `assets/gemini-svg.svg` by `build.rs` (using resvg/usvg/tiny-skia).
    // We embed the generated PNG via `include_bytes!` so the icon ships
    // inside the `rusterm` binary — no external file at runtime.
    //
    // Failure here is non-fatal — the app still launches, just with the
    // platform-default window icon — so we log and continue instead of
    // panicking.
    match dioxus::desktop::icon_from_memory::<dioxus::desktop::tao::window::Icon>(APP_ICON_PNG) {
        Ok(icon) => {
            cfg = cfg.with_icon(icon);
        }
        Err(e) => {
            tracing::warn!("failed to decode embedded window icon: {e}");
        }
    }

    // Tao intentionally ignores window icons on macOS because NSWindow has no
    // icon API. Set NSApplication's icon after Tao creates the native window so
    // `cargo run` and bare debug binaries also get the correct Dock icon.
    #[cfg(target_os = "macos")]
    {
        cfg = cfg.with_on_window(|_, _| match install_macos_application_icon(APP_ICON_PNG) {
            Ok(()) => tracing::info!("installed embedded macOS application icon"),
            Err(error) => tracing::warn!("failed to install macOS application icon: {error}"),
        });
    }

    dioxus::LaunchBuilder::new()
        .with_cfg(cfg)
        .launch(rusterm_ui::App);
}

#[cfg(test)]
mod tests {
    use super::{APP_ICON_PNG, APP_ICON_SVG, app_icon_svg_data_url};
    use base64::Engine as _;

    #[test]
    fn embedded_svg_is_available_to_the_webview_at_runtime() {
        let svg = std::str::from_utf8(APP_ICON_SVG).expect("embedded icon must be UTF-8 SVG");
        assert!(svg.contains("<svg"));
        assert!(svg.contains("viewBox=\"0 0 512 512\""));

        let url = app_icon_svg_data_url();
        let encoded = url
            .strip_prefix("data:image/svg+xml;base64,")
            .expect("SVG must be exposed as a data URL");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("SVG data URL must contain valid base64");
        assert_eq!(decoded, APP_ICON_SVG);
    }

    #[test]
    fn embedded_native_icon_is_a_decodable_png() {
        let icon =
            dioxus::desktop::icon_from_memory::<dioxus::desktop::tao::window::Icon>(APP_ICON_PNG);
        assert!(icon.is_ok(), "embedded native icon must be a valid PNG");
    }
}
