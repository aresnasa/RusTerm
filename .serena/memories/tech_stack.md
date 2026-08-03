# Tech stack

- Rust workspace, edition 2024; Cargo package manager/build/test runner.
- Async: Tokio full; SSH: russh 0.61, russh-config 0.58, russh-sftp 2.3.
- Desktop UI: Dioxus 0.7 desktop/router; terminal parsing: vte 0.15; local PTY: portable-pty 0.9.
- Persistence: rusqlite + tokio-rusqlite; optional bundled DuckDB analytics via `rusterm-ui/analytics`.
- TLS/network: rustls/tokio-rustls, reqwest rustls, Axum relay.
- macOS is a primary runtime; Dioxus uses WKWebView. Browser automation generally cannot attach to that embedded webview.
- Workspace versions and members are authoritative in root `Cargo.toml`; resolved versions in `Cargo.lock`.