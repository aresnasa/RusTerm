use dioxus::prelude::*;

/// RusTerm's own lightweight outline icon set. The geometry is intentionally
/// simple and uses `currentColor`, so icons inherit the active theme without
/// copying third-party artwork.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconName {
    ChevronDown,
    ChevronRight,
    Connect,
    Delete,
    Edit,
    Eye,
    EyeOff,
    Folder,
    FolderOpen,
    Key,
    Plus,
    Search,
    Serial,
    Shell,
    Ssh,
    Tcp,
    Telnet,
}

#[component]
pub fn Icon(name: IconName, #[props(default = 16)] size: u8) -> Element {
    let path = match name {
        IconName::ChevronDown => "M6 9l6 6 6-6",
        IconName::ChevronRight => "M9 6l6 6-6 6",
        IconName::Connect => "M14 5h5v14h-5M5 12h11m-4-4 4 4-4 4",
        IconName::Delete => "M5 7h14M9 7V4h6v3m-8 0 1 13h8l1-13M10 11v5m4-5v5",
        IconName::Edit => "M5 19l4-1 9-9-3-3-9 9-1 4zm8-11 3 3",
        IconName::Eye => {
            "M2.5 12s3.5-6 9.5-6 9.5 6 9.5 6-3.5 6-9.5 6S2.5 12 2.5 12zm9.5-2.5a2.5 2.5 0 1 0 0 5 2.5 2.5 0 0 0 0-5z"
        }
        IconName::EyeOff => {
            "M3 3l18 18M10.5 6.2A10.8 10.8 0 0 1 12 6c6 0 9.5 6 9.5 6a13 13 0 0 1-3 3.8M7.3 7.3C4.2 9 2.5 12 2.5 12s3.5 6 9.5 6c1.2 0 2.3-.2 3.3-.6M9.8 9.8a3 3 0 0 0 4.4 4.4"
        }
        IconName::Folder => "M3 7h7l2 2h9v10H3V7z",
        IconName::FolderOpen => "M3 8h7l2 2h9l-2 9H4L3 8zm1 4h16",
        IconName::Key => "M14 7a4 4 0 1 1-3.5 5.9L3 20v-3l2-2h3l2.5-2.5M14 7h.01",
        IconName::Plus => "M12 5v14M5 12h14",
        IconName::Search => "M10.5 4a6.5 6.5 0 1 0 0 13 6.5 6.5 0 0 0 0-13zm5 11 5 5",
        IconName::Serial => "M7 4h10v5h-2v4h-2V9h-2v4H9V9H7V4zm2 12h6v4H9v-4z",
        IconName::Shell => "M4 6h16v12H4V6zm3 4 3 2-3 2m5 0h4",
        IconName::Ssh => "M7 7h10v7H7V7zm3 7v3m4-3v3M8 20h8M5 10H3m18 0h-2",
        IconName::Tcp => "M4 8h16M4 16h16M8 4v16m8-16v16",
        IconName::Telnet => "M4 5h16v12H9l-5 3V5zm4 4 3 2-3 2m5 0h3",
    };

    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.7",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            path { d: "{path}" }
        }
    }
}
