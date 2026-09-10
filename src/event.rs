//! Tolerant model of a Claude Code hook payload as written to the spool.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

pub const FILE_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

#[derive(Debug, Clone, Deserialize)]
pub struct HookEvent {
    pub hook_event_name: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub tool_input: Option<Value>,
    #[serde(default)]
    pub tool_response: Option<Value>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
    /// SessionStart only: startup | resume | clear | compact | fork.
    #[serde(default)]
    pub source: Option<String>,
    /// Milliseconds since the epoch, added by the hook. jq may emit a float.
    #[serde(default)]
    pub ts: Option<f64>,
}

impl HookEvent {
    pub fn parse_line(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        serde_json::from_str(line).ok()
    }

    pub fn is_pre(&self) -> bool {
        self.hook_event_name == "PreToolUse"
    }

    pub fn is_post(&self) -> bool {
        self.hook_event_name == "PostToolUse"
    }

    pub fn is_session_start(&self) -> bool {
        self.hook_event_name == "SessionStart"
    }

    pub fn is_file_tool(&self) -> bool {
        self.tool_name
            .as_deref()
            .map(|t| FILE_TOOLS.contains(&t))
            .unwrap_or(false)
    }

    pub fn ts_ms(&self) -> u64 {
        self.ts.map(|t| t.max(0.0) as u64).unwrap_or(0)
    }

    /// Target file: `tool_input.file_path`, or `notebook_path` for NotebookEdit.
    pub fn file_path(&self) -> Option<PathBuf> {
        let input = self.tool_input.as_ref()?;
        input
            .get("file_path")
            .or_else(|| input.get("notebook_path"))
            .and_then(Value::as_str)
            .map(PathBuf::from)
    }

    /// Subagent label when the tool ran inside an Agent call.
    pub fn agent_label(&self) -> Option<String> {
        self.agent_type
            .clone()
            .or_else(|| self.agent_id.as_ref().map(|_| "subagent".to_string()))
    }

    pub fn input_str(&self, key: &str) -> Option<&str> {
        self.tool_input.as_ref()?.get(key)?.as_str()
    }

    pub fn response_str(&self, key: &str) -> Option<&str> {
        self.tool_response.as_ref()?.get(key)?.as_str()
    }

    pub fn response_bool(&self, key: &str) -> Option<bool> {
        self.tool_response.as_ref()?.get(key)?.as_bool()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_edit_pre() {
        let ev = HookEvent::parse_line(
            r#"{"session_id":"s1","cwd":"/p","hook_event_name":"PreToolUse","tool_name":"Edit","tool_use_id":"t1","tool_input":{"file_path":"/p/a.rs","old_string":"x","new_string":"y","replace_all":false},"ts":1700000000000}"#,
        )
        .unwrap();
        assert!(ev.is_pre());
        assert!(ev.is_file_tool());
        assert_eq!(ev.file_path().unwrap(), PathBuf::from("/p/a.rs"));
        assert_eq!(ev.ts_ms(), 1_700_000_000_000);
        assert_eq!(ev.input_str("old_string"), Some("x"));
    }

    #[test]
    fn parses_write_post_with_response() {
        let ev = HookEvent::parse_line(
            r#"{"hook_event_name":"PostToolUse","tool_name":"Write","tool_use_id":"t2","tool_input":{"file_path":"/p/b.txt","content":"hi"},"tool_response":{"type":"create","filePath":"/p/b.txt","originalFile":"","userModified":false}}"#,
        )
        .unwrap();
        assert!(ev.is_post());
        assert_eq!(ev.response_str("originalFile"), Some(""));
        assert_eq!(ev.response_bool("userModified"), Some(false));
        assert_eq!(ev.ts_ms(), 0);
    }

    #[test]
    fn parses_multiedit_and_notebook() {
        let m = HookEvent::parse_line(
            r#"{"hook_event_name":"PreToolUse","tool_name":"MultiEdit","tool_input":{"file_path":"/p/c.rs","edits":[{"old_string":"a","new_string":"b"}]}}"#,
        )
        .unwrap();
        assert!(m.is_file_tool());
        let n = HookEvent::parse_line(
            r#"{"hook_event_name":"PreToolUse","tool_name":"NotebookEdit","tool_input":{"notebook_path":"/p/n.ipynb","cell_id":"1","new_source":"x"}}"#,
        )
        .unwrap();
        assert_eq!(n.file_path().unwrap(), PathBuf::from("/p/n.ipynb"));
    }

    #[test]
    fn subagent_fields_and_float_ts() {
        let ev = HookEvent::parse_line(
            r#"{"hook_event_name":"PostToolUse","tool_name":"Edit","agent_id":"a1","agent_type":"Explore","ts":1.7e12}"#,
        )
        .unwrap();
        assert_eq!(ev.agent_label().as_deref(), Some("Explore"));
        assert_eq!(ev.ts_ms(), 1_700_000_000_000);
        let only_id =
            HookEvent::parse_line(r#"{"hook_event_name":"PostToolUse","agent_id":"a1"}"#).unwrap();
        assert_eq!(only_id.agent_label().as_deref(), Some("subagent"));
    }

    #[test]
    fn malformed_and_empty_lines_are_none() {
        assert!(HookEvent::parse_line("").is_none());
        assert!(HookEvent::parse_line("{not json").is_none());
        assert!(HookEvent::parse_line(r#"{"no_event_name":1}"#).is_none());
    }
}
