//! One-shot remote install + enrolment over ssh.
//!
//! `cctui-daemon enroll <user@host> --server-url … --token …` takes a machine
//! from zero to a connected fleet member:
//!
//!   1. probe the target over ssh (platform, existing install, config)
//!   2. pull the right binary through the server's release proxy
//!      (`/api/v1/daemon/binary/{target}`), verify its checksum locally AND
//!      after upload
//!   3. `install -m755`-equivalent into `~/.local/bin/cctui-daemon`
//!   4. obtain a machine key — reuse the target's existing enrolment when its
//!      `daemon.toml` still authenticates against the same server, otherwise
//!      mint a fresh one via `POST /api/v1/enroll` with the operator's token —
//!      and write `~/.config/cctui/daemon.toml`
//!   5. drop the systemd user unit, `loginctl enable-linger`,
//!      `systemctl --user enable` + start/restart
//!   6. poll `GET /api/v1/machines/{id}/status` until the daemon's WS shows
//!      connected
//!
//! Re-running against an already-enrolled machine upgrades/repairs it (binary
//! refreshed only when its checksum differs, existing key kept when still
//! valid) instead of failing. Non-systemd targets (macOS/launchd) are a
//! follow-up and error out clearly.
//!
//! The manifest/checksum machinery is shared with [`crate::selfupdate`].

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::client::ServerClient;
use crate::config::Config;
use crate::{selfupdate, service};

/// Remote paths, all under the target user's `$HOME` (expanded remotely).
const REMOTE_BIN: &str = "$HOME/.local/bin/cctui-daemon";
const REMOTE_CONFIG: &str = "$HOME/.config/cctui/daemon.toml";
const REMOTE_UNIT: &str = "$HOME/.config/systemd/user/cctui-daemon.service";

pub struct RemoteEnrollOpts {
    pub ssh_target: String,
    pub server_url: String,
    pub token: String,
    /// Machine name for a fresh enrolment; defaults to the remote hostname.
    pub name: Option<String>,
    pub kind: String,
    pub verify_timeout: Duration,
}

/// What the probe step learned about the target.
#[derive(Debug, PartialEq, Eq)]
pub struct RemoteFacts {
    pub os: String,
    pub arch: String,
    pub hostname: String,
    pub has_systemctl: bool,
    /// sha256 of an existing `~/.local/bin/cctui-daemon`, if present.
    pub bin_sha: Option<String>,
    /// Raw contents of an existing `~/.config/cctui/daemon.toml`, if present.
    pub config: Option<String>,
    pub service_active: bool,
}

/// Reject targets that could be parsed as ssh options or extra words.
/// Everything else (aliases from `~/.ssh/config`, `user@host`, bare hosts)
/// passes through to ssh verbatim.
pub fn validate_ssh_target(target: &str) -> Result<()> {
    if target.trim().is_empty() {
        bail!("ssh target is empty");
    }
    if target.starts_with('-') {
        bail!("ssh target may not start with '-' (looks like an ssh option): {target}");
    }
    if target.chars().any(char::is_whitespace) {
        bail!("ssh target may not contain whitespace: {target:?}");
    }
    Ok(())
}

/// Map `uname -s` / `uname -m` onto the release-manifest target segment.
pub fn manifest_target(os: &str, arch: &str) -> Result<&'static str> {
    match (os.to_ascii_lowercase().as_str(), arch.to_ascii_lowercase().as_str()) {
        ("linux", "x86_64" | "amd64") => Ok("linux-amd64"),
        ("linux", "aarch64" | "arm64") => Ok("linux-arm64"),
        ("darwin", _) => bail!(
            "remote enroll does not support macOS targets yet (launchd install is a \
             follow-up) — install the binary on the machine and run \
             `cctui-daemon enroll --server-url … --token … --name …` there instead"
        ),
        (os, arch) => bail!("unsupported remote platform: {os}/{arch}"),
    }
}

/// Parse the `key=value` lines emitted by the probe script.
pub fn parse_probe(output: &str) -> Result<RemoteFacts> {
    let mut os = None;
    let mut arch = None;
    let mut hostname = None;
    let mut systemctl = None;
    let mut bin_sha = None;
    let mut config_b64 = None;
    let mut service = None;
    for line in output.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "os" => os = Some(v.to_owned()),
            "arch" => arch = Some(v.to_owned()),
            "hostname" => hostname = Some(v.to_owned()),
            "systemctl" => systemctl = Some(v == "yes"),
            "bin_sha" => bin_sha = Some(v.to_owned()),
            "config_b64" => config_b64 = Some(v.to_owned()),
            "service" => service = Some(v == "active"),
            _ => {}
        }
    }
    let missing = |what: &str| anyhow::anyhow!("probe output missing `{what}` (got: {output:?})");
    let config = match config_b64.ok_or_else(|| missing("config_b64"))?.as_str() {
        "absent" => None,
        b64 => {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .context("decoding remote daemon.toml (base64)")?;
            Some(String::from_utf8(bytes).context("remote daemon.toml is not UTF-8")?)
        }
    };
    Ok(RemoteFacts {
        os: os.ok_or_else(|| missing("os"))?,
        arch: arch.ok_or_else(|| missing("arch"))?,
        hostname: hostname.ok_or_else(|| missing("hostname"))?,
        has_systemctl: systemctl.ok_or_else(|| missing("systemctl"))?,
        bin_sha: match bin_sha.ok_or_else(|| missing("bin_sha"))?.as_str() {
            "absent" => None,
            sha => Some(sha.to_owned()),
        },
        config,
        service_active: service.unwrap_or(false),
    })
}

/// Whether the remote binary must be (re)installed given its current sha.
#[must_use]
pub fn binary_needs_install(remote_sha: Option<&str>, expected_sha: &str) -> bool {
    remote_sha != Some(expected_sha)
}

/// Pure half of the reuse decision.
///
/// An existing remote config is a reuse *candidate* only if it parses and
/// points at the same server. The async half (`daemon_auth` proving the key
/// still works) lives in [`run`].
#[must_use]
pub fn reusable_config(raw: &str, server_url: &str) -> Option<Config> {
    let cfg: Config = toml::from_str(raw).ok()?;
    (cfg.server_url.trim_end_matches('/') == server_url.trim_end_matches('/')).then_some(cfg)
}

/// Run `script` on the target through ssh, optionally feeding `stdin`.
/// The script is a fixed string (no interpolation of caller data), passed as
/// a single argv element so the remote shell sees it verbatim.
async fn ssh(target: &str, script: &str, stdin: Option<&[u8]>) -> Result<String> {
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg(target)
        .arg(script)
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().context("spawning ssh (is OpenSSH installed?)")?;
    if let Some(bytes) = stdin {
        let mut pipe = child.stdin.take().expect("stdin was piped");
        pipe.write_all(bytes).await.context("writing to ssh stdin")?;
        pipe.shutdown().await.context("closing ssh stdin")?;
        drop(pipe);
    }
    let out = child.wait_with_output().await.context("waiting for ssh")?;
    if !out.status.success() {
        bail!(
            "ssh {target} failed ({}): {}\n(remote enroll needs non-interactive ssh — \
             set up key/agent auth for this target)",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

const PROBE_SCRIPT: &str = r#"set -e
echo "os=$(uname -s)"
echo "arch=$(uname -m)"
echo "hostname=$(uname -n)"
if command -v systemctl >/dev/null 2>&1; then echo "systemctl=yes"; else echo "systemctl=no"; fi
if [ -f "$HOME/.local/bin/cctui-daemon" ]; then
  echo "bin_sha=$(sha256sum "$HOME/.local/bin/cctui-daemon" | cut -d" " -f1)"
else
  echo "bin_sha=absent"
fi
if [ -f "$HOME/.config/cctui/daemon.toml" ]; then
  echo "config_b64=$(base64 < "$HOME/.config/cctui/daemon.toml" | tr -d "\n")"
else
  echo "config_b64=absent"
fi
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
if systemctl --user is-active --quiet cctui-daemon.service 2>/dev/null; then
  echo "service=active"
else
  echo "service=inactive"
fi
"#;

const INSTALL_BINARY_SCRIPT: &str = r#"set -e
mkdir -p "$HOME/.local/bin"
tmp="$HOME/.local/bin/.cctui-daemon.enroll-tmp"
cat > "$tmp"
chmod 755 "$tmp"
mv "$tmp" "$HOME/.local/bin/cctui-daemon"
sha256sum "$HOME/.local/bin/cctui-daemon" | cut -d" " -f1
"#;

const WRITE_CONFIG_SCRIPT: &str = r#"set -e
umask 077
mkdir -p "$HOME/.config/cctui"
cat > "$HOME/.config/cctui/daemon.toml"
chmod 600 "$HOME/.config/cctui/daemon.toml"
"#;

/// Everything after the unit file lands: linger (best-effort — may need
/// polkit), daemon-reload, enable, then start or restart per `%ACTION%`.
const SERVICE_SCRIPT: &str = r#"set -e
mkdir -p "$HOME/.config/systemd/user"
cat > "$HOME/.config/systemd/user/cctui-daemon.service"
loginctl enable-linger "$(id -un)" 2>/dev/null || echo "warn: loginctl enable-linger failed (daemon will stop at logout until linger is enabled)"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}"
systemctl --user daemon-reload
systemctl --user enable cctui-daemon.service
systemctl --user %ACTION% cctui-daemon.service
"#;

/// The whole remote flow. Prints progress per step; every remote action is a
/// separate, individually-verified ssh round-trip.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub async fn run(opts: RemoteEnrollOpts) -> Result<()> {
    validate_ssh_target(&opts.ssh_target)?;
    let target = opts.ssh_target.as_str();
    let server_url = opts.server_url.trim_end_matches('/').to_owned();
    let client = ServerClient::new(&server_url);

    println!("[1/6] probing {target} over ssh");
    let probe_out = ssh(target, PROBE_SCRIPT, None).await.context("probing the target")?;
    let facts = parse_probe(&probe_out)?;
    let release_target = manifest_target(&facts.os, &facts.arch)?;
    if !facts.has_systemctl {
        bail!(
            "{} ({}/{}) has no systemctl — remote enroll currently requires a systemd \
             user session",
            facts.hostname,
            facts.os,
            facts.arch
        );
    }
    println!(
        "      {} — {}/{} → {release_target}{}{}",
        facts.hostname,
        facts.os,
        facts.arch,
        if facts.config.is_some() { ", existing enrolment found" } else { "" },
        if facts.service_active { ", service running" } else { "" },
    );

    println!("[2/6] fetching daemon release from {server_url}");
    let http = selfupdate::client()?;
    let manifest = selfupdate::fetch_manifest(&http, &server_url, &opts.token)
        .await
        .context("fetching the daemon manifest (is the token valid?)")?;
    let asset = format!("cctui-daemon-{release_target}");
    let binary_url = manifest
        .assets
        .iter()
        .find(|a| a.target == release_target)
        .map(|a| a.url.clone())
        .with_context(|| {
            format!("manifest {} has no asset for target {release_target}", manifest.version)
        })?;
    let sums = selfupdate::download(&http, &selfupdate::sha256sums_url(&server_url), &opts.token)
        .await
        .context("downloading SHA256SUMS")?;
    let sums = std::str::from_utf8(&sums).context("SHA256SUMS not UTF-8")?;
    let expected_sha = selfupdate::parse_sha256sums(sums, &asset)
        .with_context(|| format!("{asset} missing from SHA256SUMS"))?;

    let install_binary = binary_needs_install(facts.bin_sha.as_deref(), &expected_sha);
    if install_binary {
        println!("[3/6] installing cctui-daemon {} → {REMOTE_BIN}", manifest.version);
        let bytes = selfupdate::download(&http, &binary_url, &opts.token)
            .await
            .context("downloading the daemon binary")?;
        let local_sha = selfupdate::hex_sha256(&bytes);
        if local_sha != expected_sha {
            bail!("downloaded {asset} hash {local_sha} != expected {expected_sha}");
        }
        let remote_sha = ssh(target, INSTALL_BINARY_SCRIPT, Some(&bytes))
            .await
            .context("uploading the binary")?;
        let remote_sha = remote_sha.trim();
        if remote_sha != expected_sha {
            bail!("uploaded binary hash {remote_sha} != expected {expected_sha}");
        }
    } else {
        println!("[3/6] binary already current (version {}) — skipping upload", manifest.version);
    }

    // Machine key: reuse the target's existing enrolment when it still
    // authenticates against this server; otherwise mint a fresh key.
    let reuse = match facts.config.as_deref().and_then(|raw| reusable_config(raw, &server_url)) {
        Some(cfg) => match client.daemon_auth(&cfg.machine_key).await {
            Ok(auth) => Some((cfg, auth.machine_id)),
            Err(err) => {
                println!("      existing machine key no longer valid ({err}); re-enrolling");
                None
            }
        },
        None => None,
    };
    let (machine_id, wrote_config) = if let Some((_, machine_id)) = reuse {
        println!("[4/6] reusing existing enrolment (machine {machine_id})");
        (machine_id, false)
    } else {
        let name = opts.name.clone().unwrap_or_else(|| facts.hostname.clone());
        println!("[4/6] enrolling '{name}' with {server_url}");
        let kind_arg = (opts.kind != "persistent").then_some(opts.kind.as_str());
        let resp = client
            .enroll(&opts.token, &name, kind_arg)
            .await
            .context("enrolling the machine (does the token have the enroll scope?)")?;
        let cfg = Config {
            server_url: server_url.clone(),
            machine_key: resp.machine_key,
            machine_id: Some(resp.machine_id),
            read_file_roots: Vec::new(),
        };
        let raw = toml::to_string_pretty(&cfg)?;
        ssh(target, WRITE_CONFIG_SCRIPT, Some(raw.as_bytes()))
            .await
            .with_context(|| format!("writing {REMOTE_CONFIG}"))?;
        (resp.machine_id, true)
    };

    println!("[5/6] installing systemd user unit + enabling linger");
    // Restart when anything the running daemon depends on changed; plain
    // `start` otherwise (a no-op on an already-active unit).
    let action = if install_binary || wrote_config { "restart" } else { "start" };
    let script = SERVICE_SCRIPT.replace("%ACTION%", action);
    let unit_out = ssh(target, &script, Some(service::UNIT_TEMPLATE.as_bytes()))
        .await
        .with_context(|| format!("installing {REMOTE_UNIT} / starting the service"))?;
    for line in unit_out.lines().filter(|l| l.starts_with("warn:")) {
        println!("      {line}");
    }

    println!(
        "[6/6] waiting for machine {machine_id} to connect (timeout {}s)",
        opts.verify_timeout.as_secs()
    );
    verify_connected(&client, &opts.token, machine_id, opts.verify_timeout, target).await?;
    println!("done: {} is enrolled and connected as machine {machine_id}", facts.hostname);
    Ok(())
}

async fn verify_connected(
    client: &ServerClient,
    token: &str,
    machine_id: Uuid,
    timeout: Duration,
    target: &str,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match client.machine_status(token, machine_id).await {
            Ok(st) if st.connected => {
                println!("      connected (liveness: {})", st.liveness);
                return Ok(());
            }
            Ok(st) if st.revoked => {
                bail!("machine {machine_id} ({}) is revoked on the server", st.name)
            }
            Ok(_) => {}
            Err(err) => tracing::debug!(%err, "machine status poll failed"),
        }
        if Instant::now() >= deadline {
            bail!(
                "machine {machine_id} did not connect within {}s — inspect the daemon with \
                 `ssh {target} journalctl --user -u cctui-daemon -n 50` (or \
                 `ssh {target} systemctl --user status cctui-daemon`)",
                timeout.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_target_validation() {
        assert!(validate_ssh_target("user@host").is_ok());
        assert!(validate_ssh_target("myalias").is_ok());
        assert!(validate_ssh_target("").is_err());
        assert!(validate_ssh_target("  ").is_err());
        assert!(validate_ssh_target("-oProxyCommand=evil").is_err());
        assert!(validate_ssh_target("host extra").is_err());
    }

    #[test]
    fn manifest_target_mapping() {
        assert_eq!(manifest_target("Linux", "x86_64").unwrap(), "linux-amd64");
        assert_eq!(manifest_target("linux", "aarch64").unwrap(), "linux-arm64");
        assert_eq!(manifest_target("Linux", "arm64").unwrap(), "linux-arm64");
        let mac = manifest_target("Darwin", "arm64").unwrap_err().to_string();
        assert!(mac.contains("macOS"), "{mac}");
        assert!(manifest_target("FreeBSD", "amd64").is_err());
    }

    #[test]
    fn probe_parses_fresh_machine() {
        let out = "os=Linux\narch=x86_64\nhostname=box\nsystemctl=yes\n\
                   bin_sha=absent\nconfig_b64=absent\nservice=inactive\n";
        let facts = parse_probe(out).unwrap();
        assert_eq!(
            facts,
            RemoteFacts {
                os: "Linux".into(),
                arch: "x86_64".into(),
                hostname: "box".into(),
                has_systemctl: true,
                bin_sha: None,
                config: None,
                service_active: false,
            }
        );
    }

    #[test]
    fn probe_parses_enrolled_machine_and_ignores_noise() {
        use base64::Engine as _;
        let toml = "server_url = \"https://s\"\nmachine_key = \"k\"\n";
        let b64 = base64::engine::general_purpose::STANDARD.encode(toml);
        let out = format!(
            "motd noise without equals\nos=Linux\narch=aarch64\nhostname=pi\nsystemctl=yes\n\
             bin_sha=abc123\nconfig_b64={b64}\nservice=active\n"
        );
        let facts = parse_probe(&out).unwrap();
        assert_eq!(facts.bin_sha.as_deref(), Some("abc123"));
        assert_eq!(facts.config.as_deref(), Some(toml));
        assert!(facts.service_active);
    }

    #[test]
    fn probe_missing_keys_is_an_error() {
        let err = parse_probe("os=Linux\n").unwrap_err().to_string();
        assert!(err.contains("probe output missing"), "{err}");
    }

    #[test]
    fn binary_install_is_idempotent_on_matching_sha() {
        assert!(!binary_needs_install(Some("aaa"), "aaa"));
        assert!(binary_needs_install(Some("bbb"), "aaa"));
        assert!(binary_needs_install(None, "aaa"));
    }

    #[test]
    fn config_reuse_requires_same_server() {
        let raw = "server_url = \"https://cctui.example.com/\"\nmachine_key = \"mk_x\"\n";
        assert!(reusable_config(raw, "https://cctui.example.com").is_some());
        assert!(reusable_config(raw, "https://other.example.com").is_none());
        assert!(reusable_config("not toml [", "https://cctui.example.com").is_none());
    }

    #[test]
    fn service_script_action_substitution() {
        let s = SERVICE_SCRIPT.replace("%ACTION%", "restart");
        assert!(s.contains("systemctl --user restart cctui-daemon.service"));
        assert!(!s.contains("%ACTION%"));
    }
}
