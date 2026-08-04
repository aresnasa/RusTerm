use dioxus::prelude::*;

use crate::state::CommandStatus;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandStatusPresentation {
    label: String,
    background: &'static str,
    title: String,
}

fn command_status_presentation(
    status: &CommandStatus,
    connected: bool,
) -> Option<CommandStatusPresentation> {
    match status {
        // `Idle` normally hides the badge — but a live, connected session
        // should still show green feedback ("链接成功"). This covers hosts
        // that can never resolve a command status: e.g. a target reached
        // through an integrated jump-host shell (OSC 133 evidence from the
        // outer shell permanently disables the prompt-return fallback, and
        // the inner shell emits no OSC 133;D), where the badge would
        // otherwise vanish after the first command and never come back.
        CommandStatus::Idle if connected => Some(CommandStatusPresentation {
            label: crate::i18n::t("cmd_status.connected"),
            background: "rgba(76, 175, 80, 0.92)",
            title: crate::i18n::t("cmd_status.connected_tip"),
        }),
        CommandStatus::Idle => None,
        CommandStatus::Success => Some(CommandStatusPresentation {
            label: crate::i18n::t("cmd_status.success"),
            background: "rgba(76, 175, 80, 0.92)",
            title: crate::i18n::t("cmd_status.success_tip"),
        }),
        CommandStatus::Failed(exit_code) => Some(CommandStatusPresentation {
            label: crate::i18n::tf("cmd_status.failed", &[("exit_code", exit_code)]),
            background: "rgba(244, 67, 54, 0.92)",
            title: crate::i18n::tf("cmd_status.failed_tip", &[("exit_code", exit_code)]),
        }),
        CommandStatus::Disconnected(reason) => Some(CommandStatusPresentation {
            label: crate::i18n::t("cmd_status.disconnected"),
            background: "rgba(244, 67, 54, 0.92)",
            title: crate::i18n::tf("cmd_status.disconnected_tip", &[("reason", reason)]),
        }),
    }
}

/// Compact command result shown in session chrome (workspace tabs and pane
/// title bars). This component never writes to the terminal/PTY output.
///
/// `connected` marks the session's live connection state: an `Idle` status
/// (no command result to show) then falls back to a green "connected" badge
/// instead of rendering nothing.
#[component]
pub fn CommandStatusBadge(status: CommandStatus, #[props(default)] connected: bool) -> Element {
    // Subscribe to language changes so the badge re-renders on switch.
    let _lang = crate::i18n::LANGUAGE();
    let Some(presentation) = command_status_presentation(&status, connected) else {
        return rsx! {};
    };

    rsx! {
        span {
            style: "
                display: inline-flex;
                align-items: center;
                height: 16px;
                padding: 0 6px;
                border-radius: 3px;
                background: {presentation.background};
                color: #ffffff;
                font-size: 10px;
                font-family: 'JetBrains Mono', monospace;
                line-height: 16px;
                flex-shrink: 0;
                pointer-events: none;
                user-select: none;
                white-space: nowrap;
                box-shadow: 0 1px 3px rgba(0,0,0,0.25);
            ",
            title: "{presentation.title}",
            "{presentation.label}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_has_no_badge_when_not_connected() {
        assert_eq!(
            command_status_presentation(&CommandStatus::Idle, false),
            None
        );
    }

    #[test]
    fn idle_shows_green_connected_badge_when_connected() {
        let presentation = command_status_presentation(&CommandStatus::Idle, true).unwrap();
        assert_eq!(presentation.label, "✓ 已连接");
        assert!(presentation.background.contains("76, 175, 80"));
    }

    #[test]
    fn success_is_green() {
        let presentation = command_status_presentation(&CommandStatus::Success, false).unwrap();
        assert_eq!(presentation.label, "✓ 成功");
        assert!(presentation.background.contains("76, 175, 80"));
    }

    #[test]
    fn real_command_status_wins_over_connected_fallback() {
        // A resolved command result must not be masked by the connection
        // fallback — Success/Failed keep their own labels while connected.
        let success = command_status_presentation(&CommandStatus::Success, true).unwrap();
        assert_eq!(success.label, "✓ 成功");
        let failed = command_status_presentation(&CommandStatus::Failed(1), true).unwrap();
        assert!(failed.label.contains("失败"));
    }

    #[test]
    fn failure_is_red_and_keeps_exit_code() {
        let presentation = command_status_presentation(&CommandStatus::Failed(127), false).unwrap();
        assert_eq!(presentation.label, "✗ 失败 (exit 127)");
        assert!(presentation.background.contains("244, 67, 54"));
    }

    #[test]
    fn disconnect_is_red_and_keeps_reason_in_tooltip() {
        let presentation =
            command_status_presentation(&CommandStatus::Disconnected("timeout".to_string()), false)
                .unwrap();
        assert_eq!(presentation.label, "⚠ 断开");
        assert!(presentation.background.contains("244, 67, 54"));
        assert!(presentation.title.contains("timeout"));
        assert!(presentation.title.contains("Enter"));
    }

    #[test]
    fn disconnect_status_wins_even_when_connection_flag_is_stale() {
        // `Disconnected` status + `connected: true` (stale flag during a
        // reconnect race) must keep showing the red disconnect badge.
        let presentation =
            command_status_presentation(&CommandStatus::Disconnected("timeout".to_string()), true)
                .unwrap();
        assert_eq!(presentation.label, "⚠ 断开");
    }
}
