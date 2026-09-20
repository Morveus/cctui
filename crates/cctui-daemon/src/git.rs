//! Git facts for a directory, read from `.git` metadata.
//!
//! No `git` subprocess except the opt-in `dirty` check. Serves the spawn
//! dialog's branch badge and the `claude_code` adapter's `git_branch` /
//! `git_remote` meta.

use std::path::{Path, PathBuf};
use std::time::Duration;

use cctui_proto::git::GitInfo;

/// Upper bound for the opt-in `git status` subprocess.
const DIRTY_TIMEOUT: Duration = Duration::from_secs(3);

/// Current branch name of `cwd`, or `None` when detached / not a repo.
#[must_use]
pub fn read_git_branch(cwd: &str) -> Option<String> {
    read_head(&PathBuf::from(cwd))
        .and_then(|(head, _)| head.trim().strip_prefix("ref: refs/heads/").map(str::to_owned))
}

/// `origin` URL of `cwd`, `None` unless it is a GitHub remote. Consumers
/// treat `None` as "no chip", so a non-GitHub origin must not leak through.
#[must_use]
pub fn read_git_remote(cwd: &str) -> Option<String> {
    let (git_dir, _) = git_dir(&PathBuf::from(cwd))?;
    let config = std::fs::read_to_string(common_dir(&git_dir).join("config")).ok()?;
    let url = origin_url(&config)?;
    is_github_remote(&url).then_some(url)
}

/// Expand a leading `~` / `~/…` to `$HOME`.
#[must_use]
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

/// Canonicalize `path` and require it to sit under one of `roots`.
///
/// Roots are canonicalized too, so symlinked homes match. Missing paths and
/// paths outside every root are errors.
pub fn resolve_allowed(path: &Path, roots: &[PathBuf]) -> anyhow::Result<PathBuf> {
    let real = std::fs::canonicalize(path)
        .map_err(|err| anyhow::anyhow!("cannot resolve {}: {err}", path.display()))?;
    let allowed = roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .any(|root| real.starts_with(&root));
    if !allowed {
        anyhow::bail!("{} is outside the allowed roots", path.display());
    }
    Ok(real)
}

/// Default allowed roots: `$HOME` only.
#[must_use]
pub fn default_roots() -> Vec<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).into_iter().collect()
}

/// `(HEAD contents, is_worktree)` for `dir`, or `None` when `dir` is not a
/// repo. A `.git` *file* (`gitdir: <path>`) marks a linked worktree; its
/// HEAD lives in the pointed-to directory (relative to `dir` when relative).
fn read_head(dir: &Path) -> Option<(String, bool)> {
    let (git_dir, worktree) = git_dir(dir)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    Some((head, worktree))
}

fn git_dir(dir: &Path) -> Option<(PathBuf, bool)> {
    let dot_git = dir.join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some((dot_git, false));
    }
    let pointer = std::fs::read_to_string(&dot_git).ok()?;
    let target = PathBuf::from(pointer.trim().strip_prefix("gitdir:")?.trim());
    let git_dir = if target.is_absolute() { target } else { dir.join(target) };
    Some((git_dir, true))
}

/// Shared git dir holding `config`: a linked worktree's own dir points at it
/// through `commondir`; a plain repo is its own common dir.
fn common_dir(git_dir: &Path) -> PathBuf {
    let Ok(raw) = std::fs::read_to_string(git_dir.join("commondir")) else {
        return git_dir.to_path_buf();
    };
    let target = PathBuf::from(raw.trim());
    if target.as_os_str().is_empty() {
        git_dir.to_path_buf()
    } else if target.is_absolute() {
        target
    } else {
        git_dir.join(target)
    }
}

fn origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let header = rest.split(']').next().unwrap_or_default().trim();
            in_origin = header.split_once(char::is_whitespace).is_some_and(|(kind, name)| {
                kind == "remote" && name.trim().trim_matches('"') == "origin"
            });
            continue;
        }
        if !in_origin {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("url")
            && !value.trim().is_empty()
        {
            return Some(value.trim().to_owned());
        }
    }
    None
}

const GITHUB_HOSTS: [&str; 3] = ["github.com", "www.github.com", "ssh.github.com"];

/// Mirrors `webui/src/lib/gitRemote.ts`: host must be GitHub and the path
/// must carry an `owner/repo` pair, or the webui parser rejects it anyway.
fn is_github_remote(url: &str) -> bool {
    let raw = url.trim();
    let (authority, path) = if let Some((scheme, rest)) = raw.split_once("://") {
        if !matches!(scheme, "http" | "https" | "ssh" | "git") {
            return false;
        }
        rest.split_once('/').unwrap_or((rest, ""))
    } else {
        match raw.split_once(':') {
            Some((_, path)) if path.starts_with('/') => return false,
            Some(parts) => parts,
            None => return false,
        }
    };
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    if !GITHUB_HOSTS.iter().any(|known| host.eq_ignore_ascii_case(known)) {
        return false;
    }
    path.split('/').filter(|part| !part.is_empty()).count() >= 2
}

/// Git facts for `path` (already tilde-expanded and root-checked by the
/// caller). Not a repo → `GitInfo { is_repo: false, .. }`.
#[must_use]
pub fn git_info(path: &Path) -> GitInfo {
    let Some((head, is_worktree)) = read_head(path) else {
        return GitInfo::default();
    };
    let head = head.trim();
    let branch = head.strip_prefix("ref: refs/heads/").map(str::to_owned);
    let detached_sha = if branch.is_none() && !head.starts_with("ref:") && !head.is_empty() {
        Some(head.to_owned())
    } else {
        None
    };
    GitInfo { is_repo: true, branch, detached_sha, dirty: None, is_worktree }
}

/// `git status --porcelain` non-empty ⇒ dirty. `None` when git is missing,
/// fails, or exceeds [`DIRTY_TIMEOUT`].
pub async fn is_dirty(path: &Path) -> Option<bool> {
    let run = tokio::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .current_dir(path)
        .stdin(std::process::Stdio::null())
        .output();
    let out = tokio::time::timeout(DIRTY_TIMEOUT, run).await.ok()?.ok()?;
    out.status.success().then(|| !out.stdout.iter().all(u8::is_ascii_whitespace))
}

/// Full resolution for a `GitInfo` request: expand `~`, enforce `roots`,
/// read `.git`, optionally run the dirty check.
pub async fn resolve_git_info(
    path: &str,
    roots: &[PathBuf],
    include_dirty: bool,
) -> anyhow::Result<GitInfo> {
    let real = resolve_allowed(&expand_tilde(path), roots)?;
    let mut info = git_info(&real);
    if include_dirty && info.is_repo {
        info.dirty = is_dirty(&real).await;
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with_head(head: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let git = tmp.path().join(".git");
        std::fs::create_dir(&git).unwrap();
        std::fs::write(git.join("HEAD"), head).unwrap();
        tmp
    }

    #[test]
    fn read_git_branch_reads_head_ref_and_ignores_detached() {
        let tmp = repo_with_head("ref: refs/heads/wave/1-ingest\n");
        assert_eq!(read_git_branch(tmp.path().to_str().unwrap()).as_deref(), Some("wave/1-ingest"));
        std::fs::write(tmp.path().join(".git/HEAD"), "0123456789abcdef0123456789abcdef01234567\n")
            .unwrap();
        assert_eq!(read_git_branch(tmp.path().to_str().unwrap()), None);
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(read_git_branch(bare.path().to_str().unwrap()), None);
    }

    fn write_config(dir: &Path, body: &str) {
        std::fs::write(dir.join("config"), body).unwrap();
    }

    #[test]
    fn read_git_remote_reads_origin_from_config() {
        let tmp = repo_with_head("ref: refs/heads/main\n");
        let cwd = tmp.path().to_str().unwrap();
        assert_eq!(read_git_remote(cwd), None);

        write_config(
            &tmp.path().join(".git"),
            "[core]\n\turl = nope\n[remote \"upstream\"]\n\turl = git@github.com:other/up.git\n\
             [remote \"origin\"]\n\turl = git@github.com:DorskFR/cctui.git\n\tfetch = +refs/*\n",
        );
        assert_eq!(read_git_remote(cwd).as_deref(), Some("git@github.com:DorskFR/cctui.git"));
    }

    #[test]
    fn read_git_remote_absent_for_non_github_and_missing_origin() {
        let tmp = repo_with_head("ref: refs/heads/main\n");
        let cwd = tmp.path().to_str().unwrap();
        let git = tmp.path().join(".git");

        write_config(&git, "[remote \"upstream\"]\n\turl = git@github.com:o/r.git\n");
        assert_eq!(read_git_remote(cwd), None);

        write_config(&git, "[remote \"origin\"]\n\turl = git@gitea.dorsk.dev:o/r.git\n");
        assert_eq!(read_git_remote(cwd), None);

        write_config(&git, "[remote \"origin\"]\n\turl =\n");
        assert_eq!(read_git_remote(cwd), None);

        let plain = tempfile::tempdir().unwrap();
        assert_eq!(read_git_remote(plain.path().to_str().unwrap()), None);
    }

    #[test]
    fn read_git_remote_uses_the_worktree_common_dir() {
        let main = repo_with_head("ref: refs/heads/main\n");
        let main_git = main.path().join(".git");
        write_config(&main_git, "[remote \"origin\"]\n\turl = https://github.com/DorskFR/cctui\n");
        let wt_git = main_git.join("worktrees/wt");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::write(wt_git.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        let wt = tempfile::tempdir().unwrap();
        std::fs::write(wt.path().join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();

        assert_eq!(
            read_git_remote(wt.path().to_str().unwrap()).as_deref(),
            Some("https://github.com/DorskFR/cctui")
        );
    }

    #[test]
    fn is_github_remote_accepts_github_forms_only() {
        for url in [
            "git@github.com:DorskFR/cctui.git",
            "https://github.com/DorskFR/cctui",
            "https://github.com/DorskFR/cctui.git",
            "ssh://git@ssh.github.com:443/DorskFR/cctui.git",
            "git://github.com/DorskFR/cctui.git",
            "https://www.github.com/DorskFR/cctui",
        ] {
            assert!(is_github_remote(url), "{url}");
        }
        for url in [
            "git@gitlab.com:DorskFR/cctui.git",
            "https://github.example.com/DorskFR/cctui",
            "https://github.com/DorskFR",
            "/srv/git/cctui.git",
            "file:///srv/git/cctui.git",
            "",
        ] {
            assert!(!is_github_remote(url), "{url}");
        }
    }

    #[test]
    fn git_info_branch_detached_and_not_a_repo() {
        let tmp = repo_with_head("ref: refs/heads/main\n");
        let info = git_info(tmp.path());
        assert_eq!(info.branch.as_deref(), Some("main"));
        assert!(info.is_repo && !info.is_worktree && info.detached_sha.is_none());

        std::fs::write(tmp.path().join(".git/HEAD"), "0123456789abcdef0123456789abcdef01234567\n")
            .unwrap();
        let info = git_info(tmp.path());
        assert_eq!(info.branch, None);
        assert_eq!(info.detached_sha.as_deref(), Some("0123456789abcdef0123456789abcdef01234567"));

        let plain = tempfile::tempdir().unwrap();
        assert_eq!(git_info(plain.path()), GitInfo::default());
    }

    #[test]
    fn git_info_follows_worktree_gitdir_pointer() {
        let main = repo_with_head("ref: refs/heads/main\n");
        let wt_git = main.path().join(".git/worktrees/wt");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::write(wt_git.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        let wt = tempfile::tempdir().unwrap();
        std::fs::write(wt.path().join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        let info = git_info(wt.path());
        assert_eq!(info.branch.as_deref(), Some("feature"));
        assert!(info.is_worktree);

        // Relative pointer resolves against the worktree dir.
        std::fs::create_dir_all(wt.path().join("meta")).unwrap();
        std::fs::write(wt.path().join("meta/HEAD"), "ref: refs/heads/rel\n").unwrap();
        std::fs::write(wt.path().join(".git"), "gitdir: meta\n").unwrap();
        assert_eq!(git_info(wt.path()).branch.as_deref(), Some("rel"));
    }

    #[test]
    fn resolve_allowed_enforces_roots() {
        let root = tempfile::tempdir().unwrap();
        let inside = root.path().join("child");
        std::fs::create_dir(&inside).unwrap();
        let roots = vec![root.path().to_path_buf()];
        assert!(resolve_allowed(&inside, &roots).is_ok());
        let outside = tempfile::tempdir().unwrap();
        assert!(resolve_allowed(outside.path(), &roots).is_err());
        // `..` cannot escape: canonicalize resolves it before the check.
        assert!(resolve_allowed(&inside.join(".."), std::slice::from_ref(&inside)).is_err());
        assert!(resolve_allowed(&root.path().join("missing"), &roots).is_err());
    }

    #[tokio::test]
    async fn resolve_git_info_reports_outside_paths_as_errors() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let roots = vec![root.path().to_path_buf()];
        assert!(resolve_git_info(other.path().to_str().unwrap(), &roots, false).await.is_err());
        let info = resolve_git_info(root.path().to_str().unwrap(), &roots, false).await.unwrap();
        assert!(!info.is_repo);
    }
}
