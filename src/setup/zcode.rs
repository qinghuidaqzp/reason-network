//! ZCode Desktop's native JSON MCP config, including its per-scope `.agents` fallback.
//! Contract: https://zcode.z.ai/cn/docs/mcp-services (also verified with Desktop 3.15.3).

use super::*;

const OWNER: &str = "REASON_NETWORK_MANAGED";
const MARKER: &str = "zcode-mcp-v1";
const HOOK_MARKER: &str = "Reason Network (managed)";

fn hook_entry(exe: &Path, event: &str, state: Option<&str>) -> Value {
    let mut args = vec!["hook", "zcode", event];
    if let Some(state) = state {
        args.extend(["--network-home", state]);
    }
    json!({"type":"process", "command":exe, "args":args, "timeoutMs":3000, "statusMessage":HOOK_MARKER})
}

fn our_hook(entry: &Value, exe: &Path) -> bool {
    entry["type"] == "process"
        && entry["statusMessage"] == HOOK_MARKER
        && entry["command"].as_str().is_some_and(|c| same_path(c, exe))
        && entry["args"].as_array().is_some_and(|a| {
            let prefix = a.len() >= 3
                && a[0] == "hook"
                && a[1] == "zcode"
                && matches!(a[2].as_str(), Some("session-start" | "post-tool-use"));
            prefix && (a.len() == 3 || (a.len() == 5 && a[3] == "--network-home" && a[4].is_string()))
        })
}

fn merge_hooks(value: &mut Value, exe: &Path, remove: bool, state: Option<&str>) -> Result<()> {
    let root = value.as_object_mut().expect("validated config");
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
            list.retain(|h| !our_hook(h, exe));
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

fn user_home() -> PathBuf {
    // ZCode uses HOME first, including on Windows; Reason's own state path is independent.
    std::env::var("HOME").ok().filter(|h| !h.trim().is_empty()).map(|h| PathBuf::from(h.trim())).unwrap_or_else(home)
}

fn detected(home: &Path) -> bool {
    home.join(".zcode/cli").is_dir() || home.join(".zcode/v2").is_dir()
}

pub(super) fn installed() -> bool {
    detected(&user_home())
}

struct ConfigFile {
    path: PathBuf,
    value: Value,
    native: bool,
}

impl ConfigFile {
    fn read(path: PathBuf, native: bool) -> Result<Self> {
        let value: Value = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("{} is not plain JSON", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        // Fail before writing anything; never replace a malformed section or echo config secrets in an error.
        let invalid =
            || anyhow!("{}: expected JSON objects for the config and MCP sections; not touching it", path.display());
        let root = value.as_object().ok_or_else(invalid)?;
        let servers = if native {
            match root.get("mcp") {
                Some(mcp) => mcp.as_object().ok_or_else(invalid)?.get("servers"),
                None => None,
            }
        } else {
            root.get("mcpServers")
        };
        if servers.is_some_and(|s| !s.is_object()) {
            return Err(invalid());
        }
        Ok(Self { path, value, native })
    }

    fn servers(&self) -> Option<&Map<String, Value>> {
        if self.native { self.value.get("mcp")?.get("servers") } else { self.value.get("mcpServers") }?.as_object()
    }

    fn servers_mut(&mut self) -> &mut Map<String, Value> {
        let root = self.value.as_object_mut().expect("validated on read");
        if self.native {
            root.entry("mcp")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap()
                .entry("servers")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap()
        } else {
            root.entry("mcpServers").or_insert_with(|| json!({})).as_object_mut().unwrap()
        }
    }

    fn entry(&self) -> Option<&Value> {
        self.servers()?.get(AGENT_NAME)
    }

    fn write(&self, dry: bool) -> Result<bool> {
        let content = serde_json::to_string_pretty(&self.value)? + "\n";
        if std::fs::read_to_string(&self.path).ok().as_deref() == Some(&content) {
            return Ok(false);
        }
        // Always keep the original, even when another entry in this file happens to use our executable.
        let backup = PathBuf::from(format!("{}.bak-reason", self.path.display()));
        if !dry && self.path.exists() && !backup.exists() {
            std::fs::copy(&self.path, backup)?;
        }
        write_file(&self.path, &content, dry)
    }
}

fn active(home: &Path, workspace: bool) -> Result<ConfigFile> {
    let native =
        ConfigFile::read(home.join(if workspace { ".zcode/config.json" } else { ".zcode/cli/config.json" }), true)?;
    // ZCode skips the entire fallback when the native source has even one server. Do not migrate or shadow it.
    if native.servers().is_some_and(|s| !s.is_empty()) {
        return Ok(native);
    }
    let fallback = ConfigFile::read(home.join(".agents/mcp.json"), false)?;
    Ok(if fallback.servers().is_some_and(|s| !s.is_empty()) { fallback } else { native })
}

fn owned(entry: &Value, exe: &Path) -> bool {
    entry.get("env").is_some_and(|env| {
        env.get(OWNER).and_then(Value::as_str) == Some(MARKER)
            && env.get("REASON_NETWORK_AGENT").and_then(Value::as_str) == Some("zcode")
    }) && entry.get("command").and_then(Value::as_str).is_some_and(|c| same_path(c, exe))
        && entry.get("args") == Some(&json!(["mcp"]))
        && entry.get("type").and_then(Value::as_str) == Some("stdio")
}

pub(super) fn wired(exe: &Path) -> bool {
    let home = user_home();
    active(&home, false).is_ok_and(|c| c.entry().is_some_and(|e| owned(e, exe)))
        && ConfigFile::read(home.join(".zcode/cli/config.json"), true).is_ok_and(|c| {
            c.value["hooks"]["enabled"] == true
                && ["SessionStart", "PostToolUse"].iter().all(|event| {
                    c.value["hooks"]["events"][event].as_array().is_some_and(|groups| {
                        groups.iter().any(|g| g["hooks"].as_array().is_some_and(|a| a.iter().any(|h| our_hook(h, exe))))
                    })
                })
        })
}

pub(super) fn apply(exe: &Path, remove: bool, dry: bool) -> Result<Option<Report>> {
    let home = user_home();
    if !detected(&home) {
        return Ok(None);
    }
    let cwd = std::env::current_dir()?;
    let git = crate::git::lookup(&cwd.to_string_lossy());
    let workspace = git.root.map(PathBuf::from).unwrap_or(cwd);
    apply_at(&home, Some(&workspace), exe, remove, dry, paths::env_var("HOME").ok().as_deref()).map(Some)
}

fn apply_at(
    home: &Path,
    workspace: Option<&Path>,
    exe: &Path,
    remove: bool,
    dry: bool,
    state: Option<&str>,
) -> Result<Report> {
    let mut report = Report { agent: "ZCode Desktop", lines: vec![], notes: vec![] };
    if remove {
        // Read both before editing either; a formerly active source may now be hidden by the other one.
        let mut configs = [
            ConfigFile::read(home.join(".zcode/cli/config.json"), true)?,
            ConfigFile::read(home.join(".agents/mcp.json"), false)?,
        ];
        let before = configs[0].value.clone();
        merge_hooks(&mut configs[0].value, exe, true, state)?;
        for config in &mut configs {
            let mut changed_hooks = config.native && config.value != before;
            if config.entry().is_some_and(|e| owned(e, exe)) {
                config.servers_mut().remove(AGENT_NAME);
                changed_hooks = true;
            }
            if changed_hooks {
                report.lines.push(changed(&config.path, config.write(dry)?));
            }
        }
        if report.lines.is_empty() {
            report.lines.push("no managed ZCode MCP server to remove".into());
        }
        return Ok(report);
    }
    let mut config = active(home, false)?;
    if let Some(workspace) = workspace {
        let project = active(workspace, true)?;
        if project.entry().is_some_and(|e| !owned(e, exe)) {
            return Err(anyhow!(
                "{} already defines {AGENT_NAME}; not masking a workspace server with a user server",
                project.path.display()
            ));
        }
    }
    if config.entry().is_some_and(|e| !owned(e, exe)) {
        return Err(anyhow!("{} already defines an unmanaged {AGENT_NAME}; not touching it", config.path.display()));
    }
    let mut native = ConfigFile::read(home.join(".zcode/cli/config.json"), true)?;
    // Validate all hook sections before writing the MCP source, which may be the separate fallback file.
    merge_hooks(&mut native.value, exe, false, state)?;
    let server = config.servers_mut().entry(AGENT_NAME).or_insert_with(|| json!({}));
    server["type"] = json!("stdio");
    server["command"] = json!(exe);
    server["args"] = json!(["mcp"]);
    if server.get("env").is_none() {
        server["env"] = json!({});
    }
    server["env"][OWNER] = json!(MARKER);
    server["env"]["REASON_NETWORK_AGENT"] = json!("zcode");
    if let Some(state) = state {
        server["env"]["REASON_NETWORK_HOME"] = json!(state);
    }
    if config.native {
        config.value["hooks"] = native.value["hooks"].clone();
    } else {
        report.lines.push(changed(&native.path, native.write(dry)?));
    }
    report.lines.push(changed(&config.path, config.write(dry)?));
    report.notes.push("Start a new ZCode session to load hooks; messages arrive after tools, with check_inbox as recovery. No idle wake-up.".into());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("reason-zcode-{} 空格", ulid::Ulid::new())))
        }
        fn put(&self, path: &str, text: &str) -> PathBuf {
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        }
        fn apply(&self, remove: bool, dry: bool) -> Result<Report> {
            apply_at(
                &self.0,
                None,
                &self.0.join("程序 空格/reason-network.exe"),
                remove,
                dry,
                Some(r"C:\测试 用户\Reason state"),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn bytes(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }

    #[test]
    fn detects_desktop_state_not_generic_agents_config() {
        let f = Fixture::new();
        assert!(!detected(&f.0));
        f.put(".agents/mcp.json", "{}");
        assert!(!detected(&f.0));
        f.put(".zcode/v2/state.json", "{}");
        assert!(detected(&f.0));
        let other = Fixture::new();
        other.put(".zcode/cli/config.json", "{}");
        assert!(detected(&other.0));
    }

    #[test]
    fn installs_preserves_settings_backs_up_and_is_idempotent() {
        let f = Fixture::new();
        let original = r#"{"provider":{"name":"ClinePass","model":"DeepSeek","unknown":[1,true]},"mcp":{"other":7,"servers":{"existing":{"command":"other","args":["a b"]}}}}"#;
        let path = f.put(".zcode/cli/config.json", original);
        f.apply(false, false).unwrap();
        let once = bytes(&path);
        let config = ConfigFile::read(path.clone(), true).unwrap();
        assert_eq!(config.value["provider"], serde_json::from_str::<Value>(original).unwrap()["provider"]);
        assert_eq!(config.value["mcp"]["other"], 7);
        assert_eq!(config.value["mcp"]["servers"]["existing"]["args"], json!(["a b"]));
        let server = config.entry().unwrap();
        assert_eq!(server["command"], json!(f.0.join("程序 空格/reason-network.exe")));
        assert_eq!(server["args"], json!(["mcp"]));
        assert_eq!(server["env"]["REASON_NETWORK_HOME"], r"C:\测试 用户\Reason state");
        assert_eq!(bytes(&PathBuf::from(format!("{}.bak-reason", path.display()))), original.as_bytes());
        f.apply(false, false).unwrap();
        assert_eq!(bytes(&path), once);
        f.apply(true, false).unwrap();
        let removed = ConfigFile::read(path, true).unwrap();
        let original: Value = serde_json::from_str(original).unwrap();
        assert_eq!(removed.value["provider"], original["provider"]);
        assert_eq!(removed.value["mcp"], original["mcp"]);
        assert!(removed.value["hooks"]["events"].as_object().unwrap().is_empty());
    }

    #[test]
    fn keeps_fallback_active_without_touching_native_provider() {
        let f = Fixture::new();
        let native = f.put(".zcode/cli/config.json", r#"{"provider":"DeepSeek","mcp":{"servers":{}}}"#);
        let original = ConfigFile::read(native.clone(), true).unwrap().value;
        let fallback = f.put(".agents/mcp.json", r#"{"unknown":5,"mcpServers":{"existing":{"command":"keep"}}}"#);
        f.apply(false, false).unwrap();
        let after = ConfigFile::read(native, true).unwrap();
        assert_eq!(after.value["provider"], original["provider"]);
        assert_eq!(after.value["mcp"], original["mcp"]);
        assert_eq!(after.value["hooks"]["events"]["SessionStart"].as_array().unwrap().len(), 1);
        let config = active(&f.0, false).unwrap();
        assert_eq!(config.path, fallback);
        assert!(config.entry().is_some());
        assert_eq!(config.value["mcpServers"]["existing"]["command"], "keep");
        f.apply(true, false).unwrap();
        assert!(active(&f.0, false).unwrap().entry().is_none());
    }

    #[test]
    fn native_servers_win_without_reading_or_changing_ignored_fallback() {
        let f = Fixture::new();
        f.put(".zcode/cli/config.json", r#"{"mcp":{"servers":{"native":{"command":"keep"}}}}"#);
        let fallback = f.put(".agents/mcp.json", "ignored invalid JSON");
        f.apply(false, false).unwrap();
        assert!(active(&f.0, false).unwrap().native);
        assert_eq!(bytes(&fallback), b"ignored invalid JSON");
    }

    #[test]
    fn protects_unmanaged_same_name_even_when_command_matches() {
        let f = Fixture::new();
        let own = json!({"mcp":{"servers":{AGENT_NAME:{"type":"stdio","command":f.0.join("程序 空格/reason-network.exe"),"args":["mcp"]}}}});
        let path = f.put(".zcode/cli/config.json", &own.to_string());
        let before = bytes(&path);
        assert!(f.apply(false, false).is_err());
        f.apply(true, false).unwrap();
        assert_eq!(bytes(&path), before);
        assert!(!PathBuf::from(format!("{}.bak-reason", path.display())).exists());
    }

    #[test]
    fn does_not_mask_foreign_workspace_server_in_either_source() {
        for name in [".zcode/config.json", ".agents/mcp.json"] {
            let f = Fixture::new();
            let project = Fixture::new();
            project.put(
                name,
                if name.contains(".zcode") {
                    r#"{"mcp":{"servers":{"reason-network":{"command":"project-owned"}}}}"#
                } else {
                    r#"{"mcpServers":{"reason-network":{"command":"project-owned"}}}"#
                },
            );
            assert!(apply_at(&f.0, Some(&project.0), Path::new("reason-network"), false, false, None).is_err());
            assert!(!f.0.exists());
        }
    }

    #[test]
    fn invalid_json_or_sections_fail_without_writes() {
        for bad in ["{", "", "[]", r#"{"mcp":null}"#, r#"{"mcp":{"servers":[]}}"#] {
            let f = Fixture::new();
            let path = f.put(".zcode/cli/config.json", bad);
            assert!(f.apply(false, false).is_err());
            assert!(f.apply(true, false).is_err());
            assert_eq!(bytes(&path), bad.as_bytes());
            assert!(!PathBuf::from(format!("{}.bak-reason", path.display())).exists());
        }
        let f = Fixture::new();
        f.put(".agents/mcp.json", r#"{"mcpServers":false}"#);
        assert!(f.apply(false, false).is_err());
        assert!(!f.0.join(".zcode").exists());
    }

    #[test]
    fn dry_run_and_empty_removal_do_not_create_files() {
        let f = Fixture::new();
        f.apply(false, true).unwrap();
        f.apply(true, false).unwrap();
        assert!(!f.0.exists());
    }

    #[test]
    fn failed_write_keeps_original_and_backup_intact() {
        let f = Fixture::new();
        let path = f.put(".zcode/cli/config.json", r#"{"provider":"keep"}"#);
        let before = bytes(&path);
        // An unwritable staging path makes the shared atomic writer fail before replacing the config.
        std::fs::create_dir(PathBuf::from(format!("{}.tmp-reason", path.display()))).unwrap();
        assert!(f.apply(false, false).is_err());
        assert_eq!(bytes(&path), before);
        assert_eq!(bytes(&PathBuf::from(format!("{}.bak-reason", path.display()))), before);
    }

    #[test]
    fn removal_finds_hidden_managed_entry_and_preserves_foreign_one() {
        let f = Fixture::new();
        f.put(".agents/mcp.json", r#"{"mcpServers":{"other":{"command":"keep"}}}"#);
        f.apply(false, false).unwrap();
        let native = f.put(".zcode/cli/config.json", r#"{"mcp":{"servers":{"reason-network":{"command":"foreign"}}}}"#);
        let before = bytes(&native);
        f.apply(true, false).unwrap();
        assert_eq!(bytes(&native), before);
        assert!(ConfigFile::read(f.0.join(".agents/mcp.json"), false).unwrap().entry().is_none());
    }

    #[test]
    fn customized_managed_entry_is_preserved_and_foreign_command_is_not_removed() {
        let f = Fixture::new();
        f.apply(false, false).unwrap();
        let mut config = active(&f.0, false).unwrap();
        let entry = config.servers_mut().get_mut(AGENT_NAME).unwrap();
        entry["enable"] = json!(false);
        entry["timeout"] = json!(90);
        entry["env"]["CUSTOM"] = json!("preserve");
        config.write(false).unwrap();
        f.apply(false, false).unwrap();
        let mut config = active(&f.0, false).unwrap();
        assert_eq!(config.entry().unwrap()["enable"], false);
        assert_eq!(config.entry().unwrap()["timeout"], 90);
        assert_eq!(config.entry().unwrap()["env"]["CUSTOM"], "preserve");
        config.servers_mut().get_mut(AGENT_NAME).unwrap()["command"] = json!("user-replacement");
        config.write(false).unwrap();
        let before = bytes(&config.path);
        assert!(f.apply(false, false).is_err());
        f.apply(true, false).unwrap();
        let after = ConfigFile::read(config.path, true).unwrap();
        let before: Value = serde_json::from_slice(&before).unwrap();
        assert_eq!(after.value["mcp"], before["mcp"]);
        assert!(after.value["hooks"]["events"].as_object().unwrap().is_empty());
    }

    #[test]
    fn hooks_merge_idempotently_and_remove_only_our_executable() {
        let exe = Path::new(r"C:\程序 空格\reason-network.exe");
        let user = json!({"type":"process","command":"user-hook","args":["keep"]});
        let mut value = json!({"provider":"keep", "mcp":{"servers":{"other":{"command":"keep"}}},
            "hooks":{"enabled":true,"timeoutMs":7000,"events":{"PostToolUse":[{"matcher":"Read","hooks":[user.clone()]}]}}});
        let original = value.clone();
        merge_hooks(&mut value, exe, false, Some(r"C:\Reason 空格")).unwrap();
        let once = value.clone();
        merge_hooks(&mut value, exe, false, Some(r"C:\Reason 空格")).unwrap();
        assert_eq!(value, once);
        let h = &value["hooks"]["events"]["SessionStart"][0]["hooks"][0];
        assert_eq!(h["type"], "process");
        assert_eq!(h["command"], json!(exe));
        assert_eq!(h["args"], json!(["hook", "zcode", "session-start", "--network-home", r"C:\Reason 空格"]));
        assert_eq!(h["timeoutMs"], 3000);
        merge_hooks(&mut value, Path::new("other-exe"), true, None).unwrap();
        assert_eq!(value, once);
        merge_hooks(&mut value, exe, true, None).unwrap();
        assert_eq!(value, original);
    }

    #[test]
    fn disabled_user_hooks_and_malformed_hooks_are_not_overwritten() {
        for hooks in [
            json!({"enabled":false,"events":{"Stop":[{"hooks":[{"type":"process","command":"keep"}]}]}}),
            json!({"enabled":"bad"}),
            json!({"events":[]}),
            json!({"enabled":true,"events":{"PostToolUse":[{}]}}),
        ] {
            let f = Fixture::new();
            let path = f.put(".zcode/cli/config.json", &json!({"provider":"keep","hooks":hooks}).to_string());
            let before = bytes(&path);
            assert!(f.apply(false, false).is_err());
            assert_eq!(bytes(&path), before);
            assert!(!PathBuf::from(format!("{}.bak-reason", path.display())).exists());
        }
    }

    #[test]
    fn removing_unmanaged_hooks_is_a_noop_even_with_empty_user_groups() {
        for hooks in
            [json!({"enabled":false}), json!({"enabled":true,"events":{"Stop":[],"PostToolUse":[{"hooks":[]}]}})]
        {
            let mut value = json!({"provider":"keep","hooks":hooks});
            let before = value.clone();
            merge_hooks(&mut value, Path::new("reason-network"), true, None).unwrap();
            assert_eq!(value, before);
        }
    }
}
