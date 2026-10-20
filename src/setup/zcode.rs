//! ZCode Desktop (https://zcode.z.ai/en/docs/mcp-services). Its user MCP servers live in `~/.zcode/cli/config.json`
//! (`mcp.servers`); while that has none, it reads `~/.agents/mcp.json` (`mcpServers`) instead, so a server added to the
//! first would hide every server in the second. Workspace servers can't mask ours: a user server wins on a name clash.

use super::*;

const HOOK_MARKER: &str = "Reason Network (managed)";

fn hook_entry(exe: &Path, event: &str, state: Option<&str>) -> Value {
    let mut args = vec!["hook", "zcode", event];
    if let Some(state) = state {
        args.extend(["--network-home", state]);
    }
    json!({"type":"process", "command":exe, "args":args, "timeoutMs":3000, "statusMessage":HOOK_MARKER})
}

fn our_hook(entry: &Value) -> bool {
    entry["type"] == "process"
        && entry["statusMessage"] == HOOK_MARKER
        && entry["command"].is_string()
        && entry["args"].as_array().is_some_and(|a| {
            let prefix = a.len() >= 3
                && a[0] == "hook"
                && a[1] == "zcode"
                && matches!(a[2].as_str(), Some("session-start" | "post-tool-use"));
            prefix && (a.len() == 3 || (a.len() == 5 && a[3] == "--network-home" && a[4].is_string()))
        })
}

fn merge_hooks(value: &mut Value, exe: &Path, remove: bool, state: Option<&str>) -> Result<()> {
    let root = obj(value)?;
    if remove && !root.contains_key("hooks") {
        return Ok(());
    }
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().ok_or_else(|| anyhow!("ZCode hooks is not an object; not touching it"))?;
    if hooks.get("enabled").is_some_and(|v| !v.is_boolean()) {
        return Err(anyhow!("ZCode hooks.enabled is not a boolean; not touching it"));
    }
    let enabled = hooks.get("enabled") == Some(&json!(true));
    if remove && !hooks.contains_key("events") {
        return Ok(());
    }
    let events = hooks.entry("events").or_insert_with(|| json!({}));
    let events =
        events.as_object_mut().ok_or_else(|| anyhow!("ZCode hooks.events is not an object; not touching it"))?;
    // Enabling the global switch must not silently activate the user's disabled hooks.
    if !remove && !enabled && events.values().any(|v| v.as_array().is_none_or(|a| !a.is_empty())) {
        return Err(anyhow!("ZCode has disabled user hooks; enable them explicitly before installing Reason hooks"));
    }
    let mut empty = Vec::new();
    for (name, groups) in events.iter_mut() {
        let groups = groups.as_array_mut().ok_or_else(|| anyhow!("ZCode hook event is not an array"))?;
        for group in groups.iter() {
            if group.get("hooks").and_then(Value::as_array).is_none() {
                return Err(anyhow!("ZCode hook group has no hooks array"));
            }
        }
        let mut removed = false;
        groups.retain_mut(|group| {
            let list = group.get_mut("hooks").and_then(Value::as_array_mut).expect("validated above");
            let before = list.len();
            list.retain(|h| !our_hook(h));
            let changed = before != list.len();
            removed |= changed;
            !changed || !list.is_empty()
        });
        if removed && groups.is_empty() {
            empty.push(name.clone());
        }
    }
    for name in empty {
        events.remove(&name);
    }
    if !remove {
        for (name, event) in [("SessionStart", "session-start"), ("PostToolUse", "post-tool-use")] {
            events
                .entry(name)
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .unwrap()
                .push(json!({"hooks":[hook_entry(exe, event, state)]}));
        }
        hooks.insert("enabled".into(), json!(true));
    }
    Ok(())
}

pub(super) fn installed() -> bool {
    let dir = home().join(".zcode");
    dir.join("cli").is_dir() || dir.join("v2").is_dir()
}

pub(super) fn wired(exe: &Path) -> bool {
    wired_in(&home(), exe) && hooks_wired(&home(), exe)
}

fn hooks_wired(home: &Path, exe: &Path) -> bool {
    read_json(&files(home)[0].0).is_ok_and(|v| {
        v["hooks"]["enabled"] == true
            && ["SessionStart", "PostToolUse"].iter().all(|event| {
                v["hooks"]["events"][event].as_array().is_some_and(|groups| {
                    groups.iter().any(|g| {
                        g["hooks"].as_array().is_some_and(|a| {
                            a.iter().any(|h| our_hook(h) && h["command"].as_str().is_some_and(|c| same_path(c, exe)))
                        })
                    })
                })
            })
    })
}

/// Whether ours is in the file ZCode reads: its own while that has any server, else the `.agents` one.
fn wired_in(home: &Path, exe: &Path) -> bool {
    let read = |(path, native): (PathBuf, bool)| -> Result<Map<String, Value>> {
        Ok(servers(&mut read_json(&path)?, native)?.clone())
    };
    let [native, fallback] = files(home);
    let servers = match read(native) {
        Ok(s) if s.is_empty() => read(fallback),
        s => s,
    };
    servers.is_ok_and(|s| s.get(AGENT_NAME).is_some_and(|e| contains(&e.to_string(), &me(exe))))
}

pub(super) fn apply(exe: &Path, remove: bool, dry: bool) -> Result<Option<Report>> {
    if !installed() {
        return Ok(None);
    }
    apply_in(&home(), exe, remove, dry).map(Some)
}

/// ZCode's own file, then the `.agents` one; `true` for ZCode's own.
fn files(home: &Path) -> [(PathBuf, bool); 2] {
    [(home.join(".zcode/cli/config.json"), true), (home.join(".agents/mcp.json"), false)]
}

fn servers(v: &mut Value, native: bool) -> Result<&mut Map<String, Value>> {
    let root = obj(v)?;
    let mcp = if native { obj(root.entry("mcp").or_insert_with(|| json!({})))? } else { root };
    obj(mcp.entry(if native { "servers" } else { "mcpServers" }).or_insert_with(|| json!({})))
}

fn apply_in(home: &Path, exe: &Path, remove: bool, dry: bool) -> Result<Report> {
    let mut r = Report { agent: "ZCode Desktop", lines: Vec::new(), notes: Vec::new() };
    let [native, fallback] = files(home);
    let state = crate::paths::env_var("HOME").ok();
    let mut hooks = read_json(&native.0)?;
    let original = hooks.clone();
    merge_hooks(&mut hooks, exe, remove, state.as_deref())?;
    // Our own entry doesn't count: ZCode should go on reading `.agents` when ours is all its own file has.
    let to_fallback = !remove
        && !servers(&mut read_json(&native.0)?, true)?.keys().any(|k| k != AGENT_NAME)
        && !servers(&mut read_json(&fallback.0)?, false)?.is_empty();
    // Removing takes ours out of both: which one ZCode reads may have changed since. Moving writes `.agents` first,
    // so a failed write there leaves ours where it was.
    let targets = if remove {
        vec![native, fallback]
    } else if to_fallback {
        vec![fallback, native]
    } else {
        vec![native]
    };
    let mut hooks_written = false;
    for (path, native) in targets {
        let mut v = read_json(&path)?;
        let servers = servers(&mut v, native)?;
        if remove || (native && to_fallback) {
            let removed = servers.remove(AGENT_NAME).is_some();
            if !removed && !(native && original != hooks) {
                continue;
            }
        } else {
            servers.insert(
                AGENT_NAME.into(),
                json!({ "type": "stdio", "command": exe.display().to_string(), "args": ["mcp"], "env": { "REASON_NETWORK_AGENT": "zcode" } }),
            );
            if let Some(state) = &state {
                servers.get_mut(AGENT_NAME).unwrap()["env"]["REASON_NETWORK_HOME"] = json!(state);
            }
        }
        if native {
            if let Some(value) = hooks.get("hooks") {
                v["hooks"] = value.clone();
            }
            hooks_written = true;
        }
        r.lines.push(changed(&path, write_json(&path, &v, dry)?));
    }
    if !hooks_written && original != hooks {
        r.lines.push(changed(&files(home)[0].0, write_json(&files(home)[0].0, &hooks, dry)?));
    }
    if r.lines.is_empty() {
        r.lines.push("no MCP server to remove".into());
    } else if !remove {
        r.notes.push("Start a new ZCode session to load SessionStart/PostToolUse hooks; inbox recovery remains available. No idle wake-up.".into());
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let h = std::env::temp_dir().join(format!("reason-zcode-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(h.join(".zcode/cli")).unwrap();
        h
    }

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(path: &Path) -> Value {
        read_json(path).unwrap()
    }

    #[test]
    fn adds_ours_beside_other_settings_and_takes_it_out_again() {
        let h = scratch();
        let [(native, _), (fallback, _)] = files(&h);
        put(&native, r#"{"provider":"keep","mcp":{"other":7,"servers":{"memory":{"command":"npx"}}}}"#);
        let exe = Path::new("/opt/reason/reason-network");
        apply_in(&h, exe, false, false).unwrap();
        let once = std::fs::read(&native).unwrap();
        apply_in(&h, exe, false, false).unwrap();
        assert_eq!(std::fs::read(&native).unwrap(), once);
        let v = read(&native);
        assert_eq!(v["provider"], "keep");
        assert_eq!(v["mcp"]["other"], 7);
        assert_eq!(v["mcp"]["servers"][AGENT_NAME]["args"], json!(["mcp"]));
        assert!(!fallback.exists());
        // By name, wherever the binary is now.
        apply_in(&h, Path::new("/elsewhere/reason-network"), true, false).unwrap();
        assert_eq!(read(&native)["mcp"]["servers"], json!({"memory": {"command": "npx"}}));
        std::fs::remove_dir_all(h).unwrap();
    }

    #[test]
    fn goes_where_zcode_reads_its_servers() {
        let h = scratch();
        let [(native, _), (fallback, _)] = files(&h);
        put(&native, r#"{"mcp":{"servers":{}}}"#);
        put(&fallback, r#"{"mcpServers":{"memory":{"command":"npx"}}}"#);
        apply_in(&h, Path::new("/opt/reason-network"), false, false).unwrap();
        assert_eq!(read(&native)["mcp"]["servers"], json!({}));
        assert!(read(&fallback)["mcpServers"][AGENT_NAME].is_object());
        apply_in(&h, Path::new("/opt/reason-network"), true, false).unwrap();
        assert_eq!(read(&fallback), json!({"mcpServers": {"memory": {"command": "npx"}}}));
        std::fs::remove_dir_all(h).unwrap();
    }

    #[test]
    fn moves_to_agents_when_ours_is_all_zcode_has() {
        let h = scratch();
        let [(native, _), (fallback, _)] = files(&h);
        let exe = Path::new("/opt/reason-network");
        apply_in(&h, exe, false, false).unwrap();
        put(&fallback, r#"{"mcpServers":{"memory":{"command":"npx"}}}"#);
        apply_in(&h, exe, false, false).unwrap();
        assert_eq!(read(&native)["mcp"]["servers"], json!({}));
        assert!(read(&fallback)["mcpServers"][AGENT_NAME].is_object());
        std::fs::remove_dir_all(h).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn keeps_ours_when_agents_cant_be_written() {
        use std::os::unix::fs::PermissionsExt;
        let h = scratch();
        let [(native, _), (fallback, _)] = files(&h);
        let exe = Path::new("/opt/reason-network");
        apply_in(&h, exe, false, false).unwrap();
        let before = std::fs::read(&native).unwrap();
        put(&fallback, r#"{"mcpServers":{"memory":{"command":"npx"}}}"#);
        let dir = fallback.parent().unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root writes anyway: nothing to check then.
        let checked = std::fs::write(dir.join("probe"), "").is_err();
        let failed = apply_in(&h, exe, false, false).is_err();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if checked {
            assert!(failed);
            assert_eq!(std::fs::read(&native).unwrap(), before);
            assert!(wired_in(&h, exe));
        }
        std::fs::remove_dir_all(h).unwrap();
    }

    #[test]
    fn wired_only_where_zcode_reads() {
        let h = scratch();
        let [(native, _), (fallback, _)] = files(&h);
        let exe = Path::new("/opt/reason-network");
        assert!(!wired_in(&h, exe));
        apply_in(&h, exe, false, false).unwrap();
        assert!(wired_in(&h, exe));
        assert!(!wired_in(&h, Path::new("/elsewhere/reason-network")));
        // In `.agents`, ours counts only while ZCode's own file has no server.
        put(&fallback, &json!({"mcpServers": {AGENT_NAME: {"command": "/opt/reason-network"}}}).to_string());
        put(&native, r#"{"mcp":{"servers":{}}}"#);
        assert!(wired_in(&h, exe));
        put(&native, r#"{"mcp":{"servers":{"memory":{"command":"npx"}}}}"#);
        assert!(!wired_in(&h, exe));
        std::fs::remove_dir_all(h).unwrap();
    }

    #[test]
    fn leaves_a_config_it_cant_read_alone() {
        let h = scratch();
        let [(native, _), _] = files(&h);
        for bad in ["{", "[]", r#"{"mcp":[]}"#, r#"{"mcp":{"servers":false}}"#] {
            put(&native, bad);
            assert!(apply_in(&h, Path::new("/opt/reason-network"), false, false).is_err());
            assert_eq!(std::fs::read_to_string(&native).unwrap(), bad);
        }
        std::fs::remove_dir_all(h).unwrap();
    }

    #[test]
    fn hook_install_preserves_user_settings_and_removes_after_the_binary_moves() {
        let exe = Path::new(r"C:\程序 空格\reason-network.exe");
        let mut v = json!({"provider":"keep","hooks":{"enabled":true,"events":{"PostToolUse":[
            {"matcher":"Read","hooks":[{"type":"process","command":"user-hook"}]}]}}});
        let original = v.clone();
        merge_hooks(&mut v, exe, false, Some(r"C:\Reason state 空格")).unwrap();
        let once = v.clone();
        merge_hooks(&mut v, exe, false, Some(r"C:\Reason state 空格")).unwrap();
        assert_eq!(v, once);
        let h = &v["hooks"]["events"]["SessionStart"][0]["hooks"][0];
        assert_eq!(h["command"], json!(exe));
        assert_eq!(h["args"], json!(["hook", "zcode", "session-start", "--network-home", r"C:\Reason state 空格"]));
        merge_hooks(&mut v, Path::new("/new/reason-network"), true, None).unwrap();
        assert_eq!(v, original);
    }

    #[test]
    fn disabled_or_invalid_user_hooks_block_install_before_any_write() {
        let h = scratch();
        let native = files(&h)[0].0.clone();
        for hooks in [
            json!(null),
            json!({"events":[]}),
            json!({"enabled":false,"events":{"Stop":[
            {"hooks":[{"type":"process","command":"user-hook"}]}]}}),
        ] {
            let text = json!({"provider":"keep","hooks":hooks}).to_string();
            put(&native, &text);
            assert!(apply_in(&h, Path::new("/opt/reason-network"), false, false).is_err());
            assert_eq!(std::fs::read_to_string(&native).unwrap(), text);
        }
        std::fs::remove_dir_all(h).unwrap();
    }
}
