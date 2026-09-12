//! Filesystem locations shared between `diffier hook` and the monitor.
//!
//! Derived from the XDG environment variables with their spec defaults rather
//! than from platform conventions (macOS `dirs::cache_dir` would be
//! `~/Library/Caches`), so the paths stay stable and predictable.

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Paths {
    /// JSONL spool the hook appends to.
    pub spool: PathBuf,
    /// Root of pre-edit snapshots: `<root>/<session_id>/<tool_use_id>`.
    pub snapshot_root: PathBuf,
    /// Where the pre-0.2 shell hook lived; `install` and `uninstall` delete it.
    pub hook_script: PathBuf,
    /// Claude Code user settings.
    pub settings: PathBuf,
}

impl Paths {
    pub fn discover() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self::from_parts(
            home,
            env::var_os("XDG_STATE_HOME"),
            env::var_os("XDG_CACHE_HOME"),
        )
    }

    /// An empty value counts as unset, as the XDG spec says and as the shell
    /// hook's `${VAR:-default}` did.
    pub fn from_parts(
        home: PathBuf,
        xdg_state: Option<OsString>,
        xdg_cache: Option<OsString>,
    ) -> Self {
        let non_empty = |v: Option<OsString>| v.filter(|v| !v.is_empty()).map(PathBuf::from);
        let state = non_empty(xdg_state).unwrap_or_else(|| home.join(".local").join("state"));
        let cache = non_empty(xdg_cache).unwrap_or_else(|| home.join(".cache"));
        let claude = home.join(".claude");
        Self {
            spool: state.join("diffier").join("events.jsonl"),
            snapshot_root: cache.join("diffier"),
            hook_script: claude.join("hooks").join("diffier.sh"),
            settings: claude.join("settings.json"),
        }
    }

    /// Previous spool file, written by the hook when it rotates at 50 MB.
    pub fn rotated_spool(&self) -> PathBuf {
        rotated(&self.spool)
    }
}

/// `<path>.1`, appended to the file name rather than replacing an extension.
pub fn rotated(spool: &std::path::Path) -> PathBuf {
    let mut s = spool.as_os_str().to_os_string();
    s.push(".1");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_xdg_values_fall_back_to_home() {
        let home = PathBuf::from("/h");
        let p = Paths::from_parts(
            home.clone(),
            Some(OsString::from("")),
            Some(OsString::from("")),
        );
        assert_eq!(
            p.spool,
            PathBuf::from("/h/.local/state/diffier/events.jsonl")
        );
        assert_eq!(p.snapshot_root, PathBuf::from("/h/.cache/diffier"));

        let unset = Paths::from_parts(home.clone(), None, None);
        assert_eq!(unset.spool, p.spool);
        assert_eq!(unset.snapshot_root, p.snapshot_root);
    }

    #[test]
    fn set_xdg_values_are_honored() {
        let p = Paths::from_parts(
            PathBuf::from("/h"),
            Some(OsString::from("/s")),
            Some(OsString::from("/c")),
        );
        assert_eq!(p.spool, PathBuf::from("/s/diffier/events.jsonl"));
        assert_eq!(p.snapshot_root, PathBuf::from("/c/diffier"));
        assert_eq!(p.settings, PathBuf::from("/h/.claude/settings.json"));
    }

    #[test]
    fn rotated_appends_suffix() {
        let p = Paths::from_parts(PathBuf::from("/h"), None, None);
        assert_eq!(
            p.rotated_spool(),
            PathBuf::from("/h/.local/state/diffier/events.jsonl.1")
        );
    }
}
