//! Build the app icon assets from `assets/gemini-svg.svg`.
//!
//! The SVG remains the source of truth. We produce three build outputs:
//! - `OUT_DIR/icon.png` for the native window icon API.
//! - `OUT_DIR/assets/gemini-svg.svg` for the WebView's runtime SVG icon.
//! - `OUT_DIR/AppIcon.icns` (macOS) with the complete 16px–1024px icon set.
//!
//! The PNG and SVG are consumed with `include_bytes!` in `main.rs`; the ICNS is
//! consumed by the macOS packaging scripts. Runtime icon loading never depends
//! on the process working directory or an external `assets/` directory.

const MACOS_ICON_SIZES: [(&str, u32); 10] = [
    ("icon_16x16.png", 16),
    ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32),
    ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128),
    ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256),
    ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512),
    ("icon_512x512@2x.png", 1024),
];

fn render_png(tree: &usvg::Tree, size: u32) -> Vec<u8> {
    let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("allocate app icon pixmap");
    let scale = size as f32 / tree.size().width();
    resvg::render(
        tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().expect("encode app icon PNG")
}

fn generate_macos_icon(tree: &usvg::Tree, out_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return None;
    }

    let iconset = out_dir.join("AppIcon.iconset");
    if iconset.exists() {
        std::fs::remove_dir_all(&iconset).expect("remove stale AppIcon.iconset");
    }
    std::fs::create_dir_all(&iconset).expect("create AppIcon.iconset");

    for (name, size) in MACOS_ICON_SIZES {
        let path = iconset.join(name);
        std::fs::write(&path, render_png(tree, size))
            .unwrap_or_else(|e| panic!("write {:?}: {e}", path));
    }

    let icns_path = out_dir.join("AppIcon.icns");
    let status = std::process::Command::new("iconutil")
        .args(["-c", "icns"])
        .arg(&iconset)
        .args(["-o"])
        .arg(&icns_path)
        .status()
        .expect("run macOS iconutil");
    assert!(status.success(), "iconutil failed with {status}");
    Some(icns_path)
}

fn main() {
    // Re-run if the SVG source changes. (No need to rerun-if-changed=build.rs
    // itself — Cargo does that implicitly.)
    println!("cargo:rerun-if-changed=../../assets/gemini-svg.svg");

    let svg_path = std::path::Path::new("../../assets/gemini-svg.svg");
    let svg = std::fs::read_to_string(svg_path)
        .unwrap_or_else(|e| panic!("read SVG at {:?}: {e}", svg_path));

    // Parse SVG. The SVG has no <text> elements, so we can keep
    // default-features off (skips fontdb/fontconfig dependency).
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).expect("parse gemini-svg.svg");

    // Tao consumes this embedded PNG on Windows/Linux. macOS also uses it at
    // runtime through NSApplication::setApplicationIconImage.
    let png = render_png(&tree, 512);

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR set by Cargo");
    let out_dir = std::path::Path::new(&out_dir);
    let png_path = out_dir.join("icon.png");
    std::fs::write(&png_path, &png).unwrap_or_else(|e| panic!("write {:?}: {e}", png_path));

    let svg_out_dir = out_dir.join("assets");
    std::fs::create_dir_all(&svg_out_dir)
        .unwrap_or_else(|e| panic!("create {:?}: {e}", svg_out_dir));
    let svg_out_path = svg_out_dir.join("gemini-svg.svg");
    std::fs::write(&svg_out_path, svg.as_bytes())
        .unwrap_or_else(|e| panic!("write {:?}: {e}", svg_out_path));

    let macos_icon = generate_macos_icon(&tree, out_dir);

    // Use `eprintln!` (stderr) instead of `cargo:warning=` so the messages
    // still appear in `cargo run` / `cargo build` output but are NOT classified
    // as warnings. `cargo:warning=` lines would clutter the build summary with
    // spurious "warning:" entries for what is purely informational status.
    eprintln!(
        "[rusterm-app] generated app icon PNG: {} ({} bytes)",
        png_path.display(),
        png.len()
    );
    eprintln!(
        "[rusterm-app] embedded app icon SVG: {} ({} bytes)",
        svg_out_path.display(),
        svg.len()
    );
    if let Some(path) = macos_icon {
        eprintln!("[rusterm-app] generated macOS app icon: {}", path.display());
    }
}
