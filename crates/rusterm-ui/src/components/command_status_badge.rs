use dioxus::prelude::*;

use crate::state::CommandStatus;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandStatusPresentation {
    label: String,
    background: &'static str,
    title: String,
}

fn command_status_presentation(status: &CommandStatus) -> Option<CommandStatusPresentation> {
    match status {
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
#[component]
pub fn CommandStatusBadge(status: CommandStatus) -> Element {
    // Subscribe to language changes so the badge re-renders on switch.
    let _lang = crate::i18n::LANGUAGE();
    let Some(presentation) = command_status_presentation(&status) else {
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
    fn idle_has_no_badge() {
        assert_eq!(command_status_presentation(&CommandStatus::Idle), None);
    }

    #[test]
    fn success_is_green() {
        let presentation = command_status_presentation(&CommandStatus::Success).unwrap();
        assert_eq!(presentation.label, "✓ 成功");
        assert!(presentation.background.contains("76, 175, 80"));
    }

    #[test]
    fn failure_is_red_and_keeps_exit_code() {
        let presentation = command_status_presentation(&CommandStatus::Failed(127)).unwrap();
        assert_eq!(presentation.label, "✗ 失败 (exit 127)");
        assert!(presentation.background.contains("244, 67, 54"));
    }

    #[test]
    fn disconnect_is_red_and_keeps_reason_in_tooltip() {
        let presentation =
            command_status_presentation(&CommandStatus::Disconnected("timeout".to_string()))
                .unwrap();
        assert_eq!(presentation.label, "⚠ 断开");
        assert!(presentation.background.contains("244, 67, 54"));
        assert!(presentation.title.contains("timeout"));
        assert!(presentation.title.contains("Enter"));
    }
}
