//! Install and remove the hook script and its entries in Claude Code settings.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::paths::Paths;

pub const HOOK_SCRIPT: &str = include_str!("../hook/diffier.sh");
pub const HOOK_MARKER: &str = "diffier.sh";
/// Anchored so an MCP tool whose name merely contains "Edit" does not match.
pub const TOOL_MATCHER: &str = "^(Edit|Write|MultiEdit|NotebookEdit)$";
const HOOK_TIMEOUT_SECS: u64 = 5;

/// (event name, matcher) pairs the monitor registers.
const HOOK_EVENTS: [(&str, Option<&str>); 3] = [
    ("PreToolUse", Some(TOOL_MATCHER)),
    ("PostToolUse", Some(TOOL_MATCHER)),
    ("SessionStart", None),
];

fn group_is_ours(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .map(|c| c.contains(HOOK_MARKER))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Wrap a path in single quotes for the shell Claude Code runs hooks in, so a
/// home directory with a space or a metacharacter still resolves.
pub fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

fn our_group(matcher: Option<&str>, script: &str) -> Value {
    let mut g = Map::new();
    if let Some(m) = matcher {
        g.insert("matcher".into(), json!(m));
    }
    g.insert(
        "hooks".into(),
        json!([{ "type": "command", "command": shell_quote(script), "timeout": HOOK_TIMEOUT_SECS }]),
    );
    Value::Object(g)
}

/// Add or refresh our hook groups. Everything else in `settings` is preserved.
pub fn merge_hooks(settings: &mut Value, script: &str) {
    if !settings.is_object() {
        *settings = json!({});
    }
    let root = settings.as_object_mut().expect("object");
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().expect("object");
    for (event, matcher) in HOOK_EVENTS {
        let list = hooks.entry(event).or_insert_with(|| json!([]));
        if !list.is_array() {
            *list = json!([]);
        }
        let arr = list.as_array_mut().expect("array");
        arr.retain(|g| !group_is_ours(g));
        arr.push(our_group(matcher, script));
    }
}

/// Remove our hook groups, dropping event arrays and `hooks` when emptied.
pub fn remove_hooks(settings: &mut Value) {
    let Some(root) = settings.as_object_mut() else {
        return;
    };
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        if let Some(arr) = hooks.get_mut(&event).and_then(Value::as_array_mut) {
            arr.retain(|g| !group_is_ours(g));
            if arr.is_empty() {
                hooks.remove(&event);
            }
        }
    }
    if hooks.is_empty() {
        root.remove("hooks");
    }
}

fn load_settings(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(json!({})),
        Ok(s) => serde_json::from_str(&s)
            .with_context(|| format!("{} is not valid JSON; not touching it", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_settings(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bak = path.with_extension("json.bak");
    if path.exists() && !bak.exists() {
        fs::copy(path, &bak).with_context(|| format!("backing up to {}", bak.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

pub fn install(paths: &Paths) -> Result<()> {
    let script = &paths.hook_script;
    if let Some(dir) = script.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(script, HOOK_SCRIPT).with_context(|| format!("writing {}", script.display()))?;
    fs::set_permissions(script, fs::Permissions::from_mode(0o755))?;
    println!("wrote hook script  {}", script.display());

    let mut settings = load_settings(&paths.settings)?;
    let before = settings.clone();
    merge_hooks(&mut settings, &script.to_string_lossy());
    if settings == before {
        println!("settings unchanged  {}", paths.settings.display());
    } else {
        write_settings(&paths.settings, &settings)?;
        for (event, _) in HOOK_EVENTS {
            println!("registered {event:<12} {}", paths.settings.display());
        }
    }
    println!("spool              {}", paths.spool.display());
    println!("snapshots          {}", paths.snapshot_root.display());
    println!("Restart running Claude Code sessions so the hooks load.");
    Ok(())
}

pub fn uninstall(paths: &Paths, purge: bool) -> Result<()> {
    if paths.settings.exists() {
        let mut settings = load_settings(&paths.settings)?;
        let before = settings.clone();
        remove_hooks(&mut settings);
        if settings == before {
            println!("no hook entries in {}", paths.settings.display());
        } else {
            write_settings(&paths.settings, &settings)?;
            println!("removed hook entries from {}", paths.settings.display());
        }
    }
    match fs::remove_file(&paths.hook_script) {
        Ok(()) => println!("removed {}", paths.hook_script.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => bail!("removing {}: {e}", paths.hook_script.display()),
    }
    if purge {
        if let Some(dir) = paths.spool.parent() {
            let _ = fs::remove_dir_all(dir);
            println!("removed {}", dir.display());
        }
        let _ = fs::remove_dir_all(&paths.snapshot_root);
        println!("removed {}", paths.snapshot_root.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = "/home/u/.claude/hooks/diffier.sh";

    #[test]
    fn merge_into_empty() {
        let mut s = json!({});
        merge_hooks(&mut s, SCRIPT);
        let pre = &s["hooks"]["PreToolUse"];
        assert_eq!(pre.as_array().unwrap().len(), 1);
        assert_eq!(pre[0]["matcher"], TOOL_MATCHER);
        assert_eq!(pre[0]["hooks"][0]["command"], shell_quote(SCRIPT));
        assert_eq!(pre[0]["hooks"][0]["timeout"], 5);
        assert!(s["hooks"]["SessionStart"][0].get("matcher").is_none());
    }

    #[test]
    fn merge_preserves_unrelated_and_is_idempotent() {
        let original = json!({
            "model": "x",
            "statusLine": {"type": "command", "command": "foo"},
            "hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "other.sh"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "bye.sh"}]}]
            }
        });
        let mut s = original.clone();
        merge_hooks(&mut s, SCRIPT);
        assert_eq!(s["model"], "x");
        assert_eq!(s["statusLine"]["command"], "foo");
        assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 1);
        let once = serde_json::to_string_pretty(&s).unwrap();
        merge_hooks(&mut s, SCRIPT);
        assert_eq!(serde_json::to_string_pretty(&s).unwrap(), once);
        // Upgrading the script path replaces rather than appends.
        merge_hooks(&mut s, "/new/diffier.sh");
        assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(
            s["hooks"]["PreToolUse"][1]["hooks"][0]["command"],
            "'/new/diffier.sh'"
        );
        // Uninstall restores the original.
        remove_hooks(&mut s);
        assert_eq!(s, original);
    }

    #[test]
    fn command_is_shell_quoted_and_still_recognized() {
        let awkward = "/Users/John Smith/o'brien/.claude/hooks/diffier.sh";
        assert_eq!(
            shell_quote(awkward),
            "'/Users/John Smith/o'\\''brien/.claude/hooks/diffier.sh'"
        );
        let mut s = json!({});
        merge_hooks(&mut s, awkward);
        let group = &s["hooks"]["PreToolUse"][0];
        assert!(group_is_ours(group));
        // Idempotent: the quoted command is still matched and replaced, not appended.
        merge_hooks(&mut s, awkward);
        assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        remove_hooks(&mut s);
        assert_eq!(s, json!({}));
    }

    #[test]
    fn remove_drops_empty_containers() {
        let mut s = json!({"a": 1});
        merge_hooks(&mut s, SCRIPT);
        remove_hooks(&mut s);
        assert_eq!(s, json!({"a": 1}));
        remove_hooks(&mut s);
        assert_eq!(s, json!({"a": 1}));
    }

    #[test]
    fn install_and_uninstall_roundtrip_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            spool: dir.path().join("state/events.jsonl"),
            snapshot_root: dir.path().join("cache"),
            hook_script: dir.path().join("claude/hooks/diffier.sh"),
            settings: dir.path().join("claude/settings.json"),
        };
        fs::create_dir_all(paths.settings.parent().unwrap()).unwrap();
        fs::write(&paths.settings, "{\n  \"theme\": \"dark\"\n}\n").unwrap();
        install(&paths).unwrap();
        assert!(paths.hook_script.exists());
        assert_eq!(
            fs::metadata(&paths.hook_script)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        let v: Value = serde_json::from_str(&fs::read_to_string(&paths.settings).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        assert!(v["hooks"]["PostToolUse"].is_array());
        assert!(paths.settings.with_extension("json.bak").exists());
        let after_first = fs::read_to_string(&paths.settings).unwrap();
        install(&paths).unwrap();
        assert_eq!(fs::read_to_string(&paths.settings).unwrap(), after_first);
        uninstall(&paths, false).unwrap();
        assert!(!paths.hook_script.exists());
        let v: Value = serde_json::from_str(&fs::read_to_string(&paths.settings).unwrap()).unwrap();
        assert_eq!(v, json!({"theme": "dark"}));
    }

    #[test]
    fn invalid_settings_abort() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        fs::write(&p, "{ nope").unwrap();
        assert!(load_settings(&p).is_err());
        assert_eq!(fs::read_to_string(&p).unwrap(), "{ nope");
    }
}
