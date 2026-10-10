//! Choosing a backend per window. Both ends use this module: the host
//! classifies each window's content when it lists windows ([`classify`]),
//! the client picks a backend from the user's own rules and then the
//! default table ([`choose_backend`]), and the host serves that backend if
//! it can, otherwise the native one ([`serve`]). docs/BACKENDS.md has the
//! default tables in prose.

use serde::{Deserialize, Serialize};

use crate::{BackendKind, ContentHint, WindowInfo};

/// Which windows a rule applies to. Every field that is set must match;
/// a rule with nothing set matches every window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowMatch {
    /// The window's app id, compared case-insensitively.
    pub app_id: Option<String>,
    /// Text the title contains, compared case-insensitively.
    pub title_contains: Option<String>,
    pub content: Option<ContentHint>,
}

impl WindowMatch {
    pub fn matches(&self, window: &WindowInfo) -> bool {
        let app_id_ok = self
            .app_id
            .as_ref()
            .is_none_or(|app_id| app_id.eq_ignore_ascii_case(&window.app_id));
        let title_ok = self
            .title_contains
            .as_ref()
            .is_none_or(|text| window.title.to_lowercase().contains(&text.to_lowercase()));
        let content_ok = self.content.is_none_or(|content| content == window.content);
        app_id_ok && title_ok && content_ok
    }
}

/// "Windows like this use that backend."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendRule {
    pub when: WindowMatch,
    pub backend: BackendKind,
}

/// The default rules, by content: text over RDP (sharp text, little
/// bandwidth), games over GameStream (latency, controllers), video by
/// passthrough (no second encode). Everything else stays native.
pub fn default_rules() -> Vec<BackendRule> {
    [
        (ContentHint::Text, BackendKind::Rdp),
        (ContentHint::Game, BackendKind::GameStream),
        (ContentHint::Video, BackendKind::Passthrough),
    ]
    .into_iter()
    .map(|(content, backend)| BackendRule {
        when: WindowMatch {
            content: Some(content),
            ..Default::default()
        },
        backend,
    })
    .collect()
}

/// Client side: the backend to ask for. The user's rules come first (a
/// per-app override is a rule naming that app id), then the defaults;
/// with no match, native.
pub fn choose_backend(window: &WindowInfo, user_rules: &[BackendRule]) -> BackendKind {
    user_rules
        .iter()
        .chain(default_rules().iter())
        .find(|rule| rule.when.matches(window))
        .map_or(BackendKind::Native, |rule| rule.backend)
}

/// Host side: the backend it will actually use, given what it can serve.
/// Native is always available: it is the fallback for everything.
pub fn serve(requested: BackendKind, available: &[BackendKind]) -> BackendKind {
    if available.contains(&requested) {
        requested
    } else {
        BackendKind::Native
    }
}

/// App ids (Wayland app_id, or a Windows executable name) the host
/// classifies by default. Matching is case-insensitive.
const TEXT_APPS: &[&str] = &[
    // Terminals
    "foot",
    "alacritty",
    "kitty",
    "org.wezfurlong.wezterm",
    "org.gnome.terminal",
    "org.gnome.ptyxis",
    "org.kde.konsole",
    "xterm",
    "windowsterminal.exe",
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    // Editors and IDEs
    "code",
    "code.exe",
    "codium",
    "org.gnome.texteditor",
    "org.kde.kate",
    "gedit",
    "emacs",
    "neovide",
    "notepad.exe",
    "notepad++.exe",
    "devenv.exe",
    "libreoffice-writer",
    "winword.exe",
];

const VIDEO_APPS: &[&str] = &[
    "mpv",
    "vlc",
    "org.videolan.vlc",
    "io.github.celluloid_player.celluloid",
    "org.gnome.totem",
    "vlc.exe",
    "mpv.exe",
    "mpc-hc64.exe",
    "mpc-be64.exe",
];

/// Web browsers: what they show depends on the page, so the title decides.
const BROWSERS: &[&str] = &[
    "firefox",
    "org.mozilla.firefox",
    "chromium",
    "google-chrome",
    "brave-browser",
    "firefox.exe",
    "floorp.exe",
    "chrome.exe",
    "msedge.exe",
    "brave.exe",
    "opera.exe",
    "vivaldi.exe",
];

/// Video sites, as browsers put them in the window title.
const VIDEO_SITES: &[&str] = &["youtube", "netflix", "twitch", "prime video", "vimeo"];

/// Host side: what a window shows, from its app id (and for a browser, the
/// page title). Steam sets the app id `steam_app_<id>` on every game it
/// runs; gamescope hosts games too.
pub fn classify(app_id: &str, title: &str) -> ContentHint {
    let app_id = app_id.to_ascii_lowercase();
    let title = title.to_lowercase();
    if app_id.starts_with("steam_app_") || app_id == "gamescope" {
        ContentHint::Game
    } else if TEXT_APPS.contains(&app_id.as_str()) {
        ContentHint::Text
    } else if VIDEO_APPS.contains(&app_id.as_str())
        || BROWSERS.contains(&app_id.as_str())
            && VIDEO_SITES.iter().any(|site| title.contains(site))
    {
        ContentHint::Video
    } else {
        ContentHint::General
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WindowId;

    fn window(app_id: &str, title: &str) -> WindowInfo {
        WindowInfo {
            id: WindowId(1),
            title: title.into(),
            app_id: app_id.into(),
            width: 800,
            height: 600,
            focused: false,
            owner: None,
            kind: crate::WindowKind::Normal,
            position: None,
            content: classify(app_id, title),
        }
    }

    #[test]
    fn default_table_by_content() {
        assert_eq!(
            choose_backend(&window("org.gnome.TextEditor", "notes"), &[]),
            BackendKind::Rdp
        );
        assert_eq!(
            choose_backend(&window("steam_app_620", "Portal 2"), &[]),
            BackendKind::GameStream
        );
        assert_eq!(
            choose_backend(&window("mpv", "film.mkv"), &[]),
            BackendKind::Passthrough
        );
        assert_eq!(
            choose_backend(&window("org.gnome.Nautilus", "Home"), &[]),
            BackendKind::Native
        );
    }

    #[test]
    fn a_user_rule_for_an_app_wins_over_the_defaults() {
        let user = [BackendRule {
            when: WindowMatch {
                app_id: Some("CODE".into()),
                ..Default::default()
            },
            backend: BackendKind::Native,
        }];
        assert_eq!(
            choose_backend(&window("code", "main.rs"), &user),
            BackendKind::Native
        );
        assert_eq!(
            choose_backend(&window("foot", "shell"), &user),
            BackendKind::Rdp
        );
    }

    #[test]
    fn title_rules_match_case_insensitively() {
        let user = [BackendRule {
            when: WindowMatch {
                title_contains: Some("youtube".into()),
                ..Default::default()
            },
            backend: BackendKind::Passthrough,
        }];
        assert_eq!(
            choose_backend(&window("firefox", "Something - YouTube"), &user),
            BackendKind::Passthrough
        );
    }

    #[test]
    fn a_browser_is_video_only_on_a_video_site() {
        assert_eq!(
            classify("chrome.exe", "Some talk - YouTube - Google Chrome"),
            ContentHint::Video
        );
        assert_eq!(
            classify("firefox", "Rust documentation - Mozilla Firefox"),
            ContentHint::General
        );
    }

    #[test]
    fn the_host_falls_back_to_native() {
        assert_eq!(
            serve(BackendKind::Rdp, &[BackendKind::Native]),
            BackendKind::Native
        );
        assert_eq!(
            serve(BackendKind::Rdp, &[BackendKind::Native, BackendKind::Rdp]),
            BackendKind::Rdp
        );
    }
}
