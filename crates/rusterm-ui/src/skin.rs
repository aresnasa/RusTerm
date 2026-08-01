use rusterm_core::config::SkinSettings;

/// CSS custom properties for application chrome. Terminal cell colors are kept
/// independent so ANSI, xterm-256, true-color, and OSC palette changes retain
/// their terminal semantics.
pub fn css_variables(settings: &SkinSettings) -> String {
    let palette = settings.palette();
    format!(
        "--skin-bg:{};--skin-surface:{};--skin-surface-hover:{};--skin-border:{};--skin-border-strong:{};--skin-text:{};--skin-text-muted:{};--skin-accent:{};--skin-accent-secondary:{};--skin-success:{};--skin-warning:{};--skin-danger:{};",
        palette.background,
        palette.surface,
        palette.surface_hover,
        palette.border,
        palette.border_strong,
        palette.text,
        palette.text_muted,
        palette.accent,
        palette.accent_secondary,
        palette.success,
        palette.warning,
        palette.danger,
    )
}
