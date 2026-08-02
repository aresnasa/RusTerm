use dioxus::prelude::*;

use crate::state::UnlockState;

#[component]
pub fn MasterPasswordDialog(
    mode: UnlockState,
    error: Option<String>,
    on_unlock: EventHandler<String>,
    on_clear_error: EventHandler<()>,
) -> Element {
    let _lang = crate::i18n::LANGUAGE();
    let mut password = use_signal(String::new);
    let mut confirm = use_signal(String::new);
    let mut loading = use_signal(|| false);
    let mut show_password = use_signal(|| false);

    let is_first_run = mode == UnlockState::FirstRun;
    let title = if is_first_run {
        crate::i18n::t("master_password.create_title")
    } else {
        crate::i18n::t("master_password.unlock_title")
    };
    let subtitle = if is_first_run {
        crate::i18n::t("master_password.create_subtitle")
    } else {
        crate::i18n::t("master_password.unlock_subtitle")
    };

    // If an error appears, clear the loading state so the user can retry
    if error.is_some() && loading() {
        loading.set(false);
    }
    let has_error = error.is_some();
    let error_text = error.clone();

    let passwords_match = !is_first_run || password() == confirm();
    let can_submit = !password().is_empty() && passwords_match && !loading();
    let pw_input_type = if show_password() { "text" } else { "password" };
    let toggle_label = if show_password() {
        crate::i18n::t("common.hide")
    } else {
        crate::i18n::t("common.show")
    };

    rsx! {
        div {
            style: "
                position: fixed;
                top: 0; left: 0; right: 0; bottom: 0;
                background: #1a1b26;
                display: flex;
                justify-content: center;
                align-items: center;
                z-index: 2000;
            ",

            div {
                style: "
                    background: #24283b;
                    border-radius: 8px;
                    padding: 32px;
                    width: 400px;
                    color: #c0caf5;
                ",

                div {
                    style: "text-align: center; margin-bottom: 24px;",
                    h2 {
                        style: "margin: 0 0 8px; font-size: 20px; font-weight: 600; color: #7aa2f7;",
                        "{title}"
                    }
                    p {
                        style: "margin: 0; font-size: 13px; color: #9aa5ce; line-height: 1.5;",
                        "{subtitle}"
                    }
                }

                div {
                    style: "display: flex; flex-direction: column; gap: 14px;",

                    div {
                        style: "display: flex; flex-direction: column; gap: 4px;",
                        label {
                            style: "font-size: 12px; color: #9aa5ce; font-weight: 500;",
                            { crate::i18n::t("master_password.label") }
                        }
                        div {
                            style: "display: flex; gap: 6px;",
                            input {
                                style: "
                                    flex: 1;
                                    background: #1a1b26;
                                    border: 1px solid #2a2b3d;
                                    border-radius: 4px;
                                    padding: 10px 12px;
                                    color: #c0caf5;
                                    font-size: 14px;
                                    outline: none;
                                    box-sizing: border-box;
                                ",
                                r#type: "{pw_input_type}",
                                placeholder: crate::i18n::t("master_password.enter_placeholder"),
                                autofocus: true,
                                value: "{password}",
                                oninput: move |e| {
                                    password.set(e.value());
                                    if has_error { on_clear_error.call(()); }
                                },
                                onkeydown: move |e| {
                                    if e.key() == Key::Enter && can_submit {
                                        loading.set(true);
                                        on_unlock.call(password());
                                    }
                                },
                            }
                            button {
                                style: "
                                    background: #1a1b26;
                                    border: 1px solid #2a2b3d;
                                    border-radius: 4px;
                                    padding: 0 12px;
                                    color: #9aa5ce;
                                    cursor: pointer;
                                    font-size: 12px;
                                    min-width: 56px;
                                ",
                                r#type: "button",
                                title: crate::i18n::t("master_password.toggle_visibility"),
                                onclick: move |_| show_password.set(!show_password()),
                                "{toggle_label}"
                            }
                        }
                    }

                    if is_first_run {
                        div {
                            style: "display: flex; flex-direction: column; gap: 4px;",
                            label {
                                style: "font-size: 12px; color: #9aa5ce; font-weight: 500;",
                                { crate::i18n::t("master_password.confirm_label") }
                            }
                            input {
                                style: "
                                    background: #1a1b26;
                                    border: 1px solid #2a2b3d;
                                    border-radius: 4px;
                                    padding: 10px 12px;
                                    color: #c0caf5;
                                    font-size: 14px;
                                    outline: none;
                                    width: 100%;
                                    box-sizing: border-box;
                                ",
                                r#type: "{pw_input_type}",
                                placeholder: crate::i18n::t("master_password.confirm_placeholder"),
                                value: "{confirm}",
                                oninput: move |e| {
                                    confirm.set(e.value());
                                    if has_error { on_clear_error.call(()); }
                                },
                                onkeydown: move |e| {
                                    if e.key() == Key::Enter && can_submit {
                                        loading.set(true);
                                        on_unlock.call(password());
                                    }
                                },
                            }
                            if !passwords_match && !confirm().is_empty() {
                                p {
                                    style: "color: #f7768e; font-size: 12px; margin: 0;",
                                    { crate::i18n::t("master_password.mismatch") }
                                }
                            }
                        }
                    }

                    if let Some(ref err) = error_text {
                        p {
                            style: "color: #f7768e; font-size: 12px; margin: 0; text-align: center;",
                            "{err}"
                        }
                    }

                    button {
                        style: if can_submit {
                            "background: #7aa2f7; border: none; color: #1a1b26; border-radius: 4px; padding: 10px; cursor: pointer; font-size: 14px; font-weight: 600; width: 100%;"
                        } else {
                            "background: #2a2b3d; border: none; color: #9aa5ce; border-radius: 4px; padding: 10px; cursor: not-allowed; font-size: 14px; width: 100%;"
                        },
                        disabled: !can_submit,
                        onclick: move |_| {
                            if can_submit {
                                loading.set(true);
                                on_unlock.call(password());
                            }
                        },
                        if loading() {
                            { crate::i18n::t("master_password.verifying") }
                        } else if is_first_run {
                            { crate::i18n::t("master_password.create_and_unlock") }
                        } else {
                            { crate::i18n::t("master_password.unlock") }
                        }
                    }

                    if is_first_run {
                        p {
                            style: "font-size: 11px; color: #9aa5ce; margin: 0; text-align: center; line-height: 1.4;",
                            { crate::i18n::t("master_password.recovery_warning") }
                        }
                    }
                }
            }
        }
    }
}
