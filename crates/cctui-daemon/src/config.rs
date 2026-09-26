//! Daemon on-disk configuration.
//!
//! Lives at `$XDG_CONFIG_HOME/cctui/daemon.toml` (or
//! `~/.config/cctui/daemon.toml`). Written by `cctui-daemon enroll`; read
//! by `cctui-daemon run`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server_url: String,
    pub machine_key: String,
    pub machine_id: Option<uuid::Uuid>,
    /// Extra roots the linked-file viewer may read from, for the cases the
    /// session's cwd and job dir cannot infer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_file_roots: Vec<String>,
}

impl Config {
    #[must_use]
    pub fn default_path() -> PathBuf {
        dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("cctui").join("daemon.toml")
    }

    pub fn load_from(path: &PathBuf) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!(
                    "no config at {} — this machine is not enrolled yet. \
                     Run `cctui-daemon enroll --server-url <url> --token <token> --name <name>` first.",
                    path.display()
                )
            } else {
                anyhow::Error::new(err).context(format!("reading {}", path.display()))
            }
        })?;
        Ok(toml::from_str(&raw)?)
    }

    /// Build a config purely from environment variables, for dispatched
    /// worker pods that are handed a shared machine key and never run `enroll`.
    /// `CCTUI_MACHINE_KEY` + (`CCTUI_SERVER_URL` or `CCTUI_URL`) are
    /// required; `machine_id` is unknown here (the server returns it from
    /// `daemon_auth`). Returns `None` when the key isn't set so the caller can
    /// fall back to the on-disk config.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let machine_key = std::env::var("CCTUI_MACHINE_KEY").ok().filter(|s| !s.is_empty())?;
        let server_url = std::env::var("CCTUI_SERVER_URL")
            .or_else(|_| std::env::var("CCTUI_URL"))
            .ok()
            .filter(|s| !s.is_empty())?;
        Some(Self { server_url, machine_key, machine_id: None, read_file_roots: Vec::new() })
    }

    /// Resolve config for `run`: prefer the env-provided shared key (dispatch
    /// pods), otherwise the on-disk config written by `enroll`.
    pub fn load_or_env(path: &PathBuf) -> anyhow::Result<Self> {
        if let Some(cfg) = Self::from_env() {
            tracing::info!("using machine key from environment (CCTUI_MACHINE_KEY)");
            return Ok(cfg);
        }
        Self::load_from(path)
    }

    /// Whether a config file exists at `path`. Used by `status` to report
    /// enrolment state without surfacing a raw I/O error.
    #[must_use]
    pub fn exists_at(path: &Path) -> bool {
        path.exists()
    }

    pub fn save_to(&self, path: &PathBuf) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(self)?;
        std::fs::write(path, raw)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(path, perms)?;
        }
        Ok(())
    }
}
