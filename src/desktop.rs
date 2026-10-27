//! An opt-in local window over the existing daemon directory. No server, message consumption or agent registration.

#[cfg(all(windows, feature = "desktop"))]
mod windows;
#[cfg(all(windows, feature = "desktop"))]
pub use windows::run;

#[cfg(not(all(windows, feature = "desktop")))]
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("Desktop requires Windows and a build with --features desktop")
}

#[cfg(any(test, all(windows, feature = "desktop")))]
mod model {
    use crate::proto::Directory;
    #[cfg(all(windows, feature = "desktop"))]
    use crate::{client::Link, proto::Request, util};
    use serde::{Deserialize, Serialize};
    #[cfg(all(windows, feature = "desktop"))]
    use std::time::Duration;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
    pub enum Command {
        Ready {},
        Refresh {},
        CopyAddress { address: String },
        CopyTeamLink {},
    }

    impl Command {
        pub fn parse(text: &str) -> Option<Self> {
            (text.len() <= 1024).then(|| serde_json::from_str(text).ok()).flatten()
        }
    }

    #[derive(Debug, Serialize)]
    pub struct Snapshot {
        pub directory: Option<Directory>,
        pub error: Option<&'static str>,
        pub updated_at: i64,
    }

    #[cfg(all(windows, feature = "desktop"))]
    pub async fn read(link: &std::sync::Arc<Link>) -> Snapshot {
        let directory =
            link.call_within(Request::Who { me: None, filter: None, all: false }, Duration::from_secs(2)).await.ok();
        Snapshot {
            error: directory
                .is_none()
                .then_some("Local daemon unavailable. Run reason-network who to start it, then refresh."),
            directory,
            updated_at: util::now_ms(),
        }
    }

    pub fn can_copy(directory: Option<&Directory>, address: &str) -> bool {
        directory.is_some_and(|d| d.sessions.iter().any(|s| s.address == address))
    }

    pub fn update_script(snapshot: &Snapshot) -> serde_json::Result<String> {
        Ok(format!("window.reasonDesktop.update({});", serde_json::to_string(snapshot)?))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::proto::{SessionInfo, Tier};

        #[test]
        fn ipc_only_accepts_fixed_bounded_commands() {
            assert_eq!(Command::parse(r#"{"action":"refresh"}"#), Some(Command::Refresh {}));
            assert_eq!(
                Command::parse(r#"{"action":"copy_address","address":"你好/codex"}"#),
                Some(Command::CopyAddress { address: "你好/codex".into() })
            );
            for input in [
                r#"{"action":"exec","command":"powershell"}"#,
                r#"{"action":"refresh","command":"x"}"#,
                r#"{"action":"copy_address"}"#,
                "{",
            ] {
                assert!(Command::parse(input).is_none());
            }
            assert!(Command::parse(&"x".repeat(1025)).is_none());
            assert_eq!(Command::parse(r#"{"action":"ready"}"#), Some(Command::Ready {}));
            assert_eq!(Command::parse(r#"{"action":"copy_team_link"}"#), Some(Command::CopyTeamLink {}));
        }

        #[test]
        fn clipboard_addresses_must_come_from_the_current_directory() {
            let d = Directory {
                team: Some("test".into()),
                person: "me".into(),
                machine: "pc".into(),
                machines: vec![],
                sessions: vec![SessionInfo {
                    address: "alice/api@pc".into(),
                    name: "api".into(),
                    person: "alice".into(),
                    machine: "pc".into(),
                    agent: "codex".into(),
                    cwd: "C:\\项目 空格".into(),
                    repo: None,
                    branch: None,
                    status: "idle".into(),
                    since: None,
                    doing: None,
                    tier: Tier::Pull,
                    unread: 3,
                    is_me: false,
                }],
            };
            assert!(can_copy(Some(&d), "alice/api@pc"));
            assert!(!can_copy(Some(&d), "alice/api"));
            assert!(!can_copy(Some(&d), "alice/api@pc; powershell"));
            assert!(!can_copy(None, "alice/api@pc"));
            let snapshot = Snapshot { directory: Some(d), error: None, updated_at: 1 };
            let script = update_script(&snapshot).unwrap();
            let json = script.strip_prefix("window.reasonDesktop.update(").unwrap().strip_suffix(");").unwrap();
            let v: serde_json::Value = serde_json::from_str(json).unwrap();
            assert_eq!(v["directory"]["sessions"][0]["cwd"], "C:\\项目 空格");
            assert_eq!(v["directory"]["sessions"][0]["unread"], 3);
        }
    }
}
