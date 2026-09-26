//! Claude Code's own per-process session registry (`~/.claude/sessions/*.json`).
//!
//! The CLI writes one file per live session and consults it — not the daemon —
//! when it decides whether a job's worktree is occupied. cctui reads it to find
//! a worker the connected `claude daemon` disowns: `kill` answers `ENOJOB` and
//! `has` answers `alive:false` for a job hosted by nobody, yet the process is
//! still running and `claude rm` then refuses.

use std::path::{Path, PathBuf};

/// One registry entry. Every field is optional: shapes differ between CLI
/// versions and an interactive session records neither `jobId` nor `cwd`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    #[serde(default)]
    pub pid: Option<i32>,
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub proc_start: Option<String>,
}

impl Entry {
    /// The pid, taken from the entry or from the `<pid>.json` file name.
    fn pid_of(&self, path: &Path) -> Option<i32> {
        self.pid.or_else(|| path.file_stem()?.to_str()?.parse().ok())
    }
}

/// `~/.claude/sessions`, or `$CLAUDE_CONFIG_DIR/sessions` when set.
pub fn default_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("sessions"));
    }
    Some(dirs::home_dir()?.join(".claude").join("sessions"))
}

/// The pid of a live process registered for `job_id`, verified against
/// `/proc/<pid>/stat`'s start time so a recycled pid is never signalled.
///
/// `None` on any doubt — including every platform without `/proc`, where the
/// start time cannot be confirmed.
pub fn live_pid_for_job(dir: &Path, job_id: &str) -> Option<i32> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        let Ok(parsed) = serde_json::from_str::<Entry>(&raw) else { continue };
        if parsed.job_id.as_deref() != Some(job_id) {
            continue;
        }
        let Some(pid) = parsed.pid_of(&path) else { continue };
        let Some(expected) = parsed.proc_start.as_deref() else { continue };
        if proc_start_of(pid).is_some_and(|actual| actual == expected) {
            return Some(pid);
        }
    }
    None
}

/// Field 22 of `/proc/<pid>/stat` (start time in clock ticks). The `comm` field
/// can contain spaces and parentheses, so parsing resumes after its last `)`.
pub fn proc_start_of(pid: i32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19).map(str::to_owned)
}

/// SIGTERM `pid`, then poll for up to ~2s for it to disappear.
pub async fn terminate(pid: i32) -> bool {
    let Some(p) = rustix::process::Pid::from_raw(pid) else { return false };
    if rustix::process::kill_process(p, rustix::process::Signal::TERM).is_err() {
        return false;
    }
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if !is_alive(pid) {
            return true;
        }
    }
    false
}

fn is_alive(pid: i32) -> bool {
    rustix::process::Pid::from_raw(pid)
        .is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
}

#[cfg(test)]
mod tests {
    use super::{Entry, live_pid_for_job, proc_start_of};

    fn write(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn matches_the_job_and_verifies_the_start_time() {
        let tmp = tempfile::tempdir().unwrap();
        let me = std::process::id().to_string();
        let Some(start) = proc_start_of(i32::try_from(std::process::id()).unwrap()) else {
            return;
        };

        write(
            tmp.path(),
            &format!("{me}.json"),
            &format!(
                r#"{{"pid":{me},"jobId":"deadbeef","procStart":"{start}","cwd":"/w","kind":"bg"}}"#
            ),
        );
        let pid = i32::try_from(std::process::id()).unwrap();
        assert_eq!(live_pid_for_job(tmp.path(), "deadbeef"), Some(pid));

        // A different job never matches.
        assert_eq!(live_pid_for_job(tmp.path(), "cafebabe"), None);
    }

    #[test]
    fn refuses_a_recycled_pid_or_an_unverifiable_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let me = std::process::id();
        write(
            tmp.path(),
            &format!("{me}.json"),
            &format!(r#"{{"pid":{me},"jobId":"deadbeef","procStart":"999999999999"}}"#),
        );
        assert_eq!(live_pid_for_job(tmp.path(), "deadbeef"), None);

        write(tmp.path(), "1.json", r#"{"pid":1,"jobId":"c0ffee00"}"#);
        assert_eq!(live_pid_for_job(tmp.path(), "c0ffee00"), None);
    }

    #[test]
    fn tolerates_foreign_shapes_and_non_json_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "292048.key", "not json");
        write(
            tmp.path(),
            "292048.json",
            r#"{"peerToken":"8c0b","procStart":"578153231","pidDomain":"linux:x:pid:[4026531836]"}"#,
        );
        assert_eq!(live_pid_for_job(tmp.path(), "deadbeef"), None);

        let parsed: Entry = serde_json::from_str(
            r#"{"peerToken":"8c0b","procStart":"578153231","pidDomain":"linux"}"#,
        )
        .unwrap();
        assert_eq!(parsed.job_id, None);
        assert_eq!(parsed.proc_start.as_deref(), Some("578153231"));
    }
}
