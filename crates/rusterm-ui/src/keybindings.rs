use dioxus::prelude::Key;

use rusterm_core::config::{KeyChord, KeybindingAction, Keybindings};

/// Convert Dioxus keyboard data into the stable string persisted in settings.
pub fn key_name(key: &Key) -> Option<String> {
    match key {
        Key::Character(value) if !value.trim().is_empty() => Some(value.to_ascii_lowercase()),
        Key::Character(_) => Some("space".to_string()),
        _ => {
            let name = format!("{key:?}");
            (!matches!(name.as_str(), "Alt" | "Control" | "Meta" | "Shift"))
                .then_some(name.to_ascii_lowercase())
        }
    }
}

/// Produce the portable chord representation for a keyboard event.
pub fn event_chord(key: &Key, ctrl: bool, alt: bool, meta: bool, shift: bool) -> Option<KeyChord> {
    let primary = if cfg!(target_os = "macos") {
        meta && !ctrl
    } else {
        ctrl && !meta
    };

    Some(KeyChord {
        key: key_name(key)?,
        primary,
        alt,
        shift,
    })
    .and_then(KeyChord::normalized)
}

/// Resolve an event against the user's application-level keybindings.
pub fn action_for_event(
    keybindings: &Keybindings,
    key: &Key,
    ctrl: bool,
    alt: bool,
    meta: bool,
    shift: bool,
) -> Option<KeybindingAction> {
    let chord = event_chord(key, ctrl, alt, meta, shift)?;
    chord
        .is_safe_application_shortcut()
        .then(|| keybindings.action_for(&chord))
        .flatten()
}

pub fn format_key_chord(chord: Option<&KeyChord>) -> String {
    let Some(chord) = chord else {
        return crate::i18n::t("keybindings.disabled");
    };

    let mut parts = Vec::new();
    if chord.primary {
        parts.push(if cfg!(target_os = "macos") {
            "⌘".to_string()
        } else {
            "Ctrl".to_string()
        });
    }
    if chord.alt {
        parts.push(if cfg!(target_os = "macos") {
            "⌥".to_string()
        } else {
            "Alt".to_string()
        });
    }
    if chord.shift {
        parts.push(if cfg!(target_os = "macos") {
            "⇧".to_string()
        } else {
            "Shift".to_string()
        });
    }
    let key = if chord.key.len() == 1 {
        chord.key.to_ascii_uppercase()
    } else {
        chord.key.clone()
    };
    parts.push(key);
    parts.join(" + ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolver_matches_default_keybinding() {
        let keybindings = Keybindings::default();
        let chord = KeyChord {
            key: "l".to_string(),
            primary: true,
            alt: false,
            shift: true,
        };
        assert_eq!(
            keybindings.action_for(&chord),
            Some(KeybindingAction::AppendPane)
        );
    }

    #[test]
    fn plain_terminal_control_chords_are_not_application_shortcuts() {
        let keybindings = Keybindings::default();
        for key in ["a", "e", "r", "w", "x", "z"] {
            let keyboard_key = Key::Character(key.into());
            assert_eq!(
                action_for_event(&keybindings, &keyboard_key, true, false, false, false),
                None,
                "Ctrl+{key} must remain available to the focused terminal"
            );
        }
    }

    #[test]
    fn formats_disabled_and_active_chords() {
        assert_eq!(format_key_chord(None), crate::i18n::t("keybindings.disabled"));
        assert!(format_key_chord(Keybindings::default().append_pane.as_ref()).contains('L'));
    }
}
