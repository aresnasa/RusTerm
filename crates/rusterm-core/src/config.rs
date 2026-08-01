use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConnectionConfig {
    pub id: String,
    pub name: String,
    pub kind: ConnectionKind,
    pub group: Option<String>,
    pub tags: Vec<String>,
    pub onekey: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ConnectionKind {
    Ssh(SshConfig),
    Serial(SerialConfig),
    Telnet(TelnetConfig),
    Shell(ShellConfig),
    Tcp(TcpConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
    pub terminal_type: String,
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
}

impl KeybindingAction {
    pub const ALL: [Self; 4] = [
        Self::CloseFocusedPane,
        Self::AppendPane,
        Self::ToggleComparison,
        Self::TogglePaneZoom,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::CloseFocusedPane => "Close focused pane",
            Self::AppendPane => "Add split pane",
            Self::ToggleComparison => "Toggle synchronized input",
            Self::TogglePaneZoom => "Toggle pane zoom",
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

impl Default for Keybindings {
    fn default() -> Self {
        Self {
            close_focused_pane: default_close_focused_pane_keybinding(),
            append_pane: default_append_pane_keybinding(),
            toggle_comparison: default_toggle_comparison_keybinding(),
            toggle_pane_zoom: default_toggle_pane_zoom_keybinding(),
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
        }
    }

    pub fn set_chord(&mut self, action: KeybindingAction, chord: Option<KeyChord>) {
        match action {
            KeybindingAction::CloseFocusedPane => self.close_focused_pane = chord,
            KeybindingAction::AppendPane => self.append_pane = chord,
            KeybindingAction::ToggleComparison => self.toggle_comparison = chord,
            KeybindingAction::TogglePaneZoom => self.toggle_pane_zoom = chord,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedConfig {
    pub version: u32,
    pub connections: Vec<PersistedConnection>,
    #[serde(default)]
    pub onekeys: Vec<PersistedOneKey>,
    #[serde(default)]
    pub master_password_hash: Option<String>,
    /// Whether the user picked "不再询问" on the session-state restore dialog.
    /// When true, we don't save session state and don't prompt on next launch.
    /// The user can re-enable via settings (future work: a settings toggle).
    /// Default false for backward compat — existing users get the prompt.
    #[serde(default)]
    pub restore_disabled: bool,
    /// Whether to show the "是否确实要关闭本软件？" confirmation dialog when the
    /// user closes the last window. Default true (safe default — always ask).
    /// When false, closing the last window exits the app immediately.
    /// Persisted so the user's choice on the dialog's "下次关闭时不再询问"
    /// checkbox survives across launches.
    #[serde(default = "default_confirm_close_on_exit")]
    pub confirm_close_on_exit: bool,
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
    /// User-configured application shortcuts. Missing in legacy settings files
    /// means the established defaults are used.
    #[serde(default)]
    pub keybindings: Keybindings,
}

/// Default for `PersistedConfig::confirm_close_on_exit`. Kept as a function
/// (not a constant) so `#[serde(default = "...")]` can reference it. True
/// because the safe default is to always ask before closing the app.
fn default_confirm_close_on_exit() -> bool {
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

// --- OneKeys (ZOC-style Expect/Send auto-fill) ---

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
    pub proxy_jump: Option<String>,
    pub keepalive_interval: Option<u64>,
    #[serde(default = "default_host_key_policy")]
    pub host_key_policy: String,
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
                proxy_jump: None,
                keepalive_interval: Some(30),
                host_key_policy: default_host_key_policy(),
            }),
            group: Some("Production".to_string()),
            tags: vec!["linux".to_string(), "prod".to_string()],
            onekey: true,
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ConnectionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
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
    fn focused_tab_appearance_defaults_for_legacy_settings() {
        let config: PersistedConfig =
            serde_json::from_str(r#"{"version":1,"connections":[]}"#).unwrap();

        assert_eq!(
            config.focused_tab_appearance,
            FocusedTabAppearance::default()
        );
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
    fn keybindings_default_for_legacy_settings() {
        let config: PersistedConfig =
            serde_json::from_str(r#"{"version":1,"connections":[]}"#).unwrap();

        assert_eq!(config.keybindings, Keybindings::default());
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
            master_password_hash: None,
            restore_disabled: false,
            confirm_close_on_exit: true,
            focused_tab_appearance: FocusedTabAppearance::default(),
            suggestion_enabled: false,
            suggestion_count: 10,
            sidebar: SidebarPreferences::default(),
            keybindings: Keybindings::default(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: PersistedConfig = serde_json::from_str(&json).unwrap();
        assert!(!parsed.suggestion_enabled);
        assert_eq!(parsed.suggestion_count, 10);
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
}
