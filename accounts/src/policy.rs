//! Who may connect to a host and what they may do there: an ordered list
//! of rules, the first that matches deciding. No rules keeps windowcast's
//! behaviour without accounts: everyone authenticated sees every window.

use serde::{Deserialize, Serialize};

use crate::Account;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rule {
    /// `user:<name>`, `group:<name>`, `method:<pin|password|oidc|kerberos>`,
    /// `provider:<name>` or `*`; the rule matches when any entry does.
    pub who: Vec<String>,
    /// Globs on the host's name; empty matches every host.
    pub hosts: Vec<String>,
    /// `false` refuses the connection.
    pub allow: bool,
    /// Globs on a window's app id or title; empty allows every window.
    pub windows: Vec<String>,
    /// Whether the session may send input; `None` leaves it to the host's
    /// own setting.
    pub input: Option<bool>,
    /// Whether the session may use the command stream (shells, launching
    /// applications); `None` leaves it to the host's own setting.
    pub commands: Option<bool>,
}

impl Default for Rule {
    fn default() -> Self {
        Rule {
            who: Vec::new(),
            hosts: Vec::new(),
            allow: true,
            windows: Vec::new(),
            input: None,
            commands: None,
        }
    }
}

/// What policy lets one session do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub allow: bool,
    /// Window globs; empty allows every window.
    pub windows: Vec<String>,
    pub input: Option<bool>,
    pub commands: Option<bool>,
    /// The host application's own limit for this one device, on top of
    /// the policy's: window globs it may see, empty for no limit
    /// ([`Decision::limited_to`], Droidtop/tracker#116). A window must
    /// pass both.
    pub device_windows: Vec<String>,
}

impl Decision {
    /// Whether a window with this app id and title may be listed,
    /// streamed and given input.
    pub fn window_allowed(&self, app_id: &str, title: &str) -> bool {
        let passes = |globs: &[String]| {
            globs.is_empty()
                || globs
                    .iter()
                    .any(|pattern| glob(pattern, app_id) || glob(pattern, title))
        };
        passes(&self.windows) && passes(&self.device_windows)
    }

    /// This decision, with the device further limited to the windows
    /// `globs` match (app id or title), as a host application sets it for
    /// one device. Empty takes the limit away.
    pub fn limited_to(mut self, globs: &[String]) -> Self {
        self.device_windows = globs.to_vec();
        self
    }
}

impl Policy {
    /// The decision for `account` (`None`: a device paired by PIN, which
    /// rules reach with `method:pin`) on the host named `host`.
    pub fn decide(&self, account: Option<&Account>, host: &str) -> Decision {
        if self.rules.is_empty() {
            return Decision {
                allow: true,
                windows: Vec::new(),
                input: None,
                commands: None,
                device_windows: Vec::new(),
            };
        }
        let rule = self.rules.iter().find(|rule| {
            (rule.hosts.is_empty() || rule.hosts.iter().any(|h| glob(h, host)))
                && rule.who.iter().any(|who| matches(who, account))
        });
        match rule {
            Some(rule) => Decision {
                allow: rule.allow,
                windows: rule.windows.clone(),
                input: rule.input,
                commands: rule.commands,
                device_windows: Vec::new(),
            },
            None => Decision {
                allow: false,
                windows: Vec::new(),
                input: Some(false),
                commands: Some(false),
                device_windows: Vec::new(),
            },
        }
    }
}

fn matches(who: &str, account: Option<&Account>) -> bool {
    if who == "*" {
        return true;
    }
    let Some((kind, value)) = who.split_once(':') else {
        return false;
    };
    match (kind, account) {
        ("method", None) => value == "pin",
        ("method", Some(a)) => value == a.method.name(),
        ("user", Some(a)) => value == a.name,
        ("group", Some(a)) => a.groups.iter().any(|g| g == value),
        ("provider", Some(a)) => value == a.provider,
        _ => false,
    }
}

/// `*` matches any run of characters, `?` any one; case-insensitive, as
/// window titles and host names are not reliably cased.
pub(crate) fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Method;

    fn account(name: &str, groups: &[&str], method: Method, provider: &str) -> Account {
        Account {
            name: name.into(),
            groups: groups.iter().map(|g| g.to_string()).collect(),
            method,
            provider: provider.into(),
        }
    }

    fn rule(who: &[&str]) -> Rule {
        Rule {
            who: who.iter().map(|w| w.to_string()).collect(),
            ..Rule::default()
        }
    }

    #[test]
    fn globs() {
        assert!(glob("*", ""));
        assert!(glob("build-*", "BUILD-07"));
        assert!(glob("*terminal*", "Windows Terminal - bash"));
        assert!(glob("a?c", "abc"));
        assert!(!glob("a?c", "ac"));
        assert!(!glob("code", "vscode"));
        assert!(glob("*code", "vscode"));
    }

    #[test]
    fn no_rules_allow_everyone() {
        let decision = Policy::default().decide(None, "host");
        assert!(decision.allow);
        assert!(decision.window_allowed("anything", "at all"));
        assert_eq!(decision.input, None);
    }

    #[test]
    fn first_matching_rule_decides() {
        let policy = Policy {
            rules: vec![
                Rule {
                    windows: vec!["*".into()],
                    input: Some(true),
                    commands: Some(true),
                    ..rule(&["group:admins"])
                },
                Rule {
                    hosts: vec!["build-*".into()],
                    windows: vec!["code".into(), "*Terminal*".into()],
                    ..rule(&["group:staff", "provider:corp"])
                },
                Rule {
                    input: Some(false),
                    ..rule(&["method:pin"])
                },
                Rule {
                    allow: false,
                    ..rule(&["*"])
                },
            ],
        };
        let admin = account("root", &["admins", "staff"], Method::Password, "ldap");
        assert_eq!(policy.decide(Some(&admin), "anything").commands, Some(true));

        let staff = account("bob", &[], Method::Oidc, "corp");
        let on_build = policy.decide(Some(&staff), "build-01");
        assert!(on_build.allow);
        assert!(on_build.window_allowed("code", "main.rs - Visual Studio Code"));
        assert!(on_build.window_allowed("wt", "Windows Terminal"));
        assert!(!on_build.window_allowed("firefox", "Mail"));
        // Not a build host: falls through to the refusal.
        assert!(!policy.decide(Some(&staff), "laptop").allow);

        let paired = policy.decide(None, "laptop");
        assert!(paired.allow);
        assert_eq!(paired.input, Some(false));

        let stranger = account("eve", &[], Method::Kerberos, "EXAMPLE.ORG");
        assert!(!policy.decide(Some(&stranger), "build-01").allow);
    }

    #[test]
    fn no_match_refuses() {
        let policy = Policy {
            rules: vec![rule(&["user:alice"])],
        };
        let alice = account("alice", &[], Method::Password, "local");
        let bob = account("bob", &[], Method::Password, "local");
        assert!(policy.decide(Some(&alice), "h").allow);
        assert!(!policy.decide(Some(&bob), "h").allow);
        assert!(!policy.decide(None, "h").allow);
    }

    #[test]
    fn reads_the_documented_json() {
        let policy: Policy = serde_json::from_str(
            r#"{ "rules": [
              { "who": ["group:admins"], "windows": ["*"], "input": true, "commands": true },
              { "who": ["group:staff", "provider:corp"], "hosts": ["build-*"], "windows": ["*Terminal*", "code"], "input": true },
              { "who": ["*"], "allow": false }
            ] }"#,
        )
        .unwrap();
        assert_eq!(policy.rules.len(), 3);
        assert!(policy.rules[0].allow);
        assert!(!policy.rules[2].allow);
    }
}

#[cfg(test)]
mod device_limit_tests {
    use super::*;

    #[test]
    fn a_device_limit_narrows_the_policy_and_both_must_pass() {
        let open = Policy::default().decide(None, "host");
        assert!(open.window_allowed("notepad.exe", "Untitled"));
        let limited = open.clone().limited_to(&["sandustry*".to_string()]);
        assert!(limited.window_allowed("Sandustry.exe", "Sandustry"));
        assert!(!limited.window_allowed("notepad.exe", "Untitled"));
        // A policy that allows only notepad and a limit to the game: nothing passes both.
        let mut notepad = open.clone();
        notepad.windows = vec!["notepad*".into()];
        assert!(!notepad
            .clone()
            .limited_to(&["sandustry*".to_string()])
            .window_allowed("notepad.exe", "Untitled"));
        // Empty takes the limit away.
        assert!(limited
            .limited_to(&[])
            .window_allowed("notepad.exe", "Untitled"));
    }
}
