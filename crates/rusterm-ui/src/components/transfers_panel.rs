use dioxus::prelude::*;

use crate::transfers::{FileEndpoint, TransferDirection, TransferJob, TransferStatus};

#[component]
pub fn TransfersPanel(
    jobs: Vec<TransferJob>,
    on_cancel: EventHandler<String>,
    on_retry: EventHandler<String>,
    on_clear_finished: EventHandler<()>,
) -> Element {
    let has_finished = jobs.iter().any(|job| job.status.is_finished());

    rsx! {
        style { r#"
            .transfers-panel-button {{
                border: 1px solid var(--skin-border);
                border-radius: 4px;
                padding: 4px 8px;
                background: var(--skin-surface);
                color: var(--skin-text);
                font-size: 11px;
                cursor: pointer;
            }}
            .transfers-panel-button:hover:not(:disabled) {{
                border-color: var(--skin-accent);
                color: var(--skin-accent);
            }}
            .transfers-panel-button:disabled {{
                opacity: 0.45;
                cursor: default;
            }}
        "# }
        section {
            style: "height:100%;min-height:0;display:flex;flex-direction:column;background:var(--skin-bg);color:var(--skin-text);",
            div {
                style: "display:flex;align-items:center;justify-content:space-between;gap:12px;padding:9px 12px;border-bottom:1px solid var(--skin-border);",
                div {
                    style: "font-size:12px;font-weight:600;",
                    "Transfers"
                }
                button {
                    class: "transfers-panel-button",
                    disabled: !has_finished,
                    title: if has_finished { "Remove completed, failed, and cancelled transfers" } else { "No finished transfers" },
                    onclick: move |_| on_clear_finished.call(()),
                    "Clear finished"
                }
            }

            div {
                style: "flex:1;min-height:0;overflow-y:auto;padding:8px;",
                if jobs.is_empty() {
                    div {
                        style: "display:flex;height:100%;min-height:120px;align-items:center;justify-content:center;padding:24px;text-align:center;color:var(--skin-text-muted);font-size:12px;line-height:1.6;box-sizing:border-box;",
                        "No file transfers yet.\nUploads and downloads will appear here."
                    }
                } else {
                    for job in jobs {
                        {
                            let id = job.id.clone();
                            let key = job.id.clone();
                            let name = file_name(&job.source);
                            let source = endpoint_label(&job.source);
                            let destination = endpoint_label(&job.destination);
                            let status_text = status(&job.status);
                            let direction = match job.direction() {
                                Some(TransferDirection::Upload) => "↑ Upload",
                                Some(TransferDirection::Download) => "↓ Download",
                                None => "↔ Transfer",
                            };
                            let can_cancel = matches!(
                                job.status,
                                TransferStatus::Queued | TransferStatus::Running
                            );
                            let can_retry = matches!(
                                job.status,
                                TransferStatus::Failed(_) | TransferStatus::Cancelled
                            );
                            let progress = if job.total == 0 {
                                if matches!(job.status, TransferStatus::Succeeded) {
                                    100.0
                                } else {
                                    0.0
                                }
                            } else {
                                (job.transferred as f64 * 100.0 / job.total as f64).clamp(0.0, 100.0)
                            };
                            let byte_progress = format!(
                                "{} / {}",
                                format_bytes(job.transferred),
                                format_bytes(job.total)
                            );

                            rsx! {
                                article {
                                    key: "{key}",
                                    style: "margin-bottom:8px;padding:10px 11px;border:1px solid var(--skin-border);border-radius:5px;background:var(--skin-surface);",
                                    div {
                                        style: "display:flex;align-items:flex-start;justify-content:space-between;gap:10px;",
                                        div {
                                            style: "min-width:0;flex:1;",
                                            div {
                                                style: "display:flex;align-items:center;gap:7px;min-width:0;",
                                                span {
                                                    style: "flex:0 0 auto;color:var(--skin-accent);font-size:10px;font-weight:600;",
                                                    "{direction}"
                                                }
                                                strong {
                                                    style: "min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:12px;",
                                                    title: "{name}",
                                                    "{name}"
                                                }
                                            }
                                            div {
                                                style: "margin-top:5px;color:var(--skin-text-muted);font:10px ui-monospace,SFMono-Regular,Menlo,monospace;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                                                title: "Source: {source}",
                                                "From: {source}"
                                            }
                                            div {
                                                style: "margin-top:2px;color:var(--skin-text-muted);font:10px ui-monospace,SFMono-Regular,Menlo,monospace;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                                                title: "Destination: {destination}",
                                                "To: {destination}"
                                            }
                                        }

                                        if can_cancel {
                                            button {
                                                class: "transfers-panel-button",
                                                onclick: move |_| on_cancel.call(id.clone()),
                                                "Cancel"
                                            }
                                        } else if can_retry {
                                            button {
                                                class: "transfers-panel-button",
                                                onclick: move |_| on_retry.call(id.clone()),
                                                "Retry"
                                            }
                                        }
                                    }

                                    div {
                                        style: "display:flex;align-items:center;justify-content:space-between;gap:8px;margin-top:9px;color:var(--skin-text-muted);font-size:10px;",
                                        span { "{byte_progress}" }
                                        span { "{progress:.0}%" }
                                    }
                                    div {
                                        style: "height:5px;margin-top:4px;overflow:hidden;border-radius:999px;background:var(--skin-border);",
                                        title: "{progress:.0}%",
                                        div {
                                            style: "width:{progress:.2}%;height:100%;border-radius:inherit;background:var(--skin-accent);transition:width 160ms ease;"
                                        }
                                    }
                                    div {
                                        style: "margin-top:6px;color:var(--skin-text-muted);font-size:10px;overflow-wrap:anywhere;",
                                        "{status_text}"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];

    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn endpoint_label(endpoint: &FileEndpoint) -> String {
    match endpoint {
        FileEndpoint::Local(path) => format!("Local · {}", path.display()),
        FileEndpoint::Remote(path) => format!("Remote · {path}"),
    }
}

fn file_name(endpoint: &FileEndpoint) -> String {
    let name = match endpoint {
        FileEndpoint::Local(path) => path.file_name().and_then(|name| name.to_str()),
        FileEndpoint::Remote(path) => path
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .filter(|name| !name.is_empty()),
    };

    name.unwrap_or("Unnamed file").to_string()
}

fn status(transfer_status: &TransferStatus) -> String {
    match transfer_status {
        TransferStatus::Queued => "Queued".to_string(),
        TransferStatus::Running => "Running".to_string(),
        TransferStatus::Succeeded => "Completed".to_string(),
        TransferStatus::Failed(reason) if reason.is_empty() => "Failed".to_string(),
        TransferStatus::Failed(reason) => format!("Failed: {reason}"),
        TransferStatus::Cancelled => "Cancelled".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn format_bytes_uses_binary_thresholds_and_readable_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn endpoint_label_identifies_local_and_remote_paths() {
        assert_eq!(
            endpoint_label(&FileEndpoint::Local(PathBuf::from("/tmp/report.txt"))),
            "Local · /tmp/report.txt"
        );
        assert_eq!(
            endpoint_label(&FileEndpoint::Remote("/srv/report.txt".to_string())),
            "Remote · /srv/report.txt"
        );
    }

    #[test]
    fn file_name_handles_local_remote_and_missing_names() {
        assert_eq!(
            file_name(&FileEndpoint::Local(PathBuf::from("/tmp/report.txt"))),
            "report.txt"
        );
        assert_eq!(
            file_name(&FileEndpoint::Remote(
                "/srv/archive/report.tar.gz".to_string()
            )),
            "report.tar.gz"
        );
        assert_eq!(
            file_name(&FileEndpoint::Remote("C:\\logs\\output.txt".to_string())),
            "output.txt"
        );
        assert_eq!(
            file_name(&FileEndpoint::Local(PathBuf::from("/"))),
            "Unnamed file"
        );
    }

    #[test]
    fn status_covers_every_transfer_state_and_preserves_failure_reason() {
        assert_eq!(status(&TransferStatus::Queued), "Queued");
        assert_eq!(status(&TransferStatus::Running), "Running");
        assert_eq!(status(&TransferStatus::Succeeded), "Completed");
        assert_eq!(
            status(&TransferStatus::Failed("connection lost".to_string())),
            "Failed: connection lost"
        );
        assert_eq!(status(&TransferStatus::Failed(String::new())), "Failed");
        assert_eq!(status(&TransferStatus::Cancelled), "Cancelled");
    }
}
