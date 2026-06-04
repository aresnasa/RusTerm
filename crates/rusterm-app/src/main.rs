use std::path::PathBuf;

use tracing;
use tracing_subscriber::EnvFilter;

fn main() {
    // Initialize logging
    let log_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rusterm")
        .join("logs");
    std::fs::create_dir_all(&log_dir).ok();

    let file_appender = tracing_appender::rolling::daily(&log_dir, "rusterm.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("rusterm=debug".parse().unwrap()))
        .with_writer(non_blocking)
        .with_ansi(false)
        .json()
        .init();

    tracing::info!("RusTerm starting...");

    // Launch Dioxus desktop app with proper window configuration
    let window = dioxus::desktop::WindowBuilder::new()
        .with_title("RusTerm")
        .with_inner_size(dioxus::desktop::LogicalSize::new(1200.0, 800.0))
        .with_min_inner_size(dioxus::desktop::LogicalSize::new(600.0, 400.0))
        .with_resizable(true)
        .with_maximizable(true);

    let head_html = r#"<meta name="viewport" content="width=device-width,initial-scale=1.0">
<style>
html,body{margin:0;padding:0;width:100%;height:100%;overflow:hidden;background:#1a1b26;}
#main{width:100%;height:100%;overflow:hidden;}
.conn-item:hover{background:#24283b!important;}
.conn-item{cursor:pointer;transition:background 0.1s;}
*:focus{outline:none !important;box-shadow:none !important;}
*[tabindex]:focus{outline:none !important;box-shadow:none !important;}
</style>"#;

    let cfg = dioxus::desktop::Config::new()
        .with_window(window)
        .with_background_color((26, 27, 38, 255))
        .with_custom_head(head_html.to_string());

    dioxus::LaunchBuilder::new()
        .with_cfg(cfg)
        .launch(rusterm_ui::App);
}
