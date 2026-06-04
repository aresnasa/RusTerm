use dioxus::prelude::*;

use crate::state::UnlockState;

#[component]
pub fn MasterPasswordDialog(
    mode: UnlockState,
    error: Option<String>,
    on_unlock: EventHandler<String>,
) -> Element {
    let mut password = use_signal(String::new);
    let mut confirm = use_signal(String::new);
    let mut loading = use_signal(|| false);

    let is_first_run = mode == UnlockState::FirstRun;
    let title = if is_first_run {
        "Create Master Password"
    } else {
        "Unlock RusTerm"
    };
    let subtitle = if is_first_run {
        "Set a master password to protect your connection credentials."
    } else {
        "Enter your master password to decrypt your connections."
    };

    let passwords_match = !is_first_run || password() == confirm();
    let can_submit = !password().is_empty() && passwords_match && !loading();

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

                // Logo / title area
                div {
                    style: "text-align: center; margin-bottom: 24px;",
                    h2 {
                        style: "margin: 0 0 8px; font-size: 20px; font-weight: 600; color: #7aa2f7;",
                        "{title}"
                    }
                    p {
                        style: "margin: 0; font-size: 13px; color: #565f89; line-height: 1.5;",
                        "{subtitle}"
                    }
                }

                div {
                    style: "display: flex; flex-direction: column; gap: 14px;",

                    label {
                        style: "font-size: 12px; color: #565f89; font-weight: 500;",
                        "Master Password"
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
                        r#type: "password",
                        placeholder: "Enter password",
                        autofocus: true,
                        value: "{password}",
                        oninput: move |e| password.set(e.value()),
                        onkeydown: move |e| {
                            if e.key() == Key::Enter && can_submit {
                                loading.set(true);
                                on_unlock.call(password());
                            }
                        },
                    }

                    if is_first_run {
                        label {
                            style: "font-size: 12px; color: #565f89; font-weight: 500;",
                            "Confirm Password"
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
                            r#type: "password",
                            placeholder: "Confirm password",
                            value: "{confirm}",
                            oninput: move |e| confirm.set(e.value()),
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
                                "Passwords do not match"
                            }
                        }
                    }

                    if let Some(ref err) = error {
                        p {
                            style: "color: #f7768e; font-size: 12px; margin: 0; text-align: center;",
                            "{err}"
                        }
                    }

                    button {
                        style: if can_submit {
                            "background: #7aa2f7; border: none; color: #1a1b26; border-radius: 4px; padding: 10px; cursor: pointer; font-size: 14px; font-weight: 600; width: 100%;"
                        } else {
                            "background: #2a2b3d; border: none; color: #565f89; border-radius: 4px; padding: 10px; cursor: not-allowed; font-size: 14px; width: 100%;"
                        },
                        disabled: !can_submit,
                        onclick: move |_| {
                            if can_submit {
                                loading.set(true);
                                on_unlock.call(password());
                            }
                        },
                        if loading() {
                            "Verifying..."
                        } else if is_first_run {
                            "Create & Unlock"
                        } else {
                            "Unlock"
                        }
                    }

                    if is_first_run {
                        p {
                            style: "font-size: 11px; color: #565f89; margin: 0; text-align: center; line-height: 1.4;",
                            "Your master password cannot be recovered if lost.\nIt protects all saved connection credentials."
                        }
                    }
                }
            }
        }
    }
}
