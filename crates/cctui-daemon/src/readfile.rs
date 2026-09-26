//! Serve one file off this machine for the webui (`DaemonFrameDown::ReadFile`).
//!
//! The path is one an agent linked in a message, and the server has already
//! checked that grant. The allow-list here is defense in depth: the temp dirs,
//! the session's working directory, its enclosing git repo and the home of the
//! user it runs as, the Claude job dirs of that user and of the daemon's, and
//! whatever extra roots the daemon is configured with. A home is a root only
//! when a session names it — the daemon's own `$HOME` never widens an
//! unattributed read. A secret deny-list applies inside every root. The path is
//! canonicalised so a symlink pointing outside every root is refused. Small
//! files ride back inline, larger ones are PUT to the blob store and answered
//! by hash.

use std::path::{Path, PathBuf};

use base64::Engine;
use cctui_proto::media::sniff_media_type;
use cctui_proto::ws::{
    DaemonFrameUp, READ_FILE_INLINE_BYTES, READ_FILE_MAX_BYTES, ReadFileErrorKind, ReadFileOk,
};
use sha2::{Digest, Sha256};

use crate::client::ServerClient;

#[derive(Debug, PartialEq, Eq)]
pub struct Refused {
    pub kind: ReadFileErrorKind,
    pub message: String,
}

fn refused(kind: ReadFileErrorKind, message: impl Into<String>) -> Refused {
    Refused { kind, message: message.into() }
}

/// Roots for a read made on behalf of the session whose cwd is `cwd`, using
/// the daemon's configured [`extra_roots`].
///
/// Each is canonicalised so `starts_with` compares real paths.
#[must_use]
pub fn allowed_roots(cwd: Option<&str>) -> Vec<PathBuf> {
    roots_from(cwd, &extra_roots())
}

/// The roots a session may be served from.
///
/// Temp dirs, the session's working directory, its enclosing git repo and the
/// home of the user it runs as, the Claude job dir of that user and of the
/// daemon's (they differ whenever the daemon runs as another user, as in a
/// worker pod), and `extra`.
///
/// Only a session widens past the temp dirs: with `cwd` `None` the daemon's
/// own `$HOME` still contributes nothing but its job dir, so an unattributed
/// read can never walk a home.
#[must_use]
pub fn roots_from(cwd: Option<&str>, extra: &[String]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> =
        [std::env::temp_dir(), PathBuf::from("/tmp"), PathBuf::from("/private/tmp")]
            .into_iter()
            .filter_map(|r| r.canonicalize().ok())
            .collect();
    let real_cwd = cwd.and_then(|c| crate::git::expand_tilde(c).canonicalize().ok());
    let session_home = real_cwd.as_deref().and_then(home_of);
    if let Some(real) = &real_cwd {
        if let Some(root) = git_root(real) {
            roots.push(root);
        }
        roots.push(real.clone());
    }
    if let Some(home) = session_home.as_ref().and_then(|h| h.canonicalize().ok()) {
        roots.push(home);
    }
    for home in [session_home, dirs::home_dir()].into_iter().flatten() {
        if let Ok(jobs) = home.join(".claude").join("jobs").canonicalize() {
            roots.push(jobs);
        }
    }
    for root in extra {
        if let Ok(real) = crate::git::expand_tilde(root).canonicalize() {
            roots.push(real);
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

/// Home of the user `dir` belongs to.
///
/// The ancestor sitting directly under a home container (`/home`, `/Users`),
/// or `/root` itself. Inferred from the path because the daemon's own `$HOME`
/// names the wrong user whenever it runs as someone else than the session.
fn home_of(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|a| {
            *a == Path::new("/root")
                || a.parent().is_some_and(|p| p == Path::new("/home") || p == Path::new("/Users"))
        })
        .map(Path::to_path_buf)
}

/// Operator-supplied roots for the cases the defaults cannot infer.
///
/// `CCTUI_READ_FILE_ROOTS` (`:`-separated, for dispatched worker pods that
/// never run `enroll`) then `read_file_roots` in `daemon.toml`.
fn extra_roots() -> Vec<String> {
    let mut roots: Vec<String> = std::env::var("CCTUI_READ_FILE_ROOTS")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if let Ok(cfg) = crate::config::Config::load_from(&crate::config::Config::default_path()) {
        roots.extend(cfg.read_file_roots);
    }
    roots
}

/// Nearest ancestor of `dir` (inclusive) holding a `.git` entry.
fn git_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find(|a| a.join(".git").exists()).map(Path::to_path_buf)
}

/// Secrets that stay unreadable however the roots are widened — a session
/// whose cwd is `$HOME` or a repo carrying a `.env` must not become a
/// credential dump.
fn is_denied(real: &Path) -> bool {
    const DENIED_DIRS: [&str; 3] = [".ssh", ".gnupg", ".aws"];
    let name = real.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let Some(parent) = real.parent() else { return true };
    if parent
        .components()
        .any(|c| DENIED_DIRS.contains(&c.as_os_str().to_str().unwrap_or_default()))
        || parent.ends_with(".config/gh")
    {
        return true;
    }
    name == ".netrc"
        || name == ".credentials.json"
        || name.starts_with(".env")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || matches!(name.to_ascii_lowercase().rsplit_once('.'), Some((_, "pem" | "key")))
}

/// Expand `~`, canonicalise (following symlinks), and require a regular file
/// under one of `roots`. A symlink whose target escapes every root is refused
/// even when the link itself sits inside one.
pub fn resolve(path: &str, roots: &[PathBuf]) -> Result<PathBuf, Refused> {
    let expanded = crate::git::expand_tilde(path);
    if !expanded.is_absolute() {
        return Err(refused(ReadFileErrorKind::Denied, "path must be absolute"));
    }
    let real = match expanded.canonicalize() {
        Ok(p) => p,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(refused(ReadFileErrorKind::NotFound, format!("{path}: not found")));
        }
        Err(err) => {
            return Err(refused(ReadFileErrorKind::Io, format!("cannot resolve {path}: {err}")));
        }
    };
    if !roots.iter().any(|root| real.starts_with(root)) {
        let checked: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
        return Err(refused(
            ReadFileErrorKind::Denied,
            format!("{path} is outside the allowed roots: {}", checked.join(", ")),
        ));
    }
    if is_denied(&real) {
        return Err(refused(ReadFileErrorKind::Denied, format!("{path} is not readable")));
    }
    let meta = std::fs::metadata(&real)
        .map_err(|err| refused(ReadFileErrorKind::Io, format!("cannot stat {path}: {err}")))?;
    if !meta.is_file() {
        return Err(refused(ReadFileErrorKind::Denied, format!("{path} is not a regular file")));
    }
    Ok(real)
}

/// Bytes of a resolved file, refusing anything over `min(max_bytes,
/// READ_FILE_MAX_BYTES)` before reading it.
pub fn read_capped(real: &Path, max_bytes: u64) -> Result<Vec<u8>, Refused> {
    let cap = max_bytes.min(READ_FILE_MAX_BYTES);
    let len = std::fs::metadata(real)
        .map_err(|err| refused(ReadFileErrorKind::Io, err.to_string()))?
        .len();
    if len > cap {
        return Err(refused(
            ReadFileErrorKind::TooLarge,
            format!("{} is {len} bytes; the cap is {cap} bytes", real.display()),
        ));
    }
    let bytes =
        std::fs::read(real).map_err(|err| refused(ReadFileErrorKind::Io, err.to_string()))?;
    if bytes.len() as u64 > cap {
        return Err(refused(
            ReadFileErrorKind::TooLarge,
            format!("{} grew past the cap", real.display()),
        ));
    }
    Ok(bytes)
}

fn file_name(real: &Path) -> String {
    real.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_owned()
}

/// Resolve + read + (for large files) upload, producing the reply frame.
pub async fn handle(
    client: &ServerClient,
    machine_key: &str,
    request_id: uuid::Uuid,
    path: &str,
    max_bytes: u64,
    cwd: Option<&str>,
) -> DaemonFrameUp {
    match read(client, machine_key, path, max_bytes, cwd).await {
        Ok(file) => DaemonFrameUp::ReadFileResult {
            request_id,
            ok: true,
            file: Some(file),
            error_kind: None,
            error: None,
        },
        Err(Refused { kind, message }) => {
            tracing::warn!(%path, ?kind, %message, "read-file refused");
            DaemonFrameUp::ReadFileResult {
                request_id,
                ok: false,
                file: None,
                error_kind: Some(kind),
                error: Some(message),
            }
        }
    }
}

async fn read(
    client: &ServerClient,
    machine_key: &str,
    path: &str,
    max_bytes: u64,
    cwd: Option<&str>,
) -> Result<ReadFileOk, Refused> {
    let roots = allowed_roots(cwd);
    let real = resolve(path, &roots)?;
    let bytes = read_capped(&real, max_bytes)?;
    let name = file_name(&real);
    let size = bytes.len() as u64;
    let sha256 = hex::encode(Sha256::digest(&bytes));
    let media_type = sniff_media_type(&name, &bytes[..bytes.len().min(8192)]);
    if size <= READ_FILE_INLINE_BYTES {
        return Ok(ReadFileOk {
            name,
            size,
            sha256,
            media_type: Some(media_type.to_owned()),
            data: Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
            blob_hash: None,
        });
    }
    client
        .put_blob(machine_key, &sha256, bytes, Some(media_type))
        .await
        .map_err(|err| refused(ReadFileErrorKind::Io, format!("blob upload failed: {err}")))?;
    Ok(ReadFileOk {
        name,
        size,
        sha256: sha256.clone(),
        media_type: Some(media_type.to_owned()),
        data: None,
        blob_hash: Some(sha256),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots_of(dir: &Path) -> Vec<PathBuf> {
        vec![dir.canonicalize().unwrap()]
    }

    #[test]
    fn regular_file_under_a_root_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("report.md");
        std::fs::write(&f, "# hi").unwrap();
        let real = resolve(f.to_str().unwrap(), &roots_of(dir.path())).unwrap();
        assert_eq!(real, f.canonicalize().unwrap());
        assert_eq!(read_capped(&real, READ_FILE_MAX_BYTES).unwrap(), b"# hi");
    }

    #[test]
    fn outside_roots_relative_and_directories_are_denied() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let f = other.path().join("x.txt");
        std::fs::write(&f, "x").unwrap();
        let roots = roots_of(dir.path());
        assert_eq!(
            resolve(f.to_str().unwrap(), &roots).unwrap_err().kind,
            ReadFileErrorKind::Denied
        );
        assert_eq!(resolve("relative/x.txt", &roots).unwrap_err().kind, ReadFileErrorKind::Denied);
        assert_eq!(
            resolve(dir.path().to_str().unwrap(), &roots).unwrap_err().kind,
            ReadFileErrorKind::Denied
        );
        assert_eq!(
            resolve(dir.path().join("missing").to_str().unwrap(), &roots).unwrap_err().kind,
            ReadFileErrorKind::NotFound
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_the_root_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("passwd");
        std::os::unix::fs::symlink("/etc/passwd", &link).unwrap();
        let err = resolve(link.to_str().unwrap(), &roots_of(dir.path())).unwrap_err();
        assert_eq!(err.kind, ReadFileErrorKind::Denied);

        let dotdot = dir.path().join("sub/../../../../etc/passwd");
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let err = resolve(dotdot.to_str().unwrap(), &roots_of(dir.path())).unwrap_err();
        assert_eq!(err.kind, ReadFileErrorKind::Denied);
    }

    #[test]
    fn the_session_cwd_is_a_root() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("out.txt");
        std::fs::write(&f, "x").unwrap();
        let roots = roots_from(Some(dir.path().to_str().unwrap()), &[]);
        assert!(roots.contains(&dir.path().canonicalize().unwrap()));
        assert!(resolve(f.to_str().unwrap(), &roots).is_ok());
    }

    #[test]
    fn the_daemon_users_claude_job_dir_is_a_root_including_its_tmp() {
        let Some(jobs) = dirs::home_dir()
            .map(|h| h.join(".claude").join("jobs"))
            .and_then(|j| j.canonicalize().ok())
        else {
            return;
        };
        assert!(roots_from(None, &[]).contains(&jobs), "the daemon user's job dir must be a root");
        let tmp = jobs.join("cdfadc1d").join("tmp").join("new-ticket.md");
        assert!(tmp.starts_with(&jobs), "a job's tmp is covered by the jobs root");
    }

    #[test]
    fn home_of_identifies_the_user_a_path_belongs_to() {
        assert_eq!(home_of(Path::new("/home/gtax/Documents/repo")), Some("/home/gtax".into()));
        assert_eq!(home_of(Path::new("/home/gtax")), Some("/home/gtax".into()));
        assert_eq!(home_of(Path::new("/Users/gtax/src/a")), Some("/Users/gtax".into()));
        assert_eq!(home_of(Path::new("/root/src")), Some("/root".into()));
        assert_eq!(home_of(Path::new("/srv/app")), None);
        assert_eq!(home_of(Path::new("/home")), None);
    }

    /// A tempdir cannot stand in for "outside every root" — the temp dirs are
    /// always roots. `/etc` is the nearest thing to a directory that exists,
    /// holds a readable regular file and is never a default root.
    #[test]
    fn configured_extra_roots_widen_and_nothing_else_does() {
        let Ok(etc) = Path::new("/etc").canonicalize() else { return };
        let Ok(hosts) = Path::new("/etc/hosts").canonicalize() else { return };
        let Ok(elsewhere) = Path::new("/bin/sh").canonicalize() else { return };
        if !hosts.starts_with(&etc) || !hosts.is_file() || elsewhere.starts_with(&etc) {
            return;
        }

        let without = roots_from(None, &[]);
        assert_eq!(resolve("/etc/hosts", &without).unwrap_err().kind, ReadFileErrorKind::Denied);

        let with = roots_from(None, &["/etc".to_owned()]);
        assert!(resolve("/etc/hosts", &with).is_ok());
        assert_eq!(
            resolve(elsewhere.to_str().unwrap(), &with).unwrap_err().kind,
            ReadFileErrorKind::Denied,
            "an extra root widens only itself"
        );
    }

    #[test]
    fn a_denial_names_the_roots_it_was_checked_against() {
        let dir = tempfile::tempdir().unwrap();
        let roots = vec![dir.path().canonicalize().unwrap()];
        let err = resolve("/etc/passwd", &roots).unwrap_err();
        assert_eq!(err.kind, ReadFileErrorKind::Denied);
        assert!(
            err.message.contains(dir.path().canonicalize().unwrap().to_str().unwrap()),
            "message must list the roots: {}",
            err.message
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_an_extra_root_is_still_denied() {
        let extra = tempfile::tempdir().unwrap();
        let link = extra.path().join("escape.txt");
        std::os::unix::fs::symlink("/etc/passwd", &link).unwrap();
        let roots = roots_from(None, &[extra.path().to_str().unwrap().to_owned()]);
        assert_eq!(
            resolve(link.to_str().unwrap(), &roots).unwrap_err().kind,
            ReadFileErrorKind::Denied
        );
    }

    #[test]
    fn without_a_session_the_daemons_home_is_never_a_root() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("out.txt");
        std::fs::write(&f, "x").unwrap();
        let roots = roots_from(Some(dir.path().to_str().unwrap()), &[]);
        assert!(roots.contains(&dir.path().canonicalize().unwrap()));
        assert!(resolve(f.to_str().unwrap(), &roots).is_ok());

        let home = PathBuf::from(std::env::var("HOME").unwrap()).canonicalize().unwrap();
        for cwd in [None, Some("/definitely/not/a/dir")] {
            assert!(
                !roots_from(cwd, &[]).contains(&home),
                "an unattributed read must not walk a home (cwd {cwd:?})"
            );
        }
    }

    #[test]
    fn the_home_of_the_session_user_is_a_root_but_other_users_homes_are_not() {
        let cwd = Path::new("/home/gtax/Documents/repo");
        assert_eq!(home_of(cwd), Some(PathBuf::from("/home/gtax")));
        assert_eq!(home_of(Path::new("/home/someone-else/x")), Some("/home/someone-else".into()));
        assert_ne!(home_of(cwd), home_of(Path::new("/home/someone-else/x")));

        let Some(home) = dirs::home_dir().and_then(|h| h.canonicalize().ok()) else { return };
        if home_of(&home).as_ref() != Some(&home) {
            return;
        }
        assert!(
            roots_from(Some(home.to_str().unwrap()), &[]).contains(&home),
            "the home the session's cwd sits in is a root"
        );
    }

    #[test]
    fn the_git_root_of_the_cwd_is_readable_but_its_siblings_are_not() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        std::fs::create_dir_all(repo.path().join("sub")).unwrap();
        let top = repo.path().join("README.md");
        std::fs::write(&top, "x").unwrap();
        let roots = roots_from(Some(repo.path().join("sub").to_str().unwrap()), &[]);
        assert!(resolve(top.to_str().unwrap(), &roots).is_ok());
        assert_eq!(
            resolve("/etc/passwd", &roots).unwrap_err().kind,
            ReadFileErrorKind::Denied,
            "the git root does not widen past the repo"
        );
    }

    #[test]
    fn secrets_are_denied_inside_an_allowed_root() {
        let dir = tempfile::tempdir().unwrap();
        let roots = roots_of(dir.path());
        for rel in [
            ".ssh/id_ed25519",
            ".aws/credentials",
            ".gnupg/secring.gpg",
            ".config/gh/hosts.yml",
            ".claude/.credentials.json",
            "repo/.env",
            "repo/.env.local",
            "certs/server.pem",
            "certs/server.key",
            "certs/SERVER.PEM",
            "certs/Server.Key",
            "certs/.pem",
            ".netrc",
        ] {
            let f = dir.path().join(rel);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, "secret").unwrap();
            assert_eq!(
                resolve(f.to_str().unwrap(), &roots).unwrap_err().kind,
                ReadFileErrorKind::Denied,
                "{rel} must be denied"
            );
        }
        let ok = dir.path().join("repo/report.md");
        std::fs::write(&ok, "x").unwrap();
        assert!(resolve(ok.to_str().unwrap(), &roots).is_ok());
    }

    #[test]
    fn secrets_are_denied_when_the_cwd_is_the_home_that_holds_them() {
        let home = tempfile::tempdir().unwrap();
        let key = home.path().join(".ssh/id_ed25519");
        std::fs::create_dir_all(key.parent().unwrap()).unwrap();
        std::fs::write(&key, "PRIVATE KEY").unwrap();
        let roots = roots_from(Some(home.path().to_str().unwrap()), &[]);
        assert!(roots.contains(&home.path().canonicalize().unwrap()), "cwd is a root");
        assert_eq!(
            resolve(key.to_str().unwrap(), &roots).unwrap_err().kind,
            ReadFileErrorKind::Denied
        );
    }

    #[test]
    fn size_cap_is_enforced_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("big.bin");
        std::fs::write(&f, vec![0u8; 1024]).unwrap();
        let real = f.canonicalize().unwrap();
        let err = read_capped(&real, 1023).unwrap_err();
        assert_eq!(err.kind, ReadFileErrorKind::TooLarge);
        assert!(read_capped(&real, 1024).is_ok());
        assert!(read_capped(&real, u64::MAX).is_ok(), "caller cap clamps to READ_FILE_MAX_BYTES");
    }

    #[tokio::test]
    async fn handle_reports_refusals_as_error_frames() {
        let client = ServerClient::new("http://127.0.0.1:9");
        let up = handle(&client, "k", uuid::Uuid::nil(), "/proc/self/status", 10, None).await;
        match up {
            DaemonFrameUp::ReadFileResult { ok, error_kind, file, .. } => {
                assert!(!ok);
                assert_eq!(error_kind, Some(ReadFileErrorKind::Denied));
                assert!(file.is_none());
            }
            other => panic!("unexpected frame {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_inlines_small_files_with_hash_and_type() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("note.md");
        std::fs::write(&f, "# note").unwrap();
        let client = ServerClient::new("http://127.0.0.1:9");
        let cwd = dir.path().to_str().unwrap();
        let up =
            handle(&client, "k", uuid::Uuid::nil(), f.to_str().unwrap(), 1 << 20, Some(cwd)).await;
        let DaemonFrameUp::ReadFileResult { ok: true, file: Some(file), .. } = up else {
            panic!("expected ok frame, got {up:?}");
        };
        assert_eq!(file.name, "note.md");
        assert_eq!(file.size, 6);
        assert_eq!(file.sha256, hex::encode(Sha256::digest(b"# note")));
        assert_eq!(file.media_type.as_deref(), Some("text/markdown; charset=utf-8"));
        assert_eq!(file.data.as_deref(), Some("IyBub3Rl"));
        assert!(file.blob_hash.is_none());
    }
}
