//! Build script: rasterize `assets/gemini-svg.svg` → `OUT_DIR/icon.png`.
//!
//! Why build-time and not a committed PNG:
//!   - SVG stays the single source of truth (icon task requirement).
//!   - The `image` crate (used transitively by dioxus for window icons)
//!     cannot decode SVG, so we need a real rasterizer. `resvg` + `usvg`
//!     + `tiny-skia` is a pure-Rust, dependency-free rasterizer that
//!     handles gradients, filters (feDropShadow) and paths — all features
//!     used by `gemini-svg.svg`.
//!   - Output is embedded via `include_bytes!(concat!(env!(\"OUT_DIR\"), \"/icon.png\"))`
//!     in `main.rs`, so the icon is compiled into the `rusterm` bin.
//!
//! Build.rs CWD is the crate root (`crates/rusterm-app/`), so the SVG is
//! two levels up: `../../assets/gemini-svg.svg`.
//!
//! Re-runs only when the SVG changes (cargo:rerun-if-changed).

fn main() {
    // Re-run if the SVG source changes. (No need to rerun-if-changed=build.rs
    // itself — Cargo does that implicitly.)
    println!("cargo:rerun-if-changed=../../assets/gemini-svg.svg");

    let svg_path = std::path::Path::new("../../assets/gemini-svg.svg");
    let svg = std::fs::read_to_string(svg_path)
        .unwrap_or_else(|e| panic!("read SVG at {:?}: {e}", svg_path));

    // Parse SVG. The SVG has no <text> elements, so we can keep
    // default-features off (skips fontdb/fontconfig dependency).
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default())
        .expect("parse gemini-svg.svg");

    // 512x512 matches the SVG's viewBox and gives crisp icons up to 256px
    // (the largest frame dioxus/tao requests on Windows).
    let mut pixmap = tiny_skia::Pixmap::new(512, 512).expect("alloc 512x512 pixmap");

    resvg::render(&tree, tiny_skia::Transform::identity(), &mut pixmap.as_mut())
        .expect("render SVG to pixmap");

    let png = pixmap.encode_png().expect("encode PNG");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR set by Cargo");
    let out_path = std::path::Path::new(&out_dir).join("icon.png");
    std::fs::write(&out_path, &png).unwrap_or_else(|e| panic!("write {:?}: {e}", out_path));

    println!(
        "cargo:warning=Generated app icon: {} ({} bytes)",
        out_path.display(),
        png.len()
    );
}
