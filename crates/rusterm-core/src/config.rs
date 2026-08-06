use serde::{Deserialize, Serialize};

/// UI display language. Lives in `rusterm-core::config` (not `rusterm-ui`)
/// so `PersistedConfig` can hold it without `rusterm-core` depending on the
/// UI crate. `rusterm-ui::i18n` re-uses this type for its `GlobalSignal` and
/// translation catalog.
///
/// Default is `Zh` for backward compatibility — the app shipped in Chinese
/// before i18n was added, so existing settings files (which omit the field
/// via `#[serde(default)]`) keep the original behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    Zh,
    En,
}

impl Language {
    /// Human-readable label in the language's own script, for the settings
    /// dropdown.
    pub fn label(self) -> &'static str {
        match self {
            Language::Zh => "中文",
            Language::En => "English",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConnectionConfig {
    pub id: String,
    pub name: String,
    pub kind: ConnectionKind,
    pub group: Option<String>,
    pub tags: Vec<String>,
    pub onekey: bool,
    /// Optional per-connection login initialization script (DSL text).
    /// `None` means no script runs after login.
    #[serde(default)]
    pub login_script: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ConnectionKind {
    Ssh(SshConfig),
    Serial(SerialConfig),
    Telnet(TelnetConfig),
    Shell(ShellConfig),
    Tcp(TcpConfig),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProxyKind {
    Http,
    Https,
    Socks5,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProxyConfig {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
    pub terminal_type: String,
    #[serde(default)]
    pub proxy: Option<ProxyConfig>,
    pub proxy_jump: Option<String>,
    pub keepalive_interval: Option<u64>,
    /// Host key verification policy.
    ///
    /// - `"accept-new"` (default): TOFU — first connection records the
    ///   server's host key fingerprint to `known_hosts`; subsequent
    ///   connections reject mismatched keys (MITM protection).
    /// - `"strict"`: reject any host whose key is not already in
    ///   `known_hosts`. Safest mode; requires the user to pre-populate
    ///   `known_hosts` (e.g. via `ssh-keyscan` or a previous `accept-new`
    ///   run on a trusted network).
    /// - `"disabled"`: skip verification entirely. **INSECURE** — vulnerable
    ///   to MITM. Provided only for break-glass / lab scenarios.
    #[serde(default = "default_host_key_policy")]
    pub host_key_policy: String,
}

pub fn default_host_key_policy() -> String {
    "accept-new".to_string()
}

// NOTE: `Debug` for `SshAuth` is implemented manually below to ensure passwords
// and key passphrases are never accidentally leaked through `{:?}` formatting
// (e.g. via `tracing::error!(?auth)`). This is part of RusTerm's privacy
// guarantee: secrets never appear in logs.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub enum SshAuth {
    Password {
        password: String,
    },
    Key {
        private_key_path: String,
        passphrase: Option<String>,
    },
    Agent,
}

impl std::fmt::Debug for SshAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SshAuth::Password { .. } => f
                .debug_struct("SshAuth::Password")
                .field("password", &"<redacted>")
                .finish(),
            SshAuth::Key {
                private_key_path, ..
            } => f
                .debug_struct("SshAuth::Key")
                .field("private_key_path", private_key_path)
                .field("passphrase", &"<redacted>")
                .finish(),
            SshAuth::Agent => f.write_str("SshAuth::Agent"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SerialConfig {
    pub port: String,
    pub baud_rate: u32,
    pub data_bits: u8,
    pub parity: String,
    pub stop_bits: u8,
    pub flow_control: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelnetConfig {
    pub host: String,
    pub port: u16,
}

// `ShellConfig::env` can carry secrets (e.g. `AWS_SECRET_ACCESS_KEY=...`),
// so its `Debug` impl redacts all env *values* while preserving keys for
// diagnosability (knowing which env vars are set is operationally useful;
// knowing their values is not, and is a classic leak vector).
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ShellConfig {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub working_dir: Option<String>,
}

impl std::fmt::Debug for ShellConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted_env: Vec<(String, &str)> = self
            .env
            .iter()
            .map(|(k, _)| (k.clone(), "<redacted>"))
            .collect();
        f.debug_struct("ShellConfig")
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env", &redacted_env)
            .field("working_dir", &self.working_dir)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TcpConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConfig {
    pub connections: Vec<ConnectionConfig>,
}

// --- Persistence types (encrypted JSON on disk) ---

// `EncryptedValue` stores AEAD ciphertext (nonce + AES-256-GCM output) as
// base64. The ciphertext itself is not secret, but we redact it in `Debug`
// to keep logs compact and avoid creating the impression that any
// cryptographic material is being written to disk in the clear.
#[derive(Clone, Serialize, Deserialize)]
pub struct EncryptedValue {
    pub _encrypted: String,
}

impl std::fmt::Debug for EncryptedValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedValue")
            .field("_encrypted", &"<redacted>")
            .finish()
    }
}

/// Visual treatment for the top tab whose session owns the focused pane.
///
/// The full outline is rendered as an inset shadow so changing its width does
/// not resize tabs or make the tab row jump.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FocusedTabAppearance {
    #[serde(default = "default_focused_tab_border_color")]
    pub border_color: String,
    #[serde(default = "default_focused_tab_border_width")]
    pub border_width: u8,
    #[serde(default = "default_focused_tab_border_radius")]
    pub border_radius: u8,
}

impl Default for FocusedTabAppearance {
    fn default() -> Self {
        Self {
            border_color: default_focused_tab_border_color(),
            border_width: default_focused_tab_border_width(),
            border_radius: default_focused_tab_border_radius(),
        }
    }
}

impl FocusedTabAppearance {
    /// Keep values safe for direct CSS interpolation, including settings that
    /// were edited manually outside the application.
    pub fn normalized(mut self) -> Self {
        let color_is_hex = self.border_color.len() == 7
            && self.border_color.starts_with('#')
            && self.border_color[1..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit());
        if !color_is_hex {
            self.border_color = default_focused_tab_border_color();
        }
        self.border_width = self.border_width.clamp(1, 4);
        self.border_radius = self.border_radius.min(12);
        self
    }
}

/// Palette for the non-terminal application chrome. Each value is restricted
/// to a hexadecimal CSS color before it is interpolated into the UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkinPalette {
    #[serde(default = "default_skin_background")]
    pub background: String,
    #[serde(default = "default_skin_surface")]
    pub surface: String,
    #[serde(default = "default_skin_surface_hover")]
    pub surface_hover: String,
    #[serde(default = "default_skin_border")]
    pub border: String,
    #[serde(default = "default_skin_border_strong")]
    pub border_strong: String,
    #[serde(default = "default_skin_text")]
    pub text: String,
    #[serde(default = "default_skin_text_muted")]
    pub text_muted: String,
    #[serde(default = "default_skin_accent")]
    pub accent: String,
    #[serde(default = "default_skin_accent_secondary")]
    pub accent_secondary: String,
    #[serde(default = "default_skin_success")]
    pub success: String,
    #[serde(default = "default_skin_warning")]
    pub warning: String,
    #[serde(default = "default_skin_danger")]
    pub danger: String,
}

impl Default for SkinPalette {
    fn default() -> Self {
        Self::tokyo_night()
    }
}

impl SkinPalette {
    pub fn tokyo_night() -> Self {
        Self {
            background: default_skin_background(),
            surface: default_skin_surface(),
            surface_hover: default_skin_surface_hover(),
            border: default_skin_border(),
            border_strong: default_skin_border_strong(),
            text: default_skin_text(),
            text_muted: default_skin_text_muted(),
            accent: default_skin_accent(),
            accent_secondary: default_skin_accent_secondary(),
            success: default_skin_success(),
            warning: default_skin_warning(),
            danger: default_skin_danger(),
        }
    }

    pub fn one_dark() -> Self {
        Self {
            background: "#282c34".to_string(),
            surface: "#21252b".to_string(),
            surface_hover: "#2c313c".to_string(),
            border: "#3e4451".to_string(),
            border_strong: "#5c6370".to_string(),
            text: "#abb2bf".to_string(),
            text_muted: "#7f848e".to_string(),
            accent: "#61afef".to_string(),
            accent_secondary: "#c678dd".to_string(),
            success: "#98c379".to_string(),
            warning: "#e5c07b".to_string(),
            danger: "#e06c75".to_string(),
        }
    }

    pub fn solarized_dark() -> Self {
        Self {
            background: "#002b36".to_string(),
            surface: "#073642".to_string(),
            surface_hover: "#0b4351".to_string(),
            border: "#164b57".to_string(),
            border_strong: "#2a6572".to_string(),
            text: "#93a1a1".to_string(),
            text_muted: "#657b83".to_string(),
            accent: "#268bd2".to_string(),
            accent_secondary: "#6c71c4".to_string(),
            success: "#859900".to_string(),
            warning: "#b58900".to_string(),
            danger: "#dc322f".to_string(),
        }
    }

    /// Tokyo Night Day — the light counterpart of Tokyo Night.
    pub fn tokyo_night_light() -> Self {
        Self {
            background: "#e1e2e7".to_string(),
            surface: "#e9eaff".to_string(),
            surface_hover: "#d5d6e7".to_string(),
            border: "#c8c9df".to_string(),
            border_strong: "#a0a1c0".to_string(),
            text: "#343b58".to_string(),
            text_muted: "#8c8ca0".to_string(),
            accent: "#2e7de9".to_string(),
            accent_secondary: "#9854f1".to_string(),
            success: "#587539".to_string(),
            warning: "#8c6c3e".to_string(),
            danger: "#f52a82".to_string(),
        }
    }

    /// One Light — the light counterpart of One Dark (Atom One Light).
    pub fn one_light() -> Self {
        Self {
            background: "#fafafa".to_string(),
            surface: "#ffffff".to_string(),
            surface_hover: "#f0f0f0".to_string(),
            border: "#e5e5e6".to_string(),
            border_strong: "#d0d0d3".to_string(),
            text: "#383a42".to_string(),
            text_muted: "#848891".to_string(),
            accent: "#4078f2".to_string(),
            accent_secondary: "#a626a4".to_string(),
            success: "#50a14f".to_string(),
            warning: "#c18401".to_string(),
            danger: "#e45649".to_string(),
        }
    }

    /// Solarized Light — the light counterpart of Solarized Dark.
    pub fn solarized_light() -> Self {
        Self {
            background: "#fdf6e3".to_string(),
            surface: "#eee8d5".to_string(),
            surface_hover: "#e4dec8".to_string(),
            border: "#ddd6c1".to_string(),
            border_strong: "#c3bd9c".to_string(),
            text: "#586e75".to_string(),
            text_muted: "#93a1a1".to_string(),
            accent: "#268bd2".to_string(),
            accent_secondary: "#6c71c4".to_string(),
            success: "#859900".to_string(),
            warning: "#b58900".to_string(),
            danger: "#dc322f".to_string(),
        }
    }

    /// A neutral light palette used as the default for the user-editable
    /// `custom_light` slot so users have a sensible starting point.
    pub fn default_light() -> Self {
        Self::tokyo_night_light()
    }

    /// Normalize colors originating from manually edited settings files before
    /// using them in an inline style string.
    pub fn normalized(mut self) -> Self {
        let fallback = Self::default();
        for (value, default) in [
            (&mut self.background, &fallback.background),
            (&mut self.surface, &fallback.surface),
            (&mut self.surface_hover, &fallback.surface_hover),
            (&mut self.border, &fallback.border),
            (&mut self.border_strong, &fallback.border_strong),
            (&mut self.text, &fallback.text),
            (&mut self.text_muted, &fallback.text_muted),
            (&mut self.accent, &fallback.accent),
            (&mut self.accent_secondary, &fallback.accent_secondary),
            (&mut self.success, &fallback.success),
            (&mut self.warning, &fallback.warning),
            (&mut self.danger, &fallback.danger),
        ] {
            if !is_css_hex_color(value) {
                *value = default.clone();
            }
        }
        self
    }
}

/// The selected built-in palette or the user-editable custom palette.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SkinKind {
    #[default]
    TokyoNight,
    OneDark,
    SolarizedDark,
    Custom,
}

impl SkinKind {
    pub const ALL: [Self; 4] = [
        Self::TokyoNight,
        Self::OneDark,
        Self::SolarizedDark,
        Self::Custom,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::TokyoNight => "Tokyo Night",
            Self::OneDark => "One Dark",
            Self::SolarizedDark => "Solarized Dark",
            Self::Custom => "Custom",
        }
    }
}

/// Appearance mode controlling whether the resolved skin palette uses the dark
/// or light variant of the selected [`SkinKind`].
///
/// - `Dark`  — always use the dark palette.
/// - `Light` — always use the light palette.
/// - `System` — follow the OS dark/light preference, resolved at render time
///   by the UI layer (which has access to the native window theme). The
///   config layer never queries the OS itself, so `rusterm-core` stays free of
///   platform dependencies.
///
/// Defaults to `Dark` for backward compatibility: every pre-existing skin was
/// dark, so legacy configs (which lack this field) load unchanged.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
    System,
}

impl ThemeMode {
    pub const ALL: [Self; 3] = [Self::Dark, Self::Light, Self::System];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
            Self::System => "System",
        }
    }

    /// Resolve `System` to the concrete mode the OS currently reports.
    /// `Dark`/`Light` pass through unchanged.
    pub const fn resolve(self, system_is_dark: bool) -> Self {
        match self {
            Self::System => {
                if system_is_dark {
                    Self::Dark
                } else {
                    Self::Light
                }
            }
            other => other,
        }
    }
}

/// Persisted application-chrome skin preferences. Terminal ANSI/xterm colors
/// are intentionally separate and continue to be controlled by the terminal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkinSettings {
    #[serde(default)]
    pub kind: SkinKind,
    /// Whether to render the dark or light variant of `kind`, or follow the OS
    /// preference (`System`).
    #[serde(default)]
    pub mode: ThemeMode,
    #[serde(default)]
    pub custom: SkinPalette,
    /// User-editable light variant used when `mode` resolves to `Light` and
    /// `kind == Custom`. Defaults to a neutral light palette.
    #[serde(default = "default_custom_light_palette")]
    pub custom_light: SkinPalette,
}

impl Default for SkinSettings {
    fn default() -> Self {
        Self {
            kind: SkinKind::TokyoNight,
            mode: ThemeMode::default(),
            custom: SkinPalette::default(),
            custom_light: default_custom_light_palette(),
        }
    }
}

impl SkinSettings {
    /// Resolve the active palette. `system_is_dark` is supplied by the UI layer
    /// (which reads the native window theme) so that `System` mode can follow
    /// the OS without `rusterm-core` taking a platform dependency.
    pub fn palette(&self, system_is_dark: bool) -> SkinPalette {
        let light = self.mode.resolve(system_is_dark) == ThemeMode::Light;
        match (self.kind, light) {
            (SkinKind::TokyoNight, false) => SkinPalette::tokyo_night(),
            (SkinKind::TokyoNight, true) => SkinPalette::tokyo_night_light(),
            (SkinKind::OneDark, false) => SkinPalette::one_dark(),
            (SkinKind::OneDark, true) => SkinPalette::one_light(),
            (SkinKind::SolarizedDark, false) => SkinPalette::solarized_dark(),
            (SkinKind::SolarizedDark, true) => SkinPalette::solarized_light(),
            (SkinKind::Custom, false) => self.custom.clone().normalized(),
            (SkinKind::Custom, true) => self.custom_light.clone().normalized(),
        }
    }

    pub fn normalized(mut self) -> Self {
        self.custom = self.custom.normalized();
        self.custom_light = self.custom_light.normalized();
        self
    }
}

fn is_css_hex_color(value: &str) -> bool {
    matches!(value.len(), 4 | 7)
        && value.starts_with('#')
        && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn default_skin_background() -> String {
    "#1a1b26".to_string()
}

fn default_skin_surface() -> String {
    "#24283b".to_string()
}

fn default_skin_surface_hover() -> String {
    "#1f2335".to_string()
}

fn default_skin_border() -> String {
    "#2a2b3d".to_string()
}

fn default_skin_border_strong() -> String {
    "#414868".to_string()
}

fn default_skin_text() -> String {
    "#c0caf5".to_string()
}

fn default_skin_text_muted() -> String {
    "#7f849c".to_string()
}

fn default_skin_accent() -> String {
    "#7aa2f7".to_string()
}

fn default_skin_accent_secondary() -> String {
    "#bb9af7".to_string()
}

fn default_skin_success() -> String {
    "#9ece6a".to_string()
}

fn default_skin_warning() -> String {
    "#e0af68".to_string()
}

fn default_skin_danger() -> String {
    "#f7768e".to_string()
}

/// Default for the `SkinSettings::custom_light` field. Uses the Tokyo Night
/// Light palette so the light custom slot starts from a coherent, known-good
/// set of values rather than the dark `SkinPalette::default()`.
fn default_custom_light_palette() -> SkinPalette {
    SkinPalette::default_light()
}

fn default_focused_tab_border_color() -> String {
    "#c0caf5".to_string()
}

fn default_focused_tab_border_width() -> u8 {
    1
}

fn default_focused_tab_border_radius() -> u8 {
    4
}

pub const DEFAULT_SIDEBAR_WIDTH_PX: u16 = 260;
pub const MIN_SIDEBAR_WIDTH_PX: u16 = 200;
pub const MAX_SIDEBAR_WIDTH_PX: u16 = 600;

/// A user-defined connection group shown in the connection sidebar.
/// Connection membership is stored in `ConnectionConfig::group` using this
/// stable id, so renaming a group does not rewrite every connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionGroup {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub collapsed: bool,
}

/// User-controlled connection-sidebar state. All fields have defaults so
/// settings written by older RusTerm versions continue to load unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SidebarPreferences {
    #[serde(default = "default_sidebar_width_px")]
    pub width_px: u16,
    #[serde(default)]
    pub hidden_connection_ids: Vec<String>,
    #[serde(default)]
    pub groups: Vec<ConnectionGroup>,
}

impl Default for SidebarPreferences {
    fn default() -> Self {
        Self {
            width_px: DEFAULT_SIDEBAR_WIDTH_PX,
            hidden_connection_ids: Vec::new(),
            groups: Vec::new(),
        }
    }
}

impl SidebarPreferences {
    /// Clamp dimensions and discard duplicate/invalid ids before state is used
    /// for CSS or written back to disk.
    pub fn normalized(mut self) -> Self {
        self.width_px = self
            .width_px
            .clamp(MIN_SIDEBAR_WIDTH_PX, MAX_SIDEBAR_WIDTH_PX);

        let mut hidden_ids = Vec::with_capacity(self.hidden_connection_ids.len());
        for id in self.hidden_connection_ids {
            if !id.is_empty() && !hidden_ids.contains(&id) {
                hidden_ids.push(id);
            }
        }
        self.hidden_connection_ids = hidden_ids;

        let mut groups = Vec::with_capacity(self.groups.len());
        for mut group in self.groups {
            group.id = group.id.trim().to_string();
            group.name = group.name.trim().to_string();
            if !group.id.is_empty()
                && !group.name.is_empty()
                && !groups
                    .iter()
                    .any(|existing: &ConnectionGroup| existing.id == group.id)
            {
                groups.push(group);
            }
        }
        self.groups = groups;
        self
    }
}

fn default_sidebar_width_px() -> u16 {
    DEFAULT_SIDEBAR_WIDTH_PX
}

pub const DEFAULT_RIGHT_PANEL_WIDTH_PX: u16 = 300;
pub const MIN_RIGHT_PANEL_WIDTH_PX: u16 = 220;
pub const MAX_RIGHT_PANEL_WIDTH_PX: u16 = 600;
pub const DEFAULT_BOTTOM_PANEL_HEIGHT_PX: u16 = 220;
pub const MIN_BOTTOM_PANEL_HEIGHT_PX: u16 = 120;
pub const MAX_BOTTOM_PANEL_HEIGHT_PX: u16 = 520;

fn default_workspace_panel_visible() -> bool {
    true
}

fn default_right_panel_width_px() -> u16 {
    DEFAULT_RIGHT_PANEL_WIDTH_PX
}

fn default_bottom_panel_height_px() -> u16 {
    DEFAULT_BOTTOM_PANEL_HEIGHT_PX
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum LeftPanelTab {
    #[default]
    Connections,
    Files,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RightPanelTab {
    #[default]
    Sessions,
    History,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BottomPanelTab {
    #[default]
    Send,
    Shell,
    Transfers,
    /// REST API relay configuration + auto-generated curl examples. Mirrors
    /// the standalone `RelayPanel` modal but lives in the bottom dock so the
    /// user can configure it right next to the sessions it controls.
    Api,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PanelId {
    Connections,
    RemoteFiles,
    Sessions,
    History,
    Send,
    EmbeddedShell,
    Transfers,
    /// Bottom-dock REST API relay panel.
    Relay,
}

impl PanelId {
    pub const ALL: [Self; 8] = [
        Self::Connections,
        Self::RemoteFiles,
        Self::Sessions,
        Self::History,
        Self::Send,
        Self::EmbeddedShell,
        Self::Transfers,
        Self::Relay,
    ];

    const fn default_zone(self) -> DockZone {
        match self {
            Self::Connections | Self::RemoteFiles => DockZone::Left,
            Self::Sessions | Self::History => DockZone::Right,
            Self::Send | Self::EmbeddedShell | Self::Transfers | Self::Relay => DockZone::Bottom,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DockZone {
    Left,
    Right,
    Bottom,
}

impl DockZone {
    const ALL: [Self; 3] = [Self::Left, Self::Right, Self::Bottom];
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DockStackState {
    #[serde(default)]
    pub panels: Vec<PanelId>,
    #[serde(default)]
    pub active: Option<PanelId>,
    #[serde(default = "default_workspace_panel_visible")]
    pub visible: bool,
    #[serde(default)]
    pub extent_px: u16,
}

impl DockStackState {
    fn normalized_for(mut self, zone: DockZone) -> Self {
        let mut unique = Vec::with_capacity(self.panels.len());
        for panel in self.panels {
            if !unique.contains(&panel) {
                unique.push(panel);
            }
        }
        self.panels = unique;
        self.active = self.active.filter(|panel| self.panels.contains(panel));
        if self.active.is_none() {
            self.active = self.panels.first().copied();
        }
        self.extent_px = match zone {
            DockZone::Left => self
                .extent_px
                .clamp(MIN_SIDEBAR_WIDTH_PX, MAX_SIDEBAR_WIDTH_PX),
            DockZone::Right => self
                .extent_px
                .clamp(MIN_RIGHT_PANEL_WIDTH_PX, MAX_RIGHT_PANEL_WIDTH_PX),
            DockZone::Bottom => self
                .extent_px
                .clamp(MIN_BOTTOM_PANEL_HEIGHT_PX, MAX_BOTTOM_PANEL_HEIGHT_PX),
        };
        self
    }
}

fn default_left_dock_stack() -> DockStackState {
    DockStackState {
        panels: vec![PanelId::Connections, PanelId::RemoteFiles],
        active: Some(PanelId::Connections),
        visible: true,
        extent_px: DEFAULT_SIDEBAR_WIDTH_PX,
    }
}

fn default_right_dock_stack() -> DockStackState {
    DockStackState {
        panels: vec![PanelId::Sessions, PanelId::History],
        active: Some(PanelId::Sessions),
        visible: true,
        extent_px: DEFAULT_RIGHT_PANEL_WIDTH_PX,
    }
}

fn default_bottom_dock_stack() -> DockStackState {
    DockStackState {
        panels: vec![PanelId::Send, PanelId::EmbeddedShell, PanelId::Transfers],
        active: Some(PanelId::Send),
        visible: true,
        extent_px: DEFAULT_BOTTOM_PANEL_HEIGHT_PX,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DockLayout {
    #[serde(default = "default_left_dock_stack")]
    pub left: DockStackState,
    #[serde(default = "default_right_dock_stack")]
    pub right: DockStackState,
    #[serde(default = "default_bottom_dock_stack")]
    pub bottom: DockStackState,
}

impl Default for DockLayout {
    fn default() -> Self {
        Self {
            left: default_left_dock_stack(),
            right: default_right_dock_stack(),
            bottom: default_bottom_dock_stack(),
        }
    }
}

impl DockLayout {
    pub fn stack(&self, zone: DockZone) -> &DockStackState {
        match zone {
            DockZone::Left => &self.left,
            DockZone::Right => &self.right,
            DockZone::Bottom => &self.bottom,
        }
    }

    pub fn stack_mut(&mut self, zone: DockZone) -> &mut DockStackState {
        match zone {
            DockZone::Left => &mut self.left,
            DockZone::Right => &mut self.right,
            DockZone::Bottom => &mut self.bottom,
        }
    }

    pub fn zone_for(&self, panel: PanelId) -> Option<DockZone> {
        DockZone::ALL
            .into_iter()
            .find(|zone| self.stack(*zone).panels.contains(&panel))
    }

    pub fn normalize(&mut self) {
        let mut seen = Vec::with_capacity(PanelId::ALL.len());
        for zone in DockZone::ALL {
            self.stack_mut(zone).panels.retain(|panel| {
                if seen.contains(panel) {
                    false
                } else {
                    seen.push(*panel);
                    true
                }
            });
        }

        for panel in PanelId::ALL {
            if !seen.contains(&panel) {
                self.stack_mut(panel.default_zone()).panels.push(panel);
                seen.push(panel);
            }
        }

        for zone in DockZone::ALL {
            let stack = std::mem::replace(self.stack_mut(zone), default_left_dock_stack());
            *self.stack_mut(zone) = stack.normalized_for(zone);
        }
    }

    pub fn normalized(mut self) -> Self {
        self.normalize();
        self
    }

    pub fn move_panel(&mut self, panel: PanelId, target_zone: DockZone, target_index: usize) {
        self.normalize();
        let source_zone = self.zone_for(panel);
        let panel_was_active = source_zone
            .map(|zone| self.stack(zone).active == Some(panel))
            .unwrap_or(false);
        let target_was_hidden = !self.stack(target_zone).visible;

        for zone in DockZone::ALL {
            let stack = self.stack_mut(zone);
            stack.panels.retain(|candidate| *candidate != panel);
            if stack.active == Some(panel) {
                stack.active = None;
            }
        }

        let target = self.stack_mut(target_zone);
        target
            .panels
            .insert(target_index.min(target.panels.len()), panel);
        target.visible = true;
        if panel_was_active || source_zone != Some(target_zone) || target_was_hidden {
            target.active = Some(panel);
        }
        self.normalize();
    }

    pub fn set_zone_visible(&mut self, zone: DockZone, visible: bool) {
        self.stack_mut(zone).visible = visible;
    }

    pub fn hide_zone(&mut self, zone: DockZone) {
        self.set_zone_visible(zone, false);
    }

    pub fn show_zone(&mut self, zone: DockZone) {
        self.set_zone_visible(zone, true);
    }
}

/// Persistent state for the tool windows docked around the terminal canvas.
/// Terminal pane geometry remains owned by `PaneLayout`; these preferences only
/// describe the outer workspace chrome. The legacy fields remain the UI-facing
/// compatibility projection while the complete layout is stored in `dock_layout`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspacePreferences {
    #[serde(default = "default_workspace_panel_visible")]
    pub left_visible: bool,
    #[serde(default = "default_workspace_panel_visible")]
    pub right_visible: bool,
    #[serde(default = "default_workspace_panel_visible")]
    pub bottom_visible: bool,
    #[serde(default = "default_right_panel_width_px")]
    pub right_width_px: u16,
    #[serde(default = "default_bottom_panel_height_px")]
    pub bottom_height_px: u16,
    #[serde(default)]
    pub left_tab: LeftPanelTab,
    #[serde(default)]
    pub right_tab: RightPanelTab,
    #[serde(default)]
    pub bottom_tab: BottomPanelTab,
    #[serde(default)]
    pub dock_layout: DockLayout,
}

impl Default for WorkspacePreferences {
    fn default() -> Self {
        Self {
            left_visible: true,
            right_visible: true,
            bottom_visible: true,
            right_width_px: DEFAULT_RIGHT_PANEL_WIDTH_PX,
            bottom_height_px: DEFAULT_BOTTOM_PANEL_HEIGHT_PX,
            left_tab: LeftPanelTab::default(),
            right_tab: RightPanelTab::default(),
            bottom_tab: BottomPanelTab::default(),
            dock_layout: DockLayout::default(),
        }
    }
}

impl WorkspacePreferences {
    pub fn normalized(mut self) -> Self {
        self.right_width_px = self
            .right_width_px
            .clamp(MIN_RIGHT_PANEL_WIDTH_PX, MAX_RIGHT_PANEL_WIDTH_PX);
        self.bottom_height_px = self
            .bottom_height_px
            .clamp(MIN_BOTTOM_PANEL_HEIGHT_PX, MAX_BOTTOM_PANEL_HEIGHT_PX);
        self.dock_layout.normalize();

        self.dock_layout.left.visible = self.left_visible;
        self.dock_layout.right.visible = self.right_visible;
        self.dock_layout.bottom.visible = self.bottom_visible;
        self.dock_layout.right.extent_px = self.right_width_px;
        self.dock_layout.bottom.extent_px = self.bottom_height_px;

        self.migrate_legacy_active_tabs();
        self.dock_layout.normalize();
        self.sync_legacy_projection();
        self
    }

    pub fn move_panel(&mut self, panel: PanelId, target_zone: DockZone, target_index: usize) {
        self.dock_layout
            .move_panel(panel, target_zone, target_index);
        self.sync_legacy_projection();
    }

    pub fn set_zone_visible(&mut self, zone: DockZone, visible: bool) {
        self.dock_layout.set_zone_visible(zone, visible);
        self.sync_legacy_projection();
    }

    pub fn hide_zone(&mut self, zone: DockZone) {
        self.set_zone_visible(zone, false);
    }

    pub fn show_zone(&mut self, zone: DockZone) {
        self.set_zone_visible(zone, true);
    }

    fn migrate_legacy_active_tabs(&mut self) {
        let legacy_panels = [
            (
                DockZone::Left,
                match self.left_tab {
                    LeftPanelTab::Connections => PanelId::Connections,
                    LeftPanelTab::Files => PanelId::RemoteFiles,
                },
                matches!(
                    self.dock_layout.left.active,
                    None | Some(PanelId::Connections | PanelId::RemoteFiles)
                ),
            ),
            (
                DockZone::Right,
                match self.right_tab {
                    RightPanelTab::Sessions => PanelId::Sessions,
                    RightPanelTab::History => PanelId::History,
                },
                matches!(
                    self.dock_layout.right.active,
                    None | Some(PanelId::Sessions | PanelId::History)
                ),
            ),
            (
                DockZone::Bottom,
                match self.bottom_tab {
                    BottomPanelTab::Send => PanelId::Send,
                    BottomPanelTab::Shell => PanelId::EmbeddedShell,
                    BottomPanelTab::Transfers => PanelId::Transfers,
                    BottomPanelTab::Api => PanelId::Relay,
                },
                matches!(
                    self.dock_layout.bottom.active,
                    None | Some(
                        PanelId::Send
                            | PanelId::EmbeddedShell
                            | PanelId::Transfers
                            | PanelId::Relay
                    )
                ),
            ),
        ];

        for (zone, panel, active_is_legacy_panel) in legacy_panels {
            let stack = self.dock_layout.stack_mut(zone);
            if active_is_legacy_panel && stack.panels.contains(&panel) {
                stack.active = Some(panel);
            }
        }
    }

    fn sync_legacy_projection(&mut self) {
        self.left_visible = self.dock_layout.left.visible;
        self.right_visible = self.dock_layout.right.visible;
        self.bottom_visible = self.dock_layout.bottom.visible;
        self.right_width_px = self.dock_layout.right.extent_px;
        self.bottom_height_px = self.dock_layout.bottom.extent_px;

        if let Some(active) = self.dock_layout.left.active {
            self.left_tab = match active {
                PanelId::Connections => LeftPanelTab::Connections,
                PanelId::RemoteFiles => LeftPanelTab::Files,
                _ => self.left_tab,
            };
        }
        if let Some(active) = self.dock_layout.right.active {
            self.right_tab = match active {
                PanelId::Sessions => RightPanelTab::Sessions,
                PanelId::History => RightPanelTab::History,
                _ => self.right_tab,
            };
        }
        if let Some(active) = self.dock_layout.bottom.active {
            self.bottom_tab = match active {
                PanelId::Send => BottomPanelTab::Send,
                PanelId::EmbeddedShell => BottomPanelTab::Shell,
                PanelId::Transfers => BottomPanelTab::Transfers,
                PanelId::Relay => BottomPanelTab::Api,
                _ => self.bottom_tab,
            };
        }
    }
}

/// A user-configured application shortcut. `primary` means Command on macOS
/// and Control on other platforms, so settings remain portable across devices.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub key: String,
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
}

impl KeyChord {
    pub fn normalized(mut self) -> Option<Self> {
        self.key = self.key.trim().to_ascii_lowercase();
        (!self.key.is_empty()).then_some(self)
    }

    /// Global application shortcuts must include the primary modifier and
    /// Shift. This deliberately leaves common terminal controls such as
    /// Ctrl+C and Ctrl+W available to the PTY.
    pub fn is_safe_application_shortcut(&self) -> bool {
        self.primary && self.shift
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum KeybindingAction {
    CloseFocusedPane,
    AppendPane,
    ToggleComparison,
    TogglePaneZoom,
    /// Toggle the floating agent chat box (issue #122).
    ///
    /// Default chord is Cmd/Ctrl + Shift + Space — this is the lowest-friction
    /// binding that satisfies `KeyChord::is_safe_application_shortcut`
    /// (primary && shift) AND actually reaches the app on macOS. Plain
    /// Cmd+Space is intercepted by Spotlight at the OS level, so it can't be
    /// the default; the global `onkeydown` in `rusterm-ui::app` still
    /// best-effort toggles the chat on plain Cmd+Space for users who've
    /// rebound Spotlight.
    ToggleChat,
}

impl KeybindingAction {
    pub const ALL: [Self; 5] = [
        Self::CloseFocusedPane,
        Self::AppendPane,
        Self::ToggleComparison,
        Self::TogglePaneZoom,
        Self::ToggleChat,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::CloseFocusedPane => "Close focused pane",
            Self::AppendPane => "Add split pane",
            Self::ToggleComparison => "Toggle synchronized input",
            Self::TogglePaneZoom => "Toggle pane zoom",
            Self::ToggleChat => "Toggle agent chat",
        }
    }
}

/// Application-level keybindings. `None` disables an action's shortcut.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Keybindings {
    #[serde(default = "default_close_focused_pane_keybinding")]
    pub close_focused_pane: Option<KeyChord>,
    #[serde(default = "default_append_pane_keybinding")]
    pub append_pane: Option<KeyChord>,
    #[serde(default = "default_toggle_comparison_keybinding")]
    pub toggle_comparison: Option<KeyChord>,
    #[serde(default = "default_toggle_pane_zoom_keybinding")]
    pub toggle_pane_zoom: Option<KeyChord>,
    /// Toggle the floating agent chat box (issue #122).
    /// Default Cmd/Ctrl + Shift + Space — see `KeybindingAction::ToggleChat`.
    #[serde(default = "default_toggle_chat_keybinding")]
    pub toggle_chat: Option<KeyChord>,
}

fn default_key_chord(key: &str) -> Option<KeyChord> {
    Some(KeyChord {
        key: key.to_string(),
        primary: true,
        alt: false,
        shift: true,
    })
}

fn default_close_focused_pane_keybinding() -> Option<KeyChord> {
    default_key_chord("w")
}

fn default_append_pane_keybinding() -> Option<KeyChord> {
    default_key_chord("l")
}

fn default_toggle_comparison_keybinding() -> Option<KeyChord> {
    default_key_chord("c")
}

fn default_toggle_pane_zoom_keybinding() -> Option<KeyChord> {
    default_key_chord("f")
}

/// Cmd/Ctrl + Shift + Space. `KeyChord::key` for the space bar is the
/// literal string "space" (see `rusterm_ui::keybindings::key_name` — the
/// space character is whitespace so the `trim().is_empty()` arm maps it to
/// "space"). This chord passes `is_safe_application_shortcut` (primary &&
/// shift) and is reachable on macOS (unlike plain Cmd+Space → Spotlight).
fn default_toggle_chat_keybinding() -> Option<KeyChord> {
    Some(KeyChord {
        key: "space".to_string(),
        primary: true,
        alt: false,
        shift: true,
    })
}

impl Default for Keybindings {
    fn default() -> Self {
        Self {
            close_focused_pane: default_close_focused_pane_keybinding(),
            append_pane: default_append_pane_keybinding(),
            toggle_comparison: default_toggle_comparison_keybinding(),
            toggle_pane_zoom: default_toggle_pane_zoom_keybinding(),
            toggle_chat: default_toggle_chat_keybinding(),
        }
    }
}

impl Keybindings {
    pub fn normalized(mut self) -> Self {
        for action in KeybindingAction::ALL {
            let chord = self.chord(action).cloned().and_then(KeyChord::normalized);
            self.set_chord(action, chord.filter(KeyChord::is_safe_application_shortcut));
        }
        self
    }

    pub fn chord(&self, action: KeybindingAction) -> Option<&KeyChord> {
        match action {
            KeybindingAction::CloseFocusedPane => self.close_focused_pane.as_ref(),
            KeybindingAction::AppendPane => self.append_pane.as_ref(),
            KeybindingAction::ToggleComparison => self.toggle_comparison.as_ref(),
            KeybindingAction::TogglePaneZoom => self.toggle_pane_zoom.as_ref(),
            KeybindingAction::ToggleChat => self.toggle_chat.as_ref(),
        }
    }

    pub fn set_chord(&mut self, action: KeybindingAction, chord: Option<KeyChord>) {
        match action {
            KeybindingAction::CloseFocusedPane => self.close_focused_pane = chord,
            KeybindingAction::AppendPane => self.append_pane = chord,
            KeybindingAction::ToggleComparison => self.toggle_comparison = chord,
            KeybindingAction::TogglePaneZoom => self.toggle_pane_zoom = chord,
            KeybindingAction::ToggleChat => self.toggle_chat = chord,
        }
    }

    pub fn action_for(&self, chord: &KeyChord) -> Option<KeybindingAction> {
        KeybindingAction::ALL
            .into_iter()
            .find(|action| self.chord(*action).is_some_and(|binding| binding == chord))
    }

    pub fn conflicting_action(
        &self,
        action: KeybindingAction,
        chord: &KeyChord,
    ) -> Option<KeybindingAction> {
        KeybindingAction::ALL.into_iter().find(|other| {
            *other != action && self.chord(*other).is_some_and(|binding| binding == chord)
        })
    }
}

// ===========================================================================
// Agent chat box (issue #122)
// ===========================================================================
//
// A floating, draggable chat panel that lives in the bottom-left of the app
// window by default. It can talk to a configurable AI agent (OpenAI /
// Anthropic / local Qwen) and doubles as a command palette: typing `/` puts
// the input into command-search mode, fuzzy-filtering the user's shell
// history (via `rusterm-history`) plus a built-in list of app commands.
//
// PERSISTENCE: `ChatSettings` is stored under `PersistedConfig::chat`. The
// panel position and the configured agents survive restarts. API keys are
// NOT stored here — they go through the existing secret/redaction path
// (`EncryptedValue` / keychain) to match the project's credential policy.
// The `AgentConfig` here only carries non-secret metadata.

/// Which AI backend an agent routes its prompts to. Mirrors
/// `rusterm_ai::AiProvider` but is defined here in core so the persistence
/// layer doesn't depend on the AI crate (and so the enum can grow
/// independently of the runtime client).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatAgentProvider {
    OpenAI,
    Anthropic,
    /// On-device Qwen model (feature-gated `qwen-local`).
    Local,
}

impl Default for ChatAgentProvider {
    fn default() -> Self {
        Self::OpenAI
    }
}

/// Non-secret description of a configured chat agent. The API key lives in
/// the secret store and is looked up by `api_key_id` at runtime — it is
/// deliberately never serialized into `settings.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Stable opaque id (uuid v4). Used as the foreign key into the secret
    /// store and as the `active_agent_id` selection.
    pub id: String,
    /// Human-readable label shown in the agent dropdown.
    pub name: String,
    /// Which provider backend to route prompts to.
    #[serde(default)]
    pub provider: ChatAgentProvider,
    /// Model identifier (e.g. `gpt-4o-mini`, `claude-3-5-sonnet-20241022`,
    /// or the local model id from `QwenLocalSettings`).
    #[serde(default)]
    pub model: String,
    /// Optional OpenAI-compatible base URL override (e.g. for self-hosted
    /// proxies / Azure OpenAI). Empty string means "use the provider default".
    #[serde(default)]
    pub base_url: String,
    /// Opaque id used to look up the API key in the secret store. Empty
    /// string means "no key configured" — the panel shows a prompt instead
    /// of erroring.
    #[serde(default)]
    pub api_key_id: String,
    /// Optional system prompt prepended to every chat turn. Empty string
    /// means "no system prompt".
    #[serde(default)]
    pub system_prompt: String,
}

impl AgentConfig {
    /// Built-in default agent so the panel is usable on first launch with
    /// no configuration. The user just needs to paste an API key.
    pub fn default_openai() -> Self {
        Self {
            id: "default".to_string(),
            name: "Default".to_string(),
            provider: ChatAgentProvider::OpenAI,
            model: "gpt-4o-mini".to_string(),
            base_url: String::new(),
            api_key_id: "default".to_string(),
            system_prompt: String::new(),
        }
    }
}

/// Floating-panel position in logical pixels, relative to the `#main`
/// container's top-left corner. `(0.0, 0.0)` is the sentinel meaning
/// "not yet placed — use the bottom-left default on first show".
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct ChatPosition {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
}

impl ChatPosition {
    /// `true` when the user has actually dragged the panel at least once.
    /// Used to decide whether to honor the stored coords or fall back to the
    /// bottom-left anchor on the next render.
    pub fn is_set(&self) -> bool {
        self.x != 0.0 || self.y != 0.0
    }
}

/// How the agent chat panel is attached to the main window (issue #122).
/// `Floating` is the legacy behavior: a draggable, `position: fixed` overlay
/// rendered outside `#main`. `Right` / `Bottom` merge the panel into the main
/// window layout so it participates in the flex flow instead of overlapping
/// the terminal content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ChatDock {
    #[default]
    Floating,
    Right,
    Bottom,
}

impl ChatDock {
    /// Cycle order for the title-bar dock toggle button.
    pub fn next(self) -> Self {
        match self {
            ChatDock::Floating => ChatDock::Right,
            ChatDock::Right => ChatDock::Bottom,
            ChatDock::Bottom => ChatDock::Floating,
        }
    }
}

/// How the chat panel's outbound AI/preset requests reach the network
/// (issue #126). Persisted (non-secret) so the choice survives restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ChatProxyMode {
    /// Honor `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` environment variables
    /// (reqwest's default behavior).
    #[default]
    System,
    /// Force a direct connection, ignoring environment proxies.
    Off,
    /// Auto-detect a local Clash-style client (probes 7890/7897/7891 on
    /// loopback) and route through it. Falls back to direct if none found.
    Clash,
    /// Use the explicit `proxy_url` (http://…, https://…, or socks5://…).
    Custom,
}

/// Persisted settings for the agent chat box (issue #122). Stored under
/// `PersistedConfig::chat`. All fields `#[serde(default)]` so legacy
/// settings files load cleanly (chat box just starts hidden with the
/// built-in default agent).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatSettings {
    /// Whether the panel was visible when the app last closed. Restored on
    /// launch so the user's workflow isn't interrupted.
    #[serde(default)]
    pub visible: bool,
    /// Configured agents. Always non-empty (the default OpenAI agent is
    /// guaranteed by `normalized`).
    #[serde(default = "default_chat_agents")]
    pub agents: Vec<AgentConfig>,
    /// Id of the currently-selected agent. `None` means "none selected";
    /// `normalized` clamps it to a valid agent id or the first agent.
    #[serde(default)]
    pub active_agent_id: Option<String>,
    /// Last dragged position. `(0,0)` sentinel → bottom-left default.
    #[serde(default)]
    pub position: ChatPosition,
    /// Panel size in logical pixels. `0.0` → built-in default.
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub height: f64,
    /// How the panel attaches to the main window. Legacy settings files omit
    /// this field and get the floating overlay.
    #[serde(default)]
    pub dock: ChatDock,
    /// Explicit user consent for RusTerm to fetch the official provider
    /// preset catalog from the network (issue #126). Default `false` — no
    /// remote fetch ever happens until the user opts in.
    #[serde(default)]
    pub allow_remote_presets: bool,
    /// Proxy routing for chat/preset requests. Default `System` (env vars).
    #[serde(default)]
    pub proxy_mode: ChatProxyMode,
    /// Proxy URL used when `proxy_mode == Custom`.
    #[serde(default)]
    pub proxy_url: String,
}

fn default_chat_agents() -> Vec<AgentConfig> {
    vec![AgentConfig::default_openai()]
}

impl Default for ChatSettings {
    fn default() -> Self {
        Self {
            visible: false,
            agents: default_chat_agents(),
            active_agent_id: Some("default".to_string()),
            position: ChatPosition::default(),
            width: 0.0,
            height: 0.0,
            dock: ChatDock::Floating,
            allow_remote_presets: false,
            proxy_mode: ChatProxyMode::System,
            proxy_url: String::new(),
        }
    }
}

impl ChatSettings {
    /// Repair untrusted/legacy state: guarantee at least one agent, and make
    /// sure `active_agent_id` points at a real agent (fall back to the
    /// first). Also clamps panel size to sane minimums.
    pub fn normalized(mut self) -> Self {
        if self.agents.is_empty() {
            self.agents = default_chat_agents();
        }
        let valid = self
            .active_agent_id
            .as_ref()
            .and_then(|id| self.agents.iter().find(|a| &a.id == id))
            .map(|a| a.id.clone());
        self.active_agent_id = Some(valid.unwrap_or_else(|| self.agents[0].id.clone()));
        if self.width < 240.0 {
            self.width = 360.0;
        }
        if self.height < 160.0 {
            self.height = 320.0;
        }
        self
    }

    /// Borrow the active agent, if any. Returns `None` only when the list is
    /// empty (which `normalized` prevents, but callers stay defensive).
    pub fn active_agent(&self) -> Option<&AgentConfig> {
        self.active_agent_id
            .as_ref()
            .and_then(|id| self.agents.iter().find(|a| &a.id == id))
            .or_else(|| self.agents.first())
    }
}

/// Which API-panel input a custom template targets. Mirrors the UI's
/// `CurlMode` (Command | Script | Script base64) without coupling core to
/// the UI crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiTemplateMode {
    Command,
    Script,
    ScriptBase64,
}

/// A user-defined quick-pick template for the API panel's curl builder.
/// Non-secret: the label and body are plain command/script text the user
/// chose to save as a shortcut chip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomApiTemplate {
    /// Chip label shown in the templates row.
    pub label: String,
    /// Which mode (and input field) the template targets.
    pub mode: ApiTemplateMode,
    /// The command or script body loaded into the input on click.
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedConfig {
    pub version: u32,
    pub connections: Vec<PersistedConnection>,
    #[serde(default)]
    pub onekeys: Vec<PersistedOneKey>,
    /// Stable OneKey selections learned from multi-match prompts. These records
    /// contain identifiers only; credential values and display labels are never
    /// persisted here.
    #[serde(default)]
    pub onekey_preferences: Vec<OneKeyPreference>,
    #[serde(default)]
    pub master_password_hash: Option<String>,
    /// Legacy preference from the former session-restore confirmation dialog.
    /// Retained for settings compatibility; automatic startup recovery no
    /// longer lets this flag suppress session-state persistence or loading.
    #[serde(default)]
    pub restore_disabled: bool,
    /// Whether to show the "是否确实要关闭本软件？" confirmation dialog when the
    /// user closes the last window. Default true (safe default — always ask).
    /// When false, closing the last window exits the app immediately.
    /// Persisted so the user's choice on the dialog's "下次关闭时不再询问"
    /// checkbox survives across launches.
    #[serde(default = "default_confirm_close_on_exit")]
    pub confirm_close_on_exit: bool,
    /// Whether to warn before highlighting a comparison where more than half
    /// of the visible rows differ. Default true so legacy settings keep the
    /// existing safety prompt.
    #[serde(default = "default_comparison_diff_warning_enabled")]
    pub comparison_diff_warning_enabled: bool,
    /// Appearance of the complete outline around the focused pane's top tab.
    #[serde(default)]
    pub focused_tab_appearance: FocusedTabAppearance,
    /// Whether the inline fish-style command suggestion popup is enabled.
    /// Default true (on by default). When false, no suggestions are shown
    /// at all — the user typed with no ghost-text or dropdown.
    #[serde(default = "default_suggestion_enabled")]
    pub suggestion_enabled: bool,
    /// Maximum number of suggestions shown in the dropdown (3, 5, or 10).
    /// Default 3 to keep the popup compact. The user can change this in
    /// the settings dialog or via the popup's own control.
    #[serde(default = "default_suggestion_count")]
    pub suggestion_count: u8,
    /// Width, hidden connections, and custom groups for the connection sidebar.
    #[serde(default)]
    pub sidebar: SidebarPreferences,
    /// Visibility, size, and active tabs for the outer docked tool panels.
    #[serde(default)]
    pub workspace: WorkspacePreferences,
    /// User-configured application shortcuts. Missing in legacy settings files
    /// means the established defaults are used.
    #[serde(default)]
    pub keybindings: Keybindings,
    /// Application-chrome skin. Missing in legacy settings files means Tokyo
    /// Night remains the default.
    #[serde(default)]
    pub skin: SkinSettings,
    /// Whether the app collects local usage-habit statistics (command names,
    /// success/failure rates, per-hour activity, typo→correction pairs) into
    /// the DuckDB analytics store. Default FALSE — data collection is opt-in
    /// for privacy. Credentials and secret material are sanitized before
    /// anything is stored, regardless of this flag.
    #[serde(default)]
    pub collect_usage_habits: bool,
    /// UI display language. Defaults to `Zh` for backward compatibility
    /// (the app shipped in Chinese before i18n). Existing settings files
    /// omit this field and get `Zh` via `Language`'s `#[default]`.
    #[serde(default)]
    pub language: Language,
    /// User-defined API panel templates. Legacy settings files omit this
    /// field and get an empty list.
    #[serde(default)]
    pub api_custom_templates: Vec<CustomApiTemplate>,
    /// Settings for the local on-device LLM (Qwen2.5-Coder-1.5B-Instruct)
    /// used to generate script/Python templates in the API panel.
    /// Feature-gated behind `rusterm-ai/qwen-local`; legacy settings files
    /// omit this field and get the disabled default.
    #[serde(default)]
    pub qwen_local: QwenLocalSettings,
    /// User-adjusted vertical offset (px) for the terminal suggestion /
    /// OneKey popups, relative to their automatic cursor-row anchor.
    /// Learned from the user dragging the popup and reapplied on every
    /// popup open so the preferred placement survives restarts.
    /// `0.0` (the legacy default) means fully automatic placement.
    #[serde(default)]
    pub suggestion_popup_offset_y: f64,

    /// Optional webhook provider used to fetch OTP / MFA verification codes
    /// automatically during SSH keyboard-interactive authentication. When a
    /// server (e.g. JumpServer with MFA enabled) prompts for an OTP code,
    /// RusTerm consults this provider to obtain the code instead of failing
    /// auth or requiring manual entry.
    ///
    /// `None` (the legacy default) disables auto-fill: OTP prompts fall back
    /// to manual entry through the existing OneKey / credential prompt UI.
    #[serde(default)]
    pub otp_webhook: Option<OtpWebhookConfig>,

    /// Floating agent chat box (issue #122): configurable agents, drag
    /// position, and last visibility. Legacy settings files omit this field
    /// and get the disabled default (panel hidden, built-in OpenAI agent).
    /// API keys are NOT stored here — see `AgentConfig::api_key_id`.
    #[serde(default)]
    pub chat: ChatSettings,
}

// ── OTP / MFA webhook provider ────────────────────────────────────────
//
// JumpServer (and other bastions) may require a one-time verification code
// as a second factor during SSH login. The code is typically delivered out
// of band — pushed to a Feishu/Slack bot chat, sent via SMS, generated by a
// TOTP app, etc. Rather than hard-code any single delivery channel, we model
// the code source as a pluggable webhook. The SSH auth loop detects an OTP
// prompt, asks the configured provider for the current code, and submits it.
//
// Three provider kinds ship out of the box:
//
//   * `feishubot`  — read the latest message from a Feishu P2P/group chat
//                    via the Open Platform API (app_id + app_secret), then
//                    extract the numeric code with a regex. This covers the
//                    common case where an MFA gateway pushes the code to a
//                    Feishu bot.
//   * `http`       — generic HTTP(S) webhook (GET or POST). The response body
//                    is treated as plain text; the OTP is extracted with a
//                    regex. Useful for self-hosted code-relay services or a
//                    TOTP endpoint that returns the current 6-digit code.
//   * `manual`     — no auto-fetch; surface the OTP prompt to the user
//                    through the existing OneKey credential popup so they
//                    can type the code themselves. This is the safe default
//                    and the behaviour when `otp_webhook` is `None`.

/// How RusTerm obtains the OTP / MFA code a bastion (e.g. JumpServer)
/// demands during keyboard-interactive SSH authentication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum OtpWebhookConfig {
    /// Read the latest OTP code from a Feishu (Lark) bot chat via the Open
    /// Platform API. The bot must be added to the chat that receives the
    /// MFA push messages.
    Feishubot {
        /// App id of the Feishu custom app (starts with `cli_`).
        app_id: String,
        /// App secret of the Feishu custom app. Stored in plaintext in the
        /// settings file for now — the master-password encryption layer
        /// covers the whole `PersistedConfig` blob on disk.
        app_secret: String,
        /// Chat id (`oc_...`) the bot listens on. The most recent message
        /// in this chat whose text matches `code_pattern` is used.
        chat_id: String,
        /// Optional regex (default `"\\b\\d{4,8}\\b"`) used to extract the
        /// numeric code from the latest message text.
        #[serde(default = "default_otp_code_pattern")]
        code_pattern: String,
        /// Optional user open id (`ou_...`) to filter received messages.
        /// When set, only messages from this user are considered — useful
        /// when the chat also carries unrelated traffic.
        #[serde(default)]
        sender_open_id: Option<String>,
        /// Maximum age (in seconds) of an acceptable code. Messages older
        /// than this are ignored so a stale code from a previous login
        /// attempt is never re-used. Default 120s.
        #[serde(default = "default_otp_max_age_secs")]
        max_age_secs: u64,
        /// Optional Feishu base URL override. Defaults to the public
        /// `https://open.feishu.cn`. Set to `https://open.larksuite.com`
        /// for the international Lark variant.
        #[serde(default = "default_feishu_base_url")]
        base_url: String,
    },
    /// Generic HTTP webhook. RusTerm issues a GET or POST to `url` and
    /// extracts the code from the response body using `code_pattern`.
    /// Use this for self-hosted TOTP relays or any service that can return
    /// the current code as plain text or JSON.
    Http {
        /// Full URL to call, e.g. `https://totp.example.local/current`.
        url: String,
        /// HTTP method — `"get"` or `"post"`. Default `get`.
        #[serde(default = "default_http_method")]
        method: String,
        /// Optional request body for POST requests (sent as `text/plain`).
        #[serde(default)]
        body: Option<String>,
        /// Optional extra headers, e.g. `Authorization: Bearer ...`.
        #[serde(default)]
        headers: Vec<(String, String)>,
        /// Regex used to extract the code from the response body. Default
        /// `"\\b\\d{4,8}\\b"`.
        #[serde(default = "default_otp_code_pattern")]
        code_pattern: String,
        /// Request timeout in seconds. Default 10.
        #[serde(default = "default_http_timeout_secs")]
        timeout_secs: u64,
    },
    /// No automatic fetch. OTP prompts are surfaced to the user through the
    /// existing OneKey credential popup for manual entry. This is also the
    /// implicit behaviour when `otp_webhook` is `None`.
    Manual,
}

impl OtpWebhookConfig {
    /// `true` when this variant can fetch a code without user interaction.
    /// `Manual` (and `None`) return `false` — the caller must fall back to
    /// the interactive prompt.
    pub fn is_automatic(&self) -> bool {
        matches!(
            self,
            OtpWebhookConfig::Feishubot { .. } | OtpWebhookConfig::Http { .. }
        )
    }
}

/// Default OTP extraction regex: a standalone 4–8 digit number. Broad
/// enough for 6-digit TOTP, 4-digit SMS, and 8-digit Feishu codes; the
/// `max_age_secs` filter on the Feishu path prevents stale matches.
pub fn default_otp_code_pattern() -> String {
    r"\b\d{4,8}\b".to_string()
}

pub fn default_otp_max_age_secs() -> u64 {
    120
}

pub fn default_feishu_base_url() -> String {
    "https://open.feishu.cn".to_string()
}

pub fn default_http_method() -> String {
    "get".to_string()
}

pub fn default_http_timeout_secs() -> u64 {
    10
}

/// Default for `PersistedConfig::confirm_close_on_exit`. Kept as a function
/// (not a constant) so `#[serde(default = "...")]` can reference it. True
/// because the safe default is to always ask before closing the app.
fn default_confirm_close_on_exit() -> bool {
    true
}

/// Default for `PersistedConfig::comparison_diff_warning_enabled`. True keeps
/// the existing protective prompt for new and upgraded installations.
fn default_comparison_diff_warning_enabled() -> bool {
    true
}

/// Default for `PersistedConfig::suggestion_enabled`. True because the
/// suggestion popup is a core productivity feature.
fn default_suggestion_enabled() -> bool {
    true
}

/// Default for `PersistedConfig::suggestion_count`. 3 keeps the popup
/// compact (it grows below the cursor and shouldn't cover too much output).
/// Valid values are 3, 5, or 10.
fn default_suggestion_count() -> u8 {
    3
}

// ── Local LLM (Qwen2.5-Coder-1.5B-Instruct) ───────────────────────────

/// Persisted settings for the local on-device template-generation model.
/// The feature is opt-in and off by default — the 1.5B model needs ~1 GB
/// RAM and a capable CPU or GPU. When disabled, the API panel's "AI
/// Generate" button is hidden entirely.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QwenLocalSettings {
    /// Master toggle. When false, no local model is loaded and the UI
    /// button is hidden.
    #[serde(default)]
    pub enabled: bool,
    /// Whether the user has acknowledged the hardware-capability warning.
    /// Set to true when the user clicks "Enable anyway" on a low-spec
    /// machine. The UI still shows the warning text but allows enabling.
    #[serde(default)]
    pub force_enabled: bool,
    /// HuggingFace mirror endpoint used for downloads.
    /// Defaults to `"https://hf-mirror.com"` (China mirror). Users behind
    /// the GFW or with slow HuggingFace connectivity can keep the default;
    /// users with direct access can set `"https://huggingface.co"`.
    /// The `HF_ENDPOINT` env var still takes priority inside the download
    /// layer when the user has not customized this field.
    #[serde(default = "default_mirror_url")]
    pub mirror_url: String,
    /// Currently selected model id — matches either a [`builtin_models`]
    /// preset id or a [`QwenLocalSettings::custom_models`] entry id.
    /// Defaults to `"qwen25-coder-1.5b"`.
    #[serde(default = "default_model_id")]
    pub active_model_id: String,
    /// User-defined custom models. Empty by default. Builtins are not
    /// duplicated here — the UI merges [`builtin_models`] with this list.
    #[serde(default)]
    pub custom_models: Vec<ModelConfig>,
}

/// Default mirror endpoint — the HuggingFace China mirror.
fn default_mirror_url() -> String {
    "https://hf-mirror.com".to_string()
}

/// Default active model id — the Qwen2.5-Coder-1.5B preset.
fn default_model_id() -> String {
    "qwen25-coder-1.5b".to_string()
}

impl Default for QwenLocalSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            force_enabled: false,
            mirror_url: default_mirror_url(),
            active_model_id: default_model_id(),
            custom_models: Vec::new(),
        }
    }
}

/// Configuration for a single local model (builtin preset or user-defined).
///
/// All Qwen2-family models (Qwen2.5-Coder, Qwen2, Qwen2.5) share the
/// `qwen2` architecture and the `<|im_start|>` chat template, so switching
/// between them only requires changing `id`/`name`/`repo_id`. Models with
/// other architectures are rejected at load time with a clear error —
/// future versions may expand the supported architecture set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelConfig {
    /// Unique stable id, e.g. `"qwen25-coder-1.5b"` or `"my-custom-model"`.
    /// Used to derive the GGUF cache filename (`{id}-q4k.gguf`).
    pub id: String,
    /// Display name shown in the UI model selector.
    pub name: String,
    /// HuggingFace repo id, e.g. `"Qwen/Qwen2.5-Coder-1.5B-Instruct"`.
    pub repo_id: String,
    /// Architecture family. Only `"qwen2"` is currently supported by the
    /// candle `quantized_qwen2` loader. Other values produce a clear error
    /// at load time.
    #[serde(default = "default_model_arch")]
    pub architecture: String,
    /// Chat template with a single `{prompt}` placeholder. Qwen2-family
    /// models use `<|im_start|>user\n{prompt}<|im_end|>\n<|im_start|>assistant\n`.
    pub prompt_template: String,
    /// End-of-sequence token, e.g. `"<|im_end|>"`.
    pub eos_token: String,
}

/// Default architecture — only `qwen2` is currently supported.
fn default_model_arch() -> String {
    "qwen2".to_string()
}

/// Built-in model presets. All use the `qwen2` architecture so they work
/// with the existing candle `quantized_qwen2` loader.
///
/// The first entry is the default fallback when `active_model_id` doesn't
/// match any builtin or custom model.
pub fn builtin_models() -> Vec<ModelConfig> {
    let qwen_template = "<|im_start|>user\n{prompt}<|im_end|>\n<|im_start|>assistant\n".to_string();
    let qwen_eos = "<|im_end|>".to_string();
    vec![
        ModelConfig {
            id: "qwen25-coder-1.5b".into(),
            name: "Qwen2.5-Coder-1.5B-Instruct".into(),
            repo_id: "Qwen/Qwen2.5-Coder-1.5B-Instruct".into(),
            architecture: "qwen2".into(),
            prompt_template: qwen_template.clone(),
            eos_token: qwen_eos.clone(),
        },
        ModelConfig {
            id: "qwen25-coder-0.5b".into(),
            name: "Qwen2.5-Coder-0.5B-Instruct".into(),
            repo_id: "Qwen/Qwen2.5-Coder-0.5B-Instruct".into(),
            architecture: "qwen2".into(),
            prompt_template: qwen_template.clone(),
            eos_token: qwen_eos.clone(),
        },
        ModelConfig {
            id: "qwen2-1.5b".into(),
            name: "Qwen2-1.5B-Instruct".into(),
            repo_id: "Qwen/Qwen2-1.5B-Instruct".into(),
            architecture: "qwen2".into(),
            prompt_template: qwen_template.clone(),
            eos_token: qwen_eos.clone(),
        },
    ]
}

/// Resolve the active model from settings. Searches builtin presets first,
/// then custom models. Falls back to the first builtin if the id is unknown
/// (e.g. a custom model was deleted but `active_model_id` still points to
/// it).
pub fn resolve_model(settings: &QwenLocalSettings) -> ModelConfig {
    let builtins = builtin_models();
    if let Some(m) = builtins.iter().find(|m| m.id == settings.active_model_id) {
        return m.clone();
    }
    if let Some(m) = settings
        .custom_models
        .iter()
        .find(|m| m.id == settings.active_model_id)
    {
        return m.clone();
    }
    // Fallback: first builtin (Qwen2.5-Coder-1.5B by convention).
    builtins
        .into_iter()
        .next()
        .expect("builtin_models() must return at least one preset")
}

// --- OneKeys (ZOC-style Expect/Send auto-fill) ---

/// A remembered selection for one connection and one normalized prompt.
/// `prompt_fingerprint` is a SHA-256 digest produced by the UI, so remote
/// prompt text is not written to settings.json. `step_id` is generated once and persisted with the encrypted step; stale
/// references safely fall back to the chooser.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OneKeyPreference {
    pub connection_id: String,
    pub prompt_fingerprint: String,
    pub onekey_id: String,
    /// Stable step identity. Empty only when deserializing an unreleased legacy
    /// index-based preference, which safely falls back to the chooser.
    #[serde(default)]
    pub step_id: String,
    /// Compatibility for early index-based preference records. New records
    /// never write this field and never use it for credential selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_index: Option<usize>,
}

/// Built-in matcher for password and SSH key passphrase prompts. Matching is
/// case-insensitive and runs against the terminal's current prompt line.
pub const DEFAULT_ONEKEY_PASSWORD_EXPECT: &str =
    r"(?:password(?:\s+for\s+[^\r\n]+)?|passphrase(?:\s+for\s+(?:key\s+)?[^\r\n]+)?):\s*$";

/// Built-in matcher for Git HTTPS username prompts.
pub const DEFAULT_ONEKEY_USERNAME_EXPECT: &str = r"Username for \S+:";

/// In-memory OneKey entry: a named sequence of Expect/Send steps.
/// When terminal output matches a step's `expect`, that step's `send` is offered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OneKey {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub steps: Vec<OneKeyStep>,
}

// `OneKeyStep` holds the `send` value in memory as plaintext (so it can be sent
// to the terminal when the step matches). Its `Debug` impl redacts `send` so
// accidental `tracing::debug!(?step)` calls don't leak credentials into logs.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct OneKeyStep {
    /// Stable identity used by remembered multi-match selections. Legacy
    /// entries receive and persist a UUID when first loaded.
    #[serde(default)]
    pub id: String,
    /// Display label, e.g. "Username" / "Password".
    #[serde(default)]
    pub label: String,
    /// Regex matched against terminal output.
    pub expect: String,
    /// Value to send when this step matches (plaintext in memory).
    pub send: String,
}

impl std::fmt::Debug for OneKeyStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneKeyStep")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("expect", &self.expect)
            .field("send", &"<redacted>")
            .finish()
    }
}

/// Persisted OneKey entry. Each step's `send` is encrypted at rest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedOneKey {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub steps: Vec<PersistedOneKeyStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedOneKeyStep {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    pub expect: String,
    pub send: EncryptedValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedConnection {
    pub id: String,
    pub name: String,
    pub kind: PersistedConnectionKind,
    pub group: Option<String>,
    pub tags: Vec<String>,
    pub onekey: bool,
    /// Raw login-script DSL text; plaintext, so it only ever references
    /// OneKey entries by name and never a credential itself.
    #[serde(default)]
    pub login_script: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PersistedConnectionKind {
    Ssh(PersistedSshConfig),
    Serial(SerialConfig),
    Telnet(TelnetConfig),
    Shell(ShellConfig),
    Tcp(TcpConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: PersistedSshAuth,
    pub terminal_type: String,
    #[serde(default)]
    pub proxy: Option<PersistedProxyConfig>,
    pub proxy_jump: Option<String>,
    pub keepalive_interval: Option<u64>,
    #[serde(default = "default_host_key_policy")]
    pub host_key_policy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedProxyConfig {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<EncryptedValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PersistedSshAuth {
    Password {
        password: EncryptedValue,
    },
    Key {
        private_key_path: String,
        passphrase: Option<EncryptedValue>,
    },
    Agent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_serialization_roundtrip() {
        let config = ConnectionConfig {
            id: "test-1".to_string(),
            name: "Test Server".to_string(),
            kind: ConnectionKind::Ssh(SshConfig {
                host: "192.168.1.1".to_string(),
                port: 22,
                username: "root".to_string(),
                auth: SshAuth::Password {
                    password: "secret".to_string(),
                },
                terminal_type: "xterm-256color".to_string(),
                proxy: None,
                proxy_jump: None,
                keepalive_interval: Some(30),
                host_key_policy: default_host_key_policy(),
            }),
            group: Some("Production".to_string()),
            tags: vec!["linux".to_string(), "prod".to_string()],
            onekey: true,
            login_script: Some("expect Password:\nsend hunter2\n".to_string()),
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ConnectionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn otp_webhook_feishubot_roundtrip() {
        let cfg = OtpWebhookConfig::Feishubot {
            app_id: "cli_xxx".to_string(),
            app_secret: "secret".to_string(),
            chat_id: "oc_yyy".to_string(),
            code_pattern: default_otp_code_pattern(),
            sender_open_id: Some("ou_zzz".to_string()),
            max_age_secs: 90,
            base_url: default_feishu_base_url(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        // Tagged enum must serialize with `kind`.
        assert!(json.contains("\"kind\":\"feishubot\""));
        let back: OtpWebhookConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn otp_webhook_http_roundtrip() {
        let cfg = OtpWebhookConfig::Http {
            url: "https://totp.example.local/current".to_string(),
            method: "post".to_string(),
            body: Some("{}".to_string()),
            headers: vec![("Authorization".to_string(), "Bearer x".to_string())],
            code_pattern: r"(\d{6})".to_string(),
            timeout_secs: 5,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"kind\":\"http\""));
        let back: OtpWebhookConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn otp_webhook_manual_tagged_serialization() {
        let json = serde_json::to_string(&OtpWebhookConfig::Manual).unwrap();
        assert_eq!(json, "{\"kind\":\"manual\"}");
        let back: OtpWebhookConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, OtpWebhookConfig::Manual);
    }

    #[test]
    fn otp_webhook_feishubot_uses_defaults_when_fields_omitted() {
        // Legacy/partial settings files may omit the optional fields — they
        // must fall back to the documented defaults, not fail to parse.
        let json = r#"{
            "kind":"feishubot",
            "app_id":"cli_x",
            "app_secret":"s",
            "chat_id":"oc_y"
        }"#;
        let cfg: OtpWebhookConfig = serde_json::from_str(json).unwrap();
        match cfg {
            OtpWebhookConfig::Feishubot {
                code_pattern,
                max_age_secs,
                base_url,
                sender_open_id,
                ..
            } => {
                assert_eq!(code_pattern, default_otp_code_pattern());
                assert_eq!(max_age_secs, default_otp_max_age_secs());
                assert_eq!(base_url, default_feishu_base_url());
                assert!(sender_open_id.is_none());
            }
            other => panic!("expected Feishubot, got {:?}", other),
        }
    }

    #[test]
    fn persisted_config_without_otp_webhook_field_loads_as_none() {
        // A settings file written before this feature shipped must still load
        // — the missing `otp_webhook` field defaults to `None` via
        // `#[serde(default)]`.
        let json = r#"{
            "version":1,
            "connections":[]
        }"#;
        let cfg: PersistedConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.otp_webhook.is_none());
    }

    #[test]
    fn legacy_ssh_config_without_proxy_defaults_to_direct_connection() {
        let json = r#"{
            "host":"example.com",
            "port":22,
            "username":"alice",
            "auth":"Agent",
            "terminal_type":"xterm-256color",
            "proxy_jump":null,
            "keepalive_interval":null
        }"#;

        let config: SshConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.proxy, None);
        assert_eq!(config.host_key_policy, default_host_key_policy());
    }

    #[test]
    fn proxy_debug_redacts_password() {
        let proxy = ProxyConfig {
            kind: ProxyKind::Https,
            host: "proxy.example".to_string(),
            port: 443,
            username: Some("alice".to_string()),
            password: Some("proxy-secret".to_string()),
        };

        let debug = format!("{proxy:?}");
        assert!(!debug.contains("proxy-secret"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn test_config_toml_roundtrip() {
        let config = ConnectionConfig {
            id: "test-2".to_string(),
            name: "Serial Device".to_string(),
            kind: ConnectionKind::Serial(SerialConfig {
                port: "/dev/ttyUSB0".to_string(),
                baud_rate: 115200,
                data_bits: 8,
                parity: "none".to_string(),
                stop_bits: 1,
                flow_control: "none".to_string(),
            }),
            group: None,
            tags: vec![],
            onekey: false,
            login_script: None,
        };

        let toml_str = toml::to_string(&config).unwrap();
        let deserialized: ConnectionConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_all_connection_kinds() {
        let configs = vec![
            ConnectionKind::Ssh(SshConfig {
                host: "host".to_string(),
                port: 22,
                username: "user".to_string(),
                auth: SshAuth::Agent,
                terminal_type: "xterm-256color".to_string(),
                proxy: None,
                proxy_jump: None,
                keepalive_interval: None,
                host_key_policy: default_host_key_policy(),
            }),
            ConnectionKind::Serial(SerialConfig {
                port: "/dev/ttyS0".to_string(),
                baud_rate: 9600,
                data_bits: 8,
                parity: "none".to_string(),
                stop_bits: 1,
                flow_control: "none".to_string(),
            }),
            ConnectionKind::Telnet(TelnetConfig {
                host: "host".to_string(),
                port: 23,
            }),
            ConnectionKind::Shell(ShellConfig {
                command: Some("/bin/bash".to_string()),
                args: vec![],
                env: vec![],
                working_dir: None,
            }),
            ConnectionKind::Tcp(TcpConfig {
                host: "host".to_string(),
                port: 8080,
            }),
        ];

        for kind in configs {
            let json = serde_json::to_string(&kind).unwrap();
            let de: ConnectionKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, de);
        }
    }

    #[test]
    fn sidebar_preferences_default_for_legacy_settings() {
        let config: PersistedConfig =
            serde_json::from_str(r#"{"version":1,"connections":[]}"#).unwrap();

        assert_eq!(config.sidebar, SidebarPreferences::default());
    }

    #[test]
    fn sidebar_preferences_normalize_width_and_duplicate_ids() {
        let preferences = SidebarPreferences {
            width_px: 10,
            hidden_connection_ids: vec!["alpha".to_string(), "alpha".to_string(), String::new()],
            groups: vec![
                ConnectionGroup {
                    id: "group-a".to_string(),
                    name: " Group A ".to_string(),
                    collapsed: true,
                },
                ConnectionGroup {
                    id: "group-a".to_string(),
                    name: "Duplicate".to_string(),
                    collapsed: false,
                },
            ],
        }
        .normalized();

        assert_eq!(preferences.width_px, MIN_SIDEBAR_WIDTH_PX);
        assert_eq!(preferences.hidden_connection_ids, vec!["alpha"]);
        assert_eq!(preferences.groups.len(), 1);
        assert_eq!(preferences.groups[0].name, "Group A");
        assert!(preferences.groups[0].collapsed);
    }

    #[test]
    fn sidebar_preferences_roundtrip() {
        let preferences = SidebarPreferences {
            width_px: 412,
            hidden_connection_ids: vec!["hidden-connection".to_string()],
            groups: vec![ConnectionGroup {
                id: "production".to_string(),
                name: "Production".to_string(),
                collapsed: true,
            }],
        };
        let json = serde_json::to_string(&preferences).unwrap();
        let parsed: SidebarPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, preferences);
    }

    #[test]
    fn workspace_preferences_default_for_legacy_settings() {
        let config: PersistedConfig =
            serde_json::from_str(r#"{"version":1,"connections":[]}"#).unwrap();

        assert_eq!(config.workspace, WorkspacePreferences::default());
    }

    #[test]
    fn workspace_preferences_normalize_sizes_and_roundtrip_tabs() {
        let preferences = WorkspacePreferences {
            left_visible: false,
            right_visible: true,
            bottom_visible: false,
            right_width_px: 10,
            bottom_height_px: u16::MAX,
            left_tab: LeftPanelTab::Files,
            right_tab: RightPanelTab::History,
            bottom_tab: BottomPanelTab::Transfers,
            ..WorkspacePreferences::default()
        }
        .normalized();

        assert_eq!(preferences.right_width_px, MIN_RIGHT_PANEL_WIDTH_PX);
        assert_eq!(preferences.bottom_height_px, MAX_BOTTOM_PANEL_HEIGHT_PX);
        assert_eq!(
            preferences.dock_layout.left.active,
            Some(PanelId::RemoteFiles)
        );
        assert_eq!(preferences.dock_layout.right.active, Some(PanelId::History));
        assert_eq!(
            preferences.dock_layout.bottom.active,
            Some(PanelId::Transfers)
        );

        let json = serde_json::to_string(&preferences).unwrap();
        let parsed: WorkspacePreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, preferences);
    }

    #[test]
    fn workspace_preferences_migrate_legacy_json_into_dock_layout() {
        let preferences: WorkspacePreferences = serde_json::from_str(
            r#"{
                "left_visible": false,
                "right_visible": true,
                "bottom_visible": false,
                "right_width_px": 410,
                "bottom_height_px": 315,
                "left_tab": "files",
                "right_tab": "history",
                "bottom_tab": "shell"
            }"#,
        )
        .unwrap();
        let preferences = preferences.normalized();

        assert_eq!(
            preferences.dock_layout.left.panels,
            vec![PanelId::Connections, PanelId::RemoteFiles]
        );
        assert_eq!(
            preferences.dock_layout.left.active,
            Some(PanelId::RemoteFiles)
        );
        assert!(!preferences.dock_layout.left.visible);
        assert_eq!(preferences.dock_layout.right.active, Some(PanelId::History));
        assert_eq!(preferences.dock_layout.right.extent_px, 410);
        assert_eq!(
            preferences.dock_layout.bottom.active,
            Some(PanelId::EmbeddedShell)
        );
        assert!(!preferences.dock_layout.bottom.visible);
        assert_eq!(preferences.dock_layout.bottom.extent_px, 315);
    }

    #[test]
    fn dock_layout_reorders_within_a_zone() {
        let mut layout = DockLayout::default();
        layout.move_panel(PanelId::RemoteFiles, DockZone::Left, 0);

        assert_eq!(
            layout.left.panels,
            vec![PanelId::RemoteFiles, PanelId::Connections]
        );
        assert_eq!(layout.left.active, Some(PanelId::Connections));
    }

    #[test]
    fn dock_layout_moves_panels_across_zones() {
        let mut layout = DockLayout::default();
        layout.move_panel(PanelId::Connections, DockZone::Right, 1);

        assert_eq!(layout.left.panels, vec![PanelId::RemoteFiles]);
        assert_eq!(layout.left.active, Some(PanelId::RemoteFiles));
        assert_eq!(
            layout.right.panels,
            vec![PanelId::Sessions, PanelId::Connections, PanelId::History]
        );
        assert_eq!(layout.right.active, Some(PanelId::Connections));
        assert_eq!(layout.zone_for(PanelId::Connections), Some(DockZone::Right));
    }

    #[test]
    fn moving_to_a_hidden_edge_reopens_it() {
        let mut layout = DockLayout::default();
        layout.hide_zone(DockZone::Right);
        layout.move_panel(PanelId::Connections, DockZone::Right, 0);

        assert!(layout.right.visible);
        assert_eq!(layout.right.active, Some(PanelId::Connections));
        assert_eq!(layout.right.panels.first(), Some(&PanelId::Connections));
    }

    #[test]
    fn dock_layout_normalize_repairs_duplicates_missing_panels_active_and_extents() {
        let mut layout = DockLayout {
            left: DockStackState {
                panels: vec![PanelId::Sessions, PanelId::Sessions],
                active: Some(PanelId::History),
                visible: true,
                extent_px: 0,
            },
            right: DockStackState {
                panels: vec![PanelId::Sessions, PanelId::Connections],
                active: Some(PanelId::Sessions),
                visible: false,
                extent_px: u16::MAX,
            },
            bottom: DockStackState {
                panels: Vec::new(),
                active: Some(PanelId::Connections),
                visible: true,
                extent_px: u16::MAX,
            },
        };

        layout.normalize();

        for panel in PanelId::ALL {
            let occurrences = DockZone::ALL
                .into_iter()
                .filter(|zone| layout.stack(*zone).panels.contains(&panel))
                .count();
            assert_eq!(occurrences, 1, "{panel:?} must occur exactly once");
        }
        assert_eq!(layout.left.active, Some(PanelId::Sessions));
        assert_eq!(layout.right.active, Some(PanelId::Connections));
        assert_eq!(layout.bottom.active, Some(PanelId::Send));
        assert_eq!(layout.left.extent_px, MIN_SIDEBAR_WIDTH_PX);
        assert_eq!(layout.right.extent_px, MAX_RIGHT_PANEL_WIDTH_PX);
        assert_eq!(layout.bottom.extent_px, MAX_BOTTOM_PANEL_HEIGHT_PX);
        assert!(!layout.right.visible);

        let normalized = layout.clone();
        layout.normalize();
        assert_eq!(layout, normalized);
    }

    #[test]
    fn focused_tab_appearance_defaults_for_legacy_settings() {
        let config: PersistedConfig =
            serde_json::from_str(r#"{"version":1,"connections":[]}"#).unwrap();

        assert_eq!(
            config.focused_tab_appearance,
            FocusedTabAppearance::default()
        );
    }

    #[test]
    fn login_script_defaults_to_none_for_legacy_persisted_connection() {
        // Legacy settings.json connections predate the login_script field and
        // must deserialize with `login_script: None`.
        let json = r#"{
            "id":"c1",
            "name":"Router",
            "kind":{"Serial":{"port":"/dev/ttyUSB0","baud_rate":115200,"data_bits":8,"parity":"none","stop_bits":1,"flow_control":"none"}},
            "group":null,
            "tags":[],
            "onekey":false
        }"#;

        let conn: PersistedConnection = serde_json::from_str(json).unwrap();
        assert_eq!(conn.login_script, None);

        let json = serde_json::to_string(&serde_json::json!({
            "id": "c1",
            "name": "Router",
            "kind": {"Serial": {"port": "/dev/ttyUSB0", "baud_rate": 115200, "data_bits": 8, "parity": "none", "stop_bits": 1, "flow_control": "none"}},
            "group": null,
            "tags": [],
            "onekey": false,
            "login_script": "expect Password:\nsend hunter2\n"
        }))
        .unwrap();
        let conn: PersistedConnection = serde_json::from_str(&json).unwrap();
        assert_eq!(
            conn.login_script,
            Some("expect Password:\nsend hunter2\n".to_string())
        );
    }

    #[test]
    fn login_script_roundtrips_on_connection_config() {
        let mut conn = ConnectionConfig {
            id: "c1".to_string(),
            name: "Router".to_string(),
            kind: ConnectionKind::Serial(SerialConfig {
                port: "/dev/ttyUSB0".to_string(),
                baud_rate: 115200,
                data_bits: 8,
                parity: "none".to_string(),
                stop_bits: 1,
                flow_control: "none".to_string(),
            }),
            group: None,
            tags: vec![],
            onekey: false,
            login_script: Some("expect Password:\nsend hunter2\n".to_string()),
        };

        let json = serde_json::to_string(&conn).unwrap();
        let parsed: ConnectionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, conn);

        // Legacy ConnectionConfig JSON (no login_script key) must still load.
        conn.login_script = None;
        let parsed: ConnectionConfig = serde_json::from_str(
            r#"{"id":"c1","name":"Router","kind":{"Serial":{"port":"/dev/ttyUSB0","baud_rate":115200,"data_bits":8,"parity":"none","stop_bits":1,"flow_control":"none"}},"group":null,"tags":[],"onekey":false}"#,
        )
        .unwrap();
        assert_eq!(parsed, conn);
    }

    #[test]
    fn suggestion_settings_default_for_legacy_settings() {
        // A legacy settings.json that predates the suggestion fields should
        // deserialize with sensible defaults (enabled=true, count=3).
        let config: PersistedConfig =
            serde_json::from_str(r#"{"version":1,"connections":[]}"#).unwrap();
        assert!(config.suggestion_enabled);
        assert_eq!(config.suggestion_count, 3);
    }

    #[test]
    fn comparison_diff_warning_defaults_to_enabled_for_legacy_settings() {
        let config: PersistedConfig =
            serde_json::from_str(r#"{"version":1,"connections":[]}"#).unwrap();

        assert!(config.comparison_diff_warning_enabled);
    }

    #[test]
    fn comparison_diff_warning_roundtrips() {
        let config: PersistedConfig = serde_json::from_str(
            r#"{"version":1,"connections":[],"comparison_diff_warning_enabled":false}"#,
        )
        .unwrap();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: PersistedConfig = serde_json::from_str(&json).unwrap();

        assert!(!parsed.comparison_diff_warning_enabled);
    }

    #[test]
    fn keybindings_default_for_legacy_settings() {
        let config: PersistedConfig =
            serde_json::from_str(r#"{"version":1,"connections":[]}"#).unwrap();

        assert_eq!(config.keybindings, Keybindings::default());
    }

    #[test]
    fn skin_defaults_for_legacy_settings_and_normalizes_custom_colors() {
        let config: PersistedConfig =
            serde_json::from_str(r#"{"version":1,"connections":[]}"#).unwrap();
        assert_eq!(config.skin, SkinSettings::default());

        let skin = SkinSettings {
            kind: SkinKind::Custom,
            custom: SkinPalette {
                accent: "not-a-color;display:none".to_string(),
                ..SkinPalette::default()
            },
            ..SkinSettings::default()
        }
        .normalized();
        assert_eq!(skin.custom.accent, SkinPalette::default().accent);
    }

    #[test]
    fn theme_mode_defaults_to_dark_for_legacy_settings() {
        let config: PersistedConfig =
            serde_json::from_str(r#"{"version":1,"connections":[]}"#).unwrap();
        assert_eq!(config.skin.mode, ThemeMode::Dark);
    }

    #[test]
    fn theme_mode_system_resolves_to_os_preference() {
        assert_eq!(ThemeMode::System.resolve(true), ThemeMode::Dark);
        assert_eq!(ThemeMode::System.resolve(false), ThemeMode::Light);
        assert_eq!(ThemeMode::Dark.resolve(false), ThemeMode::Dark);
        assert_eq!(ThemeMode::Light.resolve(true), ThemeMode::Light);
    }

    #[test]
    fn palette_picks_light_variant_when_mode_resolves_to_light() {
        let dark = SkinSettings {
            kind: SkinKind::TokyoNight,
            mode: ThemeMode::Dark,
            ..SkinSettings::default()
        };
        let light = SkinSettings {
            kind: SkinKind::TokyoNight,
            mode: ThemeMode::Light,
            ..SkinSettings::default()
        };
        // Dark mode never consults the OS flag.
        assert_eq!(dark.palette(true), SkinPalette::tokyo_night());
        assert_eq!(dark.palette(false), SkinPalette::tokyo_night());
        // Light mode picks the light variant regardless of the OS flag.
        assert_eq!(light.palette(true), SkinPalette::tokyo_night_light());
        assert_eq!(light.palette(false), SkinPalette::tokyo_night_light());
    }

    #[test]
    fn palette_system_mode_follows_os_preference() {
        let system = SkinSettings {
            kind: SkinKind::OneDark,
            mode: ThemeMode::System,
            ..SkinSettings::default()
        };
        assert_eq!(system.palette(true), SkinPalette::one_dark());
        assert_eq!(system.palette(false), SkinPalette::one_light());
    }

    #[test]
    fn palette_custom_uses_the_resolved_variant_slot() {
        let mut skin = SkinSettings {
            kind: SkinKind::Custom,
            mode: ThemeMode::System,
            ..SkinSettings::default()
        };
        skin.custom.accent = "#111111".to_string();
        skin.custom_light.accent = "#222222".to_string();
        assert_eq!(skin.palette(true).accent, "#111111");
        assert_eq!(skin.palette(false).accent, "#222222");
    }

    #[test]
    fn skin_settings_roundtrip_preserves_mode_and_custom_light() {
        let skin = SkinSettings {
            kind: SkinKind::Custom,
            mode: ThemeMode::System,
            custom: SkinPalette::default(),
            custom_light: SkinPalette::one_light(),
        };
        let json = serde_json::to_string(&skin).unwrap();
        let restored: SkinSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, skin);
        // Legacy configs (no mode / custom_light fields) keep working.
        let legacy: SkinSettings =
            serde_json::from_str(r#"{"kind":"tokyo_night","custom":{}}"#).unwrap();
        assert_eq!(legacy.mode, ThemeMode::Dark);
        assert_eq!(legacy.custom_light, default_custom_light_palette());
    }

    #[test]
    fn keybindings_normalize_unsafe_manual_values() {
        let keybindings = Keybindings {
            close_focused_pane: Some(KeyChord {
                key: " W ".to_string(),
                primary: true,
                alt: false,
                shift: true,
            }),
            append_pane: Some(KeyChord {
                key: "c".to_string(),
                primary: true,
                alt: false,
                shift: false,
            }),
            ..Keybindings::default()
        }
        .normalized();

        assert_eq!(keybindings.close_focused_pane.unwrap().key, "w");
        assert!(keybindings.append_pane.is_none());
    }

    #[test]
    fn keybindings_detect_conflicting_actions() {
        let keybindings = Keybindings::default();
        let chord = keybindings.append_pane.as_ref().unwrap();
        assert_eq!(
            keybindings.conflicting_action(KeybindingAction::TogglePaneZoom, chord),
            Some(KeybindingAction::AppendPane)
        );
    }

    #[test]
    fn suggestion_settings_roundtrip() {
        let config = PersistedConfig {
            version: 1,
            connections: vec![],
            onekeys: vec![],
            onekey_preferences: vec![],
            master_password_hash: None,
            restore_disabled: false,
            confirm_close_on_exit: true,
            comparison_diff_warning_enabled: true,
            focused_tab_appearance: FocusedTabAppearance::default(),
            suggestion_enabled: false,
            suggestion_count: 10,
            sidebar: SidebarPreferences::default(),
            workspace: WorkspacePreferences::default(),
            keybindings: Keybindings::default(),
            skin: SkinSettings::default(),
            collect_usage_habits: false,
            language: Language::default(),
            api_custom_templates: Vec::new(),
            qwen_local: QwenLocalSettings::default(),
            suggestion_popup_offset_y: -38.5,
            otp_webhook: None,
            chat: ChatSettings::default(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: PersistedConfig = serde_json::from_str(&json).unwrap();
        assert!(!parsed.suggestion_enabled);
        assert_eq!(parsed.suggestion_count, 10);
        assert_eq!(parsed.suggestion_popup_offset_y, -38.5);
    }

    #[test]
    fn suggestion_popup_offset_defaults_to_automatic_for_legacy_settings() {
        // Settings files written before the drag-to-move popup feature must
        // deserialize with the automatic-placement default (0.0).
        let config: PersistedConfig =
            serde_json::from_str(r#"{"version":1,"connections":[]}"#).unwrap();
        assert_eq!(config.suggestion_popup_offset_y, 0.0);
    }

    #[test]
    fn collect_usage_habits_defaults_off_for_legacy_settings() {
        // A legacy settings.json that predates the usage-habits field must
        // deserialize with collection DISABLED (privacy-safe opt-in default).
        let config: PersistedConfig =
            serde_json::from_str(r#"{"version":1,"connections":[]}"#).unwrap();
        assert!(!config.collect_usage_habits);

        // And an explicit true must round-trip.
        let json = serde_json::to_string(&PersistedConfig {
            collect_usage_habits: true,
            ..config
        })
        .unwrap();
        let parsed: PersistedConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.collect_usage_habits);
    }

    #[test]
    fn api_custom_templates_default_empty_for_legacy_settings_and_roundtrip() {
        // A legacy settings.json that predates custom API templates must
        // deserialize with an empty list.
        let config: PersistedConfig =
            serde_json::from_str(r#"{"version":1,"connections":[]}"#).unwrap();
        assert!(config.api_custom_templates.is_empty());

        // Templates for every mode must round-trip.
        let json = serde_json::to_string(&PersistedConfig {
            api_custom_templates: vec![
                CustomApiTemplate {
                    label: "check disk".to_string(),
                    mode: ApiTemplateMode::Command,
                    body: "df -h /data".to_string(),
                },
                CustomApiTemplate {
                    label: "restart svc".to_string(),
                    mode: ApiTemplateMode::Script,
                    body: "#!/bin/sh\nsystemctl restart my-svc".to_string(),
                },
                CustomApiTemplate {
                    label: "b64".to_string(),
                    mode: ApiTemplateMode::ScriptBase64,
                    body: "IyEvYmluL3NoCnVwdGltZQ==".to_string(),
                },
            ],
            ..config
        })
        .unwrap();
        let parsed: PersistedConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.api_custom_templates.len(), 3);
        assert_eq!(
            parsed.api_custom_templates[0].mode,
            ApiTemplateMode::Command
        );
        assert_eq!(parsed.api_custom_templates[1].mode, ApiTemplateMode::Script);
        assert_eq!(
            parsed.api_custom_templates[2].mode,
            ApiTemplateMode::ScriptBase64
        );
        assert_eq!(parsed.api_custom_templates[1].label, "restart svc");
        assert_eq!(
            parsed.api_custom_templates[1].body,
            "#!/bin/sh\nsystemctl restart my-svc"
        );
    }

    #[test]
    fn focused_tab_appearance_normalizes_untrusted_values() {
        let appearance = FocusedTabAppearance {
            border_color: "red; display: none".to_string(),
            border_width: 99,
            border_radius: 99,
        }
        .normalized();

        assert_eq!(appearance.border_color, "#c0caf5");
        assert_eq!(appearance.border_width, 4);
        assert_eq!(appearance.border_radius, 12);
    }

    #[test]
    fn test_ssh_auth_variants() {
        let auths = vec![
            SshAuth::Password {
                password: "pass".to_string(),
            },
            SshAuth::Key {
                private_key_path: "/path/to/key".to_string(),
                passphrase: Some("secret".to_string()),
            },
            SshAuth::Agent,
        ];

        for auth in auths {
            let json = serde_json::to_string(&auth).unwrap();
            let de: SshAuth = serde_json::from_str(&json).unwrap();
            assert_eq!(auth, de);
        }
    }

    #[test]
    fn qwen_local_settings_defaults_and_legacy() {
        // Default settings: disabled, default mirror, default model.
        let s = QwenLocalSettings::default();
        assert!(!s.enabled);
        assert!(!s.force_enabled);
        assert_eq!(s.mirror_url, "https://hf-mirror.com");
        assert_eq!(s.active_model_id, "qwen25-coder-1.5b");
        assert!(s.custom_models.is_empty());

        // Legacy settings.json (pre-Phase-2) with only enabled/force_enabled
        // must deserialize with the new defaults filled in.
        let legacy = r#"{"enabled":true,"force_enabled":true}"#;
        let parsed: QwenLocalSettings = serde_json::from_str(legacy).unwrap();
        assert!(parsed.enabled);
        assert!(parsed.force_enabled);
        assert_eq!(parsed.mirror_url, "https://hf-mirror.com");
        assert_eq!(parsed.active_model_id, "qwen25-coder-1.5b");
        assert!(parsed.custom_models.is_empty());
    }

    #[test]
    fn qwen_local_settings_roundtrip_with_custom_model() {
        let settings = QwenLocalSettings {
            enabled: true,
            force_enabled: false,
            mirror_url: "https://huggingface.co".to_string(),
            active_model_id: "my-model".to_string(),
            custom_models: vec![ModelConfig {
                id: "my-model".to_string(),
                name: "My Custom Model".to_string(),
                repo_id: "org/custom-model".to_string(),
                architecture: "qwen2".to_string(),
                prompt_template: "<|im_start|>user\n{prompt}<|im_end|>\n<|im_start|>assistant\n"
                    .to_string(),
                eos_token: "<|im_end|>".to_string(),
            }],
        };
        let json = serde_json::to_string(&settings).unwrap();
        let parsed: QwenLocalSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(settings, parsed);
    }

    #[test]
    fn builtin_models_non_empty_with_default_first() {
        let models = builtin_models();
        assert!(!models.is_empty());
        // The first preset must be the Qwen2.5-Coder-1.5B (the default fallback).
        assert_eq!(models[0].id, "qwen25-coder-1.5b");
        assert_eq!(models[0].repo_id, "Qwen/Qwen2.5-Coder-1.5B-Instruct");
        // All builtins must use qwen2 architecture.
        for m in &models {
            assert_eq!(m.architecture, "qwen2");
            assert!(m.prompt_template.contains("{prompt}"));
        }
    }

    #[test]
    fn resolve_model_finds_builtin_and_custom_and_falls_back() {
        // Builtin: found by id.
        let s = QwenLocalSettings {
            active_model_id: "qwen25-coder-0.5b".to_string(),
            ..Default::default()
        };
        let m = resolve_model(&s);
        assert_eq!(m.id, "qwen25-coder-0.5b");
        assert_eq!(m.repo_id, "Qwen/Qwen2.5-Coder-0.5B-Instruct");

        // Custom: found by id.
        let s = QwenLocalSettings {
            active_model_id: "my-custom".to_string(),
            custom_models: vec![ModelConfig {
                id: "my-custom".to_string(),
                name: "My Custom".to_string(),
                repo_id: "org/custom".to_string(),
                architecture: "qwen2".to_string(),
                prompt_template: "{prompt}".to_string(),
                eos_token: "<end>".to_string(),
            }],
            ..Default::default()
        };
        let m = resolve_model(&s);
        assert_eq!(m.id, "my-custom");
        assert_eq!(m.repo_id, "org/custom");

        // Unknown id: falls back to first builtin.
        let s = QwenLocalSettings {
            active_model_id: "does-not-exist".to_string(),
            ..Default::default()
        };
        let m = resolve_model(&s);
        assert_eq!(m.id, "qwen25-coder-1.5b");
    }

    // ── Agent chat box (issue #122) ─────────────────────────────────────
    #[test]
    fn chat_settings_default_has_one_openai_agent_selected() {
        let s = ChatSettings::default().normalized();
        assert_eq!(s.agents.len(), 1);
        assert_eq!(s.agents[0].id, "default");
        assert_eq!(s.agents[0].provider, ChatAgentProvider::OpenAI);
        assert_eq!(s.active_agent_id.as_deref(), Some("default"));
        // Panel starts hidden on a fresh install.
        assert!(!s.visible);
        // Default size is sane (normalized clamps the 0.0 sentinel).
        assert!(s.width >= 240.0);
        assert!(s.height >= 160.0);
    }

    #[test]
    fn chat_settings_normalized_repairs_empty_agents_and_dangling_active_id() {
        // Hand-craft a degenerate config: no agents, active id points nowhere.
        let s = ChatSettings {
            visible: true,
            agents: vec![],
            active_agent_id: Some("ghost".to_string()),
            position: ChatPosition { x: 42.0, y: 99.0 },
            width: 0.0,
            height: 0.0,
            dock: ChatDock::Floating,
            allow_remote_presets: false,
            proxy_mode: ChatProxyMode::System,
            proxy_url: String::new(),
        };
        let n = s.normalized();
        // The default OpenAI agent is restored.
        assert_eq!(n.agents.len(), 1);
        // The dangling active id is clamped to the (only) real agent.
        assert_eq!(n.active_agent_id.as_deref(), Some("default"));
        // The user's dragged position survives (only agents/id are repaired).
        assert_eq!(n.position, ChatPosition { x: 42.0, y: 99.0 });
        // Zero size sentinel is clamped to the built-in defaults.
        assert!(n.width >= 240.0);
        assert!(n.height >= 160.0);
    }

    #[test]
    fn chat_settings_roundtrip_preserves_agents_position_and_visibility() {
        let s = ChatSettings {
            visible: true,
            agents: vec![
                AgentConfig::default_openai(),
                AgentConfig {
                    id: "anthropic-1".to_string(),
                    name: "Claude".to_string(),
                    provider: ChatAgentProvider::Anthropic,
                    model: "claude-3-5-sonnet-20241022".to_string(),
                    base_url: String::new(),
                    api_key_id: "anthropic-1".to_string(),
                    system_prompt: "Be terse.".to_string(),
                },
            ],
            active_agent_id: Some("anthropic-1".to_string()),
            position: ChatPosition { x: 120.0, y: 300.0 },
            width: 420.0,
            height: 400.0,
            dock: ChatDock::Right,
            allow_remote_presets: true,
            proxy_mode: ChatProxyMode::Clash,
            proxy_url: String::new(),
        }
        .normalized();
        let json = serde_json::to_string(&s).unwrap();
        let parsed: ChatSettings = serde_json::from_str(&json).unwrap();
        let parsed = parsed.normalized();
        assert_eq!(parsed, s);
        assert_eq!(
            parsed.active_agent().map(|a| a.id.as_str()),
            Some("anthropic-1")
        );
        assert_eq!(parsed.position, ChatPosition { x: 120.0, y: 300.0 });
        // The dock mode survives the roundtrip too.
        assert_eq!(parsed.dock, ChatDock::Right);
        // Issue #126: network consent + proxy settings survive the roundtrip.
        assert!(parsed.allow_remote_presets);
        assert_eq!(parsed.proxy_mode, ChatProxyMode::Clash);
    }

    #[test]
    fn chat_dock_cycles_through_all_modes_and_defaults_to_floating() {
        assert_eq!(ChatDock::default(), ChatDock::Floating);
        assert_eq!(ChatDock::Floating.next(), ChatDock::Right);
        assert_eq!(ChatDock::Right.next(), ChatDock::Bottom);
        assert_eq!(ChatDock::Bottom.next(), ChatDock::Floating);
        // Legacy settings files omit `dock` entirely → floating overlay.
        let legacy = r#"{"visible":true}"#;
        let parsed: ChatSettings = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.dock, ChatDock::Floating);
    }

    #[test]
    fn legacy_config_without_chat_field_loads_with_disabled_default() {
        // Settings files written before issue #122 omit the `chat` field
        // entirely. They must deserialize into the disabled default so the
        // panel doesn't pop open on upgrade.
        let config: PersistedConfig =
            serde_json::from_str(r#"{"version":1,"connections":[]}"#).unwrap();
        assert!(!config.chat.visible);
        assert_eq!(config.chat.agents.len(), 1);
        assert_eq!(config.chat.agents[0].provider, ChatAgentProvider::OpenAI);
    }

    #[test]
    fn toggle_chat_keybinding_defaults_to_primary_shift_space() {
        // The default chord MUST pass `is_safe_application_shortcut`
        // (primary && shift) so it's reachable on macOS and doesn't collide
        // with plain terminal control chords. Plain Cmd+Space (no shift) is
        // handled by a special-case in the global onkeydown, NOT by this
        // keybinding, because it's Spotlight's system hotkey.
        let kb = Keybindings::default();
        // Clone the chord so we don't partially-move `kb` before the
        // `action_for` lookup below.
        let chord = kb
            .toggle_chat
            .clone()
            .expect("toggle_chat must have a default chord");
        assert_eq!(chord.key, "space");
        assert!(chord.primary);
        assert!(chord.shift);
        assert!(!chord.alt);
        assert!(chord.is_safe_application_shortcut());
        // And it resolves to the ToggleChat action.
        assert_eq!(kb.action_for(&chord), Some(KeybindingAction::ToggleChat));
    }
}
