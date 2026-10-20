//! ZCode has no hook-output acceptance callback. Only lease mail here, never acknowledge stdout as delivery.
use super::*;
use crate::render;

const LIMIT: Duration = Duration::from_millis(1500);
pub(crate) const OUTPUT_LIMIT: usize = 24 * 1024;

/// The process executor has no `env` field. Re-exec only this fixed hook with its configured state directory.
pub(crate) async fn at_home(event: &str, home: std::path::PathBuf) {
    if !matches!(event, "session-start" | "post-tool-use") {
        return;
    }
    let Ok(exe) = crate::util::current_exe() else { return };
    let mut child = tokio::process::Command::new(exe);
    child.args(["hook", "zcode", event]).env("REASON_NETWORK_HOME", home).kill_on_drop(true);
    // The short-lived state wrapper is not the host process that keeps the session alive.
    if let Some(pid) = agent::hook_agent_pid() {
        child.env("REASON_NETWORK_HOOK_PID", pid.to_string());
    }
    #[cfg(windows)]
    child.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    let _ = tokio::time::timeout(Duration::from_millis(2500), child.status()).await;
}

pub(super) async fn run(event: &str) {
    // Host stdin is a single JSON line. A bounded reader also handles a host that leaves stdin open.
    let input = read_input();
    if let Ok(Some(out)) = tokio::time::timeout(LIMIT, handle(&Link::existing(), event, &input)).await {
        let _ = writeln!(std::io::stdout(), "{out}");
    }
}

fn read_input() -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut line = String::new();
        let _ = std::io::BufReader::new(std::io::stdin().take(1024 * 1024)).read_line(&mut line);
        let _ = tx.send(line);
    });
    rx.recv_timeout(Duration::from_millis(300)).unwrap_or_default()
}

fn payload(event: &str, input: &str) -> Option<HookCall> {
    let name = match event {
        "session-start" => "SessionStart",
        "post-tool-use" => "PostToolUse",
        _ => return None,
    };
    let v: Value = serde_json::from_str(input.trim_start_matches('\u{feff}')).ok()?;
    let field = |snake: &str, camel: &str| {
        v.get(snake)
            .or_else(|| v.get(camel))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
    };
    if field("hook_event_name", "hookEventName")?.as_str() != name {
        return None;
    }
    Some(HookCall {
        agent: "zcode".into(),
        event: if event == "session-start" { "start" } else { "tool" }.into(),
        session_id: field("session_id", "sessionId")?,
        cwd: Some(field("cwd", "cwd")?),
        pid: crate::paths::env_var("HOOK_PID").ok().and_then(|p| p.parse().ok()).or_else(agent::hook_agent_pid),
        // SessionStart registers identity only. No pending mail is consumed by a hook.
        mail: event == "post-tool-use",
        // ZCode's transcript is a temporary event snapshot, not a persistent session log.
        transcript: None,
        doing: None,
        name: None,
    })
}

async fn handle(link: &std::sync::Arc<Link>, event: &str, input: &str) -> Option<String> {
    let call = payload(event, input)?;
    let reply: HookReply = link.call(Request::Hook(call)).await.ok()?;
    let text = match reply.text {
        Some(text) => text,
        None if event == "session-start" => format!(
            "Reason Network: you are {}. Messages arrive after tools while you work. Reply with send_message and the incoming id as reply_to; that confirms receipt, not task completion. check_inbox also recovers pending messages. Idle sessions are not woken.",
            reply.address?
        ),
        _ => return None,
    };
    let out = format_for("zcode", event, if event == "session-start" { "start" } else { "tool" }, &text);
    (out.len() <= OUTPUT_LIMIT).then_some(out)
}

/// Keep complete JSON below the host's 32 KiB stdout cap, including Windows ASCII escaping.
pub(crate) fn batch(messages: Vec<Message>) -> Vec<Message> {
    let mut selected = Vec::new();
    for message in messages {
        selected.push(message);
        let text = render::for_agent(&selected, Some("zcode"), false);
        if format_for("zcode", "post-tool-use", "tool", &text).len() > OUTPUT_LIMIT {
            selected.pop();
            continue; // Oversized messages remain unread and available through check_inbox.
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_real_session_and_event_in_both_host_spellings() {
        let a =
            payload("session-start", r#"{"session_id":"one","cwd":"C:\\用户 空格","hook_event_name":"SessionStart"}"#)
                .unwrap();
        let b = payload("post-tool-use", r#"{"sessionId":"two","cwd":"C:\\用户 空格","hookEventName":"PostToolUse"}"#)
            .unwrap();
        assert_eq!(a.session_id, "one");
        assert_eq!(b.session_id, "two");
        assert_ne!(format!("zcode:{}", a.session_id), format!("zcode:{}", b.session_id));
        assert_eq!(a.cwd.as_deref(), Some(r"C:\用户 空格"));
        assert!(!a.mail);
        assert!(b.mail);
        for bad in [
            "{}",
            "{",
            r#"{"session_id":"","cwd":"/","hook_event_name":"SessionStart"}"#,
            r#"{"session_id":"s","cwd":"/","hook_event_name":"Stop"}"#,
        ] {
            assert!(payload("session-start", bad).is_none());
        }
        assert!(payload("stop", "{}").is_none());
    }

    #[test]
    fn stdout_is_context_json_and_preserves_unicode() {
        for (event, ev, expected) in
            [("session-start", "start", "SessionStart"), ("post-tool-use", "tool", "PostToolUse")]
        {
            let out = format_for("zcode", event, ev, "你好 \"消息\" 🦀\nnext");
            let v: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(v["hookSpecificOutput"]["hookEventName"], expected);
            assert_eq!(v["hookSpecificOutput"]["additionalContext"], "你好 \"消息\" 🦀\nnext");
            assert!(v.get("decision").is_none());
        }
    }

    #[test]
    fn batching_keeps_json_complete_and_leaves_large_mail_for_the_inbox() {
        let message = |id: &str, body: String| Message {
            id: id.into(),
            from: "a/codex".into(),
            to: "b/zcode".into(),
            from_agent: Some("codex".into()),
            from_repo: None,
            via: None,
            reply_to: None,
            body,
            wake: true,
            notify: false,
            sent_at: 0,
            approval: None,
        };
        let selected = batch(vec![
            message("big", "中".repeat(64 * 1024)),
            message("one", "你好".into()),
            message("two", "hello".into()),
        ]);
        assert_eq!(selected.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["one", "two"]);
        let out = format_for("zcode", "post-tool-use", "tool", &render::for_agent(&selected, Some("zcode"), false));
        assert!(out.len() <= OUTPUT_LIMIT);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("你好"));
    }
}
