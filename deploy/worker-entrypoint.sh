#!/usr/bin/env sh
# Entrypoint for the cctui worker container (contract v1).
#
# The dispatcher injects the session identity as env (CCTUI_URL,
# CCTUI_MACHINE_KEY, SESSION_ID, TASK_PAYLOAD_JSON, optional REPLY_URL /
# TASK_NAME). The cctui-daemon reads CCTUI_MACHINE_KEY + CCTUI_URL from the
# environment (Config::from_env), so no enroll step and no config file are
# needed here.
#
# The container starts as ROOT to bootstrap the sandbox, then drops to the
# unprivileged `worker` user (uid 1000) via cctui-supervisor before exec'ing the
# daemon. The phases below run root-side; each is INDIVIDUALLY SKIPPABLE on its
# own env, so a worker booted with ONLY CCTUI_URL + CCTUI_MACHINE_KEY +
# SESSION_ID falls straight through to
# `exec cctui-supervisor -- cctui-daemon run --no-auto-update`.
#
# No phase may hard-fail when its inputs are absent. The ONE exception is the
# context-pack fetch: if CONTEXT_PACK_URL is set it MUST succeed (the pack
# defines the guard rules — proceeding without it would weaken the sandbox).
#
# Ephemeral worker: --no-auto-update, since the container is short-lived and
# re-fetched on every spawn (k8s/docker workers never self-update — CCT memory).
#
# See docs/worker-contract.md for the full env / mount / capability contract.
set -eu

log() { echo "cctui-worker: $*"; }

# Extra read-only paths for DERIVED images. The base entrypoint's RO
# allow-list is fixed, but a fat derived image installs its own toolchain outside
# it (e.g. Node/pnpm under /opt/mise, Rust under /opt/rust). Such an image can set
# CCTUI_WORKER_EXTRA_RO (colon-separated, e.g. `/opt/mise:/opt/rust`) to have each
# path added to the supervisor's Landlock RO set. Emits a `--ro <path>` token per
# non-empty entry (newline-separated for word-splitting at the call sites); a
# no-op when the var is unset/empty.
extra_ro_flags() {
    _ero="${CCTUI_WORKER_EXTRA_RO:-}"
    [ -n "$_ero" ] || return 0
    _OLDIFS=$IFS; IFS=:
    for _p in $_ero; do
        IFS=$_OLDIFS
        [ -n "$_p" ] && printf '%s\n%s\n' --ro "$_p"
        IFS=:
    done
    IFS=$_OLDIFS
}

# RW analogue of extra_ro_flags. Derived images — and phase_dockerd
# below — add writable paths to the supervisor's Landlock RW set via
# CCTUI_WORKER_EXTRA_RW (colon-separated). Emits a `--rw <path>` token per
# non-empty entry; a no-op when unset/empty.
extra_rw_flags() {
    _erw="${CCTUI_WORKER_EXTRA_RW:-}"
    [ -n "$_erw" ] || return 0
    _OLDIFS=$IFS; IFS=:
    for _p in $_erw; do
        IFS=$_OLDIFS
        [ -n "$_p" ] && printf '%s\n%s\n' --rw "$_p"
        IFS=:
    done
    IFS=$_OLDIFS
}


# ── Required platform identity ──────────────────────────────────────────────
if [ -z "${CCTUI_MACHINE_KEY:-}" ]; then
    echo "cctui-worker: CCTUI_MACHINE_KEY is required (injected by the dispatcher)" >&2
    exit 1
fi
if [ -z "${CCTUI_URL:-}" ] && [ -z "${CCTUI_SERVER_URL:-}" ]; then
    echo "cctui-worker: CCTUI_URL (or CCTUI_SERVER_URL) is required (injected by the dispatcher)" >&2
    exit 1
fi
if [ -z "${SESSION_ID:-}" ]; then
    echo "cctui-worker: SESSION_ID is required (injected by the dispatcher)" >&2
    exit 1
fi

# Normalize the server URL we reason about (host extraction, policy seeding).
CCTUI_BASE_URL="${CCTUI_URL:-${CCTUI_SERVER_URL:-}}"

WORKER_UID=1000
WORKER_USER=worker
PROXY_UID=1337
PROXY_PORT=15001
PROXY_HEALTH_PORT=15002
GUARD_PORT=9999
POLICY_FILE=/var/run/guard-proxy/policy.json
GUARD_STATE=/var/run/workflow-guard/state
CONTEXT_DIR=/opt/context

adapter_is_codex() {
    case "${TASK_ADAPTER:-}" in
        codex | codex-cli) return 0 ;;
        *) return 1 ;;
    esac
}

# host:port from a URL (defaulting the port by scheme). Empty on no input.
url_hostport() {
    _u="$1"
    [ -n "$_u" ] || return 0
    _scheme=$(printf '%s' "$_u" | sed -n 's,^\([a-zA-Z][a-zA-Z0-9+.-]*\)://.*,\1,p')
    _hostport=$(printf '%s' "$_u" | sed -e 's,^[a-zA-Z][a-zA-Z0-9+.-]*://,,' -e 's,/.*$,,' -e 's,^[^@]*@,,')
    case "$_hostport" in
        *:*) printf '%s' "$_hostport" ;;
        *)
            case "$_scheme" in
                https) printf '%s:443' "$_hostport" ;;
                http)  printf '%s:80'  "$_hostport" ;;
                *)     printf '%s' "$_hostport" ;;
            esac
            ;;
    esac
}

# bare host (no port) from a URL.
url_host() {
    printf '%s' "$(url_hostport "$1")" | sed 's,:.*$,,'
}

# The guard-proxy sidecar TLS-terminates injection hosts with leaf certs
# signed by a per-pod CA it mints at boot, writing the PUBLIC cert (only) to the
# shared emptyDir below. Trust that CA so the worker's toolchains accept the
# intercepted TLS — IN ADDITION to the public roots, never replacing them
# (non-injected hosts stay passthrough with their real certs). Bounded wait +
# best-effort: if injection is disabled the file never appears, so we log and
# carry on. transparent-external only; single-container modes never MITM.
GUARD_CA_FILE="${GUARD_PROXY_CA_FILE:-/var/run/guard-proxy-ca/ca.pem}"
install_guard_ca() {
    _i=0
    while [ "$_i" -lt 20 ]; do
        [ -s "$GUARD_CA_FILE" ] && break
        _i=$((_i + 1)); sleep 0.5
    done
    if [ ! -s "$GUARD_CA_FILE" ]; then
        log "guard-proxy CA absent at $GUARD_CA_FILE after 10s — injection off (passthrough only); nothing to install"
        return 0
    fi

    _sys=""
    for _c in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt; do
        [ -f "$_c" ] && { _sys="$_c"; break; }
    done

    # Prefer the OS store: update-ca-certificates folds the guard CA into the
    # system bundle so every OpenSSL client trusts it with no env at all.
    _bundle="$GUARD_CA_FILE"
    if command -v update-ca-certificates >/dev/null 2>&1 \
            && mkdir -p /usr/local/share/ca-certificates 2>/dev/null \
            && cp "$GUARD_CA_FILE" /usr/local/share/ca-certificates/cctui-guard-proxy.crt 2>/dev/null \
            && update-ca-certificates >/dev/null 2>&1; then
        log "guard-proxy CA installed into system trust store"
        [ -n "$_sys" ] && _bundle="$_sys"
    elif [ -n "$_sys" ]; then
        # Store not writable: build a combined bundle (public roots + guard CA)
        # so the 'replace' env vars below don't drop the public roots.
        _bundle=/tmp/cctui-guard-ca-bundle.pem
        cat "$_sys" "$GUARD_CA_FILE" > "$_bundle" 2>/dev/null || _bundle="$GUARD_CA_FILE"
    fi

    # NODE_EXTRA_CA_CERTS is additive (Node keeps its bundled roots) → guard CA
    # alone; the rest replace the trust set → the combined bundle. Exported here,
    # inherited by the supervised daemon/agent tree.
    export NODE_EXTRA_CA_CERTS="$GUARD_CA_FILE"
    export GIT_SSL_CAINFO="$_bundle"
    export REQUESTS_CA_BUNDLE="$_bundle"
    export SSL_CERT_FILE="$_bundle"
    export CURL_CA_BUNDLE="$_bundle"
    log "guard-proxy CA trusted by worker toolchains (bundle=$_bundle)"
}

# Remote GPG signing. The private key never enters this container; the
# guard-proxy sidecar runs gpg-agent and forwards ONLY its restricted
# `--extra-socket` (cannot export secret keys) into the shared gpg-agent
# emptyDir. Here we point the worker's gpg at that forwarded socket: import the
# PUBLIC key the sidecar published, symlink gpg's expected agent-socket to the
# extra socket, and set user.signingkey + commit.gpgsign — ONLY when the socket
# is actually present. Bounded wait, best-effort: if absent (feature off) we log
# loud and leave signing unconfigured. transparent-external only (same gate as
# the CA install); single-container modes keep the legacy in-worker key import.
GPG_AGENT_DIR="${CCTUI_GPG_AGENT_DIR:-/var/run/gpg-agent}"
GPG_EXTRA_SOCKET="${CCTUI_GPG_EXTRA_SOCKET:-${GPG_AGENT_DIR}/S.gpg-agent.extra}"
wire_gpg_forwarding() {
    command -v gpg >/dev/null 2>&1 || { log "gpg missing — skipping remote signing wiring"; return 0; }
    _i=0
    while [ "$_i" -lt 20 ]; do
        [ -S "$GPG_EXTRA_SOCKET" ] && break
        _i=$((_i + 1)); sleep 0.5
    done
    if [ ! -S "$GPG_EXTRA_SOCKET" ]; then
        log "gpg-agent extra socket absent at $GPG_EXTRA_SOCKET after 10s — remote signing off; commits will be UNSIGNED"
        return 0
    fi

    _home="/home/${WORKER_USER}"
    _gnupg="${_home}/.gnupg"
    mkdir -p "$_gnupg" && chmod 700 "$_gnupg"

    if [ -s "${GPG_AGENT_DIR}/pubkey.asc" ]; then
        HOME="$_home" GNUPGHOME="$_gnupg" gpg --batch --import "${GPG_AGENT_DIR}/pubkey.asc" >/dev/null 2>&1 || true
    fi

    # Point gpg at the forwarded socket. gpgconf reports where THIS gpg expects
    # the agent socket; symlink both that path and the homedir fallback so gpg
    # finds it whether or not a /run/user socketdir exists in this container.
    _sock=$(HOME="$_home" GNUPGHOME="$_gnupg" gpgconf --list-dirs agent-socket 2>/dev/null || true)
    if [ -n "$_sock" ]; then
        mkdir -p "$(dirname "$_sock")" 2>/dev/null || true
        ln -sf "$GPG_EXTRA_SOCKET" "$_sock" 2>/dev/null || true
    fi
    ln -sf "$GPG_EXTRA_SOCKET" "${_gnupg}/S.gpg-agent" 2>/dev/null || true

    _signkey=""
    [ -s "${GPG_AGENT_DIR}/signingkey" ] && _signkey=$(head -n1 "${GPG_AGENT_DIR}/signingkey" 2>/dev/null)
    [ -n "$_signkey" ] || _signkey=$(HOME="$_home" GNUPGHOME="$_gnupg" gpg --list-keys --with-colons 2>/dev/null \
        | awk -F: '$1=="fpr"{print $10; exit}')

    chown -R "${WORKER_UID}:${WORKER_UID}" "$_gnupg" 2>/dev/null || true
    if [ -n "$_signkey" ]; then
        HOME="$_home" git config --global user.signingkey "$_signkey" 2>/dev/null || true
        HOME="$_home" git config --global commit.gpgsign true 2>/dev/null || true
        [ -e "${_home}/.gitconfig" ] && chown "${WORKER_UID}:${WORKER_UID}" "${_home}/.gitconfig" 2>/dev/null || true
        log "remote signing wired: user.signingkey=$_signkey (agent socket forwarded from sidecar)"
    else
        log "WARNING: forwarded gpg socket present but no public key resolved; commit.gpgsign left off"
    fi
}

# ── Phase 1: Network mode + guard proxy ─────────────────────────────────────
# transparent (default when CAP_NET_ADMIN): iptables REDIRECT worker egress to
#   the proxy; exempt the proxy uid, root, loopback, the CCTUI_URL host, DNS,
#   and any WORKER_NET_EXEMPT entries; deny IPv6 egress.
# forward (or no NET_ADMIN): no iptables; export HTTP(S)_PROXY for the worker.
# transparent-external (explicit only): the proxy runs as a separate
#   sidecar container and the iptables REDIRECT was installed by a NET_ADMIN
#   init container (cctui-worker-net-init) — this container needs neither
#   privileged nor NET_ADMIN. Only seed the policy and wait for the sidecar.
# In the two in-container modes start cctui-guard-proxy (uid 1337); in all modes
# seed a deny-default policy that always-allows the CCTUI_URL + REPLY_URL hosts.
NET_MODE=""
phase_network() {
    # Decide the mode. Explicit WORKER_NET_MODE wins; else transparent iff we
    # can actually install iptables rules (a proxy for CAP_NET_ADMIN).
    NET_MODE="${WORKER_NET_MODE:-}"
    if [ -z "$NET_MODE" ]; then
        if iptables -t nat -L >/dev/null 2>&1; then
            NET_MODE=transparent
        else
            NET_MODE=forward
        fi
    fi

    mkdir -p "$(dirname "$POLICY_FILE")"

    # Seed a base deny-default policy: allow the structural hosts (the cctui
    # server + the result callback) plus any WORKER_NET_ALLOW hosts. cctui-guard
    # rewrites this per step when a guarded prompt runs (WORKER_NET_ALLOW is
    # re-applied there via --always-allow so it survives every rewrite).
    #
    # WORKER_NET_ALLOW vs WORKER_NET_EXEMPT: ALLOW routes the host THROUGH the
    # proxy and permits it by SNI (IP-independent — the right tool for CDN /
    # multi-IP hosts like a SaaS API). EXEMPT bypasses the proxy via an iptables
    # RETURN on a single boot-resolved IP — only safe for IP-stable hosts.
    _cctui_hp=$(url_hostport "$CCTUI_BASE_URL")
    _reply_hp=$(url_hostport "${REPLY_URL:-}")
    # Boot-only: the pack host is deliberately NOT --always-allow'ed in
    # phase_guard, so the first per-step rewrite closes it again.
    _pack_hp=$(url_hostport "${CONTEXT_PACK_URL:-}")
    _net_allow=$(printf '%s' "${WORKER_NET_ALLOW:-}" | tr ',' '\n' \
        | sed 's/^[[:space:]]*//;s/[[:space:]]*$//;/^$/d')
    _allowed=$(printf '%s\n%s\n%s\n%s\n' "$_cctui_hp" "$_reply_hp" "$_pack_hp" "$_net_allow" \
        | sed '/^$/d' | jq -R . | jq -cs 'unique')
    printf '{"allowed_hosts":%s,"default":"deny"}\n' "$_allowed" > "$POLICY_FILE"
    log "seeded guard-proxy policy (allow: ${_cctui_hp}${_reply_hp:+, $_reply_hp}${_pack_hp:+, $_pack_hp}${WORKER_NET_ALLOW:+, $WORKER_NET_ALLOW}; default deny)"

    if [ "$NET_MODE" = transparent-external ]; then
        # Nothing to start and no iptables here. The sidecar proxy is deny-all
        # until it hot-reloads the policy seeded above (1s mtime poll), so this
        # wait only avoids boot-time denials — never fail-open.
        _i=0
        while [ "$_i" -lt 60 ]; do
            if curl -fsS "http://127.0.0.1:${PROXY_HEALTH_PORT}/ready" >/dev/null 2>&1; then
                log "external guard-proxy ready (:${PROXY_HEALTH_PORT})"
                break
            fi
            _i=$((_i + 1))
            sleep 0.5
        done
        [ "$_i" -ge 60 ] && log "WARNING: external guard-proxy not ready after 30s (egress stays deny-all)"
        install_guard_ca
        wire_gpg_forwarding
        return 0
    fi

    if [ "$NET_MODE" = transparent ]; then
        if iptables -t nat -L >/dev/null 2>&1; then
            # Exempt root + proxy uid (avoid a redirect loop), loopback.
            iptables -t nat -A OUTPUT -m owner --uid-owner 0 -j RETURN
            iptables -t nat -A OUTPUT -m owner --uid-owner "$PROXY_UID" -j RETURN
            iptables -t nat -A OUTPUT -d 127.0.0.0/8 -j RETURN

            # DNS (TCP fallback; UDP/53 bypasses the TCP-only REDIRECT already).
            _dns=$(awk '/^nameserver/{print $2; exit}' /etc/resolv.conf 2>/dev/null || true)
            if [ -n "$_dns" ]; then
                iptables -t nat -A OUTPUT -d "$_dns" -j RETURN
                log "iptables: RETURN DNS $_dns"
            fi

            # Operator-plane direct exemptions: comma-separated host:port.
            # These bypass the proxy entirely (resolve to IPs and RETURN).
            if [ -n "${WORKER_NET_EXEMPT:-}" ]; then
                _OLDIFS=$IFS; IFS=,
                for _e in $WORKER_NET_EXEMPT; do
                    IFS=$_OLDIFS
                    _h=$(printf '%s' "$_e" | sed 's,:.*$,,')
                    [ -n "$_h" ] || continue
                    # IPv4 only: this is an IPv4 iptables chain, and getent
                    # hosts would return an AAAA record first for dual-stack
                    # public hosts (e.g. automation.example.internal), which iptables rejects.
                    _ip=$(getent ahostsv4 "$_h" 2>/dev/null | awk '{print $1; exit}' || true)
                    if [ -n "$_ip" ]; then
                        # Don't let one unreachable exempt host (set -e) abort
                        # the whole guard setup — log and carry on.
                        if iptables -t nat -A OUTPUT -d "$_ip" -j RETURN 2>/dev/null; then
                            log "iptables: RETURN exempt $_h ($_ip)"
                        else
                            log "WARNING: iptables exempt rule failed for $_h ($_ip)"
                        fi
                    else
                        log "WARNING: could not resolve WORKER_NET_EXEMPT host $_h"
                    fi
                    IFS=,
                done
                IFS=$_OLDIFS
            fi

            # Everything else from the worker: REDIRECT into the proxy.
            iptables -t nat -A OUTPUT -p tcp -m owner --uid-owner "$WORKER_UID" \
                -j REDIRECT --to-port "$PROXY_PORT"
            log "iptables: worker egress (uid $WORKER_UID) -> :$PROXY_PORT"

            # Deny IPv6 egress: the proxy + REDIRECT are IPv4-only
            # (SO_ORIGINAL_DST), so any IPv6 egress would bypass the filter.
            if ip6tables -L >/dev/null 2>&1; then
                ip6tables -A OUTPUT -o lo -j ACCEPT
                ip6tables -A OUTPUT -j REJECT
                log "ip6tables: IPv6 egress denied (forces IPv4 fallback)"
            else
                log "WARNING: ip6tables unavailable — IPv6 egress is UNFILTERED"
            fi
        else
            log "WARNING: transparent requested but iptables unavailable; falling back to forward"
            NET_MODE=forward
        fi
    fi

    if [ "$NET_MODE" = forward ]; then
        # No iptables: clients must honor the proxy env. Export for the dropped
        # worker tree (the supervisor inherits this env into the daemon).
        export HTTP_PROXY="http://127.0.0.1:${PROXY_PORT}"
        export HTTPS_PROXY="http://127.0.0.1:${PROXY_PORT}"
        export http_proxy="$HTTP_PROXY"
        export https_proxy="$HTTPS_PROXY"
        # Never proxy loopback / the daemon's own control socket.
        export NO_PROXY="127.0.0.1,localhost"
        export no_proxy="$NO_PROXY"
        log "forward mode: HTTP(S)_PROXY -> 127.0.0.1:${PROXY_PORT}"
    fi

    # Start the proxy as uid 1337 (a uid the worker can't write as). setpriv is
    # part of util-linux (present on the base); fall back to running as root if
    # it's missing (still functional, just less isolated).
    if command -v setpriv >/dev/null 2>&1; then
        setpriv --reuid="$PROXY_UID" --regid="$PROXY_UID" --clear-groups \
            cctui-guard-proxy --mode "$NET_MODE" \
                --listen "0.0.0.0:${PROXY_PORT}" \
                --health-listen "0.0.0.0:${PROXY_HEALTH_PORT}" \
                --policy "$POLICY_FILE" &
    else
        cctui-guard-proxy --mode "$NET_MODE" \
            --listen "0.0.0.0:${PROXY_PORT}" \
            --health-listen "0.0.0.0:${PROXY_HEALTH_PORT}" \
            --policy "$POLICY_FILE" &
    fi
    GUARD_PROXY_PID=$!

    # Wait for readiness (policy loaded). Best-effort: don't block boot forever.
    _i=0
    while [ "$_i" -lt 20 ]; do
        if curl -fsS "http://127.0.0.1:${PROXY_HEALTH_PORT}/ready" >/dev/null 2>&1; then
            break
        fi
        _i=$((_i + 1))
        sleep 0.25
    done
    log "guard-proxy started (mode=$NET_MODE, pid $GUARD_PROXY_PID)"
}

# $1 = repo dir, $2 = fetch URL (may embed a token; falls back to origin)
ensure_base_branch() {
    _bd="$1"
    _burl="${2:-origin}"
    if [ "$(git -C "$_bd" rev-parse --is-shallow-repository 2>/dev/null)" = "true" ]; then
        git -C "$_bd" fetch -q "$_burl" --unshallow 2>/dev/null \
            || git -C "$_bd" fetch -q "$_burl" 2>/dev/null || true
    fi
    _def=$(git -C "$_bd" symbolic-ref --short refs/remotes/origin/HEAD 2>/dev/null | sed 's,^origin/,,')
    [ -n "$_def" ] || _def=$(git -C "$_bd" ls-remote --symref "$_burl" HEAD 2>/dev/null \
        | awk '$1 == "ref:" { sub("refs/heads/", "", $2); print $2; exit }')
    [ -n "$_def" ] || return 0
    git -C "$_bd" fetch -q "$_burl" "+refs/heads/${_def}:refs/remotes/origin/${_def}" 2>/dev/null || true
    git -C "$_bd" symbolic-ref refs/remotes/origin/HEAD "refs/remotes/origin/${_def}" 2>/dev/null || true
    git -C "$_bd" branch -f "$_def" "refs/remotes/origin/${_def}" 2>/dev/null \
        && log "workspace: base branch ${_def} @ $(git -C "$_bd" rev-parse --short "refs/remotes/origin/${_def}" 2>/dev/null)" \
        || true
}

# ── Phase 2: Workspace ──────────────────────────────────────────────────────
# WARM_REPO_DIR -> overlayfs (rsync fallback); else TASK_REPO_URL -> blobless
# clone; else an empty /workspace. chown to the worker either way. Skipped (just
# an empty, worker-owned /workspace) when neither var is set.
phase_workspace() {
    mkdir -p /workspace
    _ws_overlay=0
    # Baked warm image (warm.Dockerfile): /workspace already holds the repos as
    # ordinary image-layer files owned by the worker — no overlay, no clone.
    # Just fetch + check out the requested ref in the baked checkout. Runs as
    # the worker uid so no root-owned files land in the repo.
    if [ -f /workspace/.warm-baked ] && [ -n "${TASK_REPO:-}" ] \
            && [ -d "/workspace/${TASK_REPO}/.git" ]; then
        log "workspace: baked warm image ($(cat /workspace/.warm-baked)); using /workspace/${TASK_REPO} in place"
        if [ -n "${TASK_REPO_REF:-}" ]; then
            _wtok="${GITHUB_TOKEN:-}"
            if [ -z "$_wtok" ] && [ -n "${TASK_IDENTITY:-}" ]; then
                _wid=$(printf '%s' "$TASK_IDENTITY" | tr '[:lower:]' '[:upper:]' | tr -c 'A-Z0-9' '_')
                _wid=${_wid%_}
                eval "_wtok=\${GITHUB_TOKEN_${_wid}:-}"
            fi
            _wurl=$(git -C "/workspace/${TASK_REPO}" remote get-url origin 2>/dev/null || echo "")
            [ -n "$_wtok" ] && [ -n "$_wurl" ] \
                && _wurl=$(printf '%s' "$_wurl" | sed "s,^https://,https://${_wtok}@,")
            if git -C "/workspace/${TASK_REPO}" fetch -q "${_wurl:-origin}" "$TASK_REPO_REF" 2>/dev/null \
                    && git -C "/workspace/${TASK_REPO}" checkout -q FETCH_HEAD 2>/dev/null; then
                log "workspace: checked out TASK_REPO_REF=$TASK_REPO_REF"
            else
                log "WARNING: could not check out TASK_REPO_REF=$TASK_REPO_REF (baked HEAD left as-is)"
            fi
            ensure_base_branch "/workspace/${TASK_REPO}" "${_wurl:-origin}"
        fi
        return 0
    fi
    # Warm cache only when it actually holds this repo (WARM_REPO_DIR/<repo>);
    # an empty/missing cache must fall through to a clone, not overlay nothing.
    if [ -n "${WARM_REPO_DIR:-}" ] && [ -n "${TASK_REPO:-}" ] \
            && [ -d "${WARM_REPO_DIR%/}/${TASK_REPO}" ]; then
        mkdir -p /overlay/upper /overlay/work
        if mount -t overlay overlay \
                -o "lowerdir=${WARM_REPO_DIR},upperdir=/overlay/upper,workdir=/overlay/work" \
                /workspace 2>/dev/null; then
            log "workspace: overlayfs on WARM_REPO_DIR=$WARM_REPO_DIR"
            _ws_overlay=1
        else
            log "workspace: overlayfs unavailable, rsync-copying WARM_REPO_DIR"
            rsync -a "${WARM_REPO_DIR%/}/" /workspace/
        fi
    elif [ -n "${TASK_REPO_URL:-}" ]; then
        # On-demand clone into /workspace/<repo> (matches the warm-overlay layout
        # the prompts expect at /workspace/${TASK_REPO}). Private repos need a
        # token; creds aren't materialized yet at this phase, so resolve the
        # identity token from the pod env and inject it into the clone URL, then
        # scrub it from the persisted remote.
        _dest="/workspace/${TASK_REPO:-repo}"
        mkdir -p "$_dest"
        _wtok="${GITHUB_TOKEN:-}"
        if [ -z "$_wtok" ] && [ -n "${TASK_IDENTITY:-}" ]; then
            _wid=$(printf '%s' "$TASK_IDENTITY" | tr '[:lower:]' '[:upper:]' | tr -c 'A-Z0-9' '_')
            _wid=${_wid%_}
            eval "_wtok=\${GITHUB_TOKEN_${_wid}:-}"
        fi
        _curl="$TASK_REPO_URL"
        [ -n "$_wtok" ] && _curl=$(printf '%s' "$TASK_REPO_URL" | sed "s,^https://,https://${_wtok}@,")
        if git clone --filter=blob:none "$_curl" "$_dest" 2>/dev/null; then
            if [ -n "${TASK_REPO_REF:-}" ]; then
                git -C "$_dest" fetch -q "$_curl" "$TASK_REPO_REF" 2>/dev/null \
                    && git -C "$_dest" checkout -q FETCH_HEAD 2>/dev/null \
                    || log "WARNING: could not check out TASK_REPO_REF=$TASK_REPO_REF"
            fi
            ensure_base_branch "$_dest" "$_curl"
            git -C "$_dest" remote set-url origin "$TASK_REPO_URL" 2>/dev/null || true
            log "workspace: cloned ${TASK_REPO} into $_dest${TASK_REPO_REF:+ @ ${TASK_REPO_REF}}"
        else
            log "WARNING: TASK_REPO_URL clone failed; /workspace left empty"
        fi
    else
        log "workspace: empty /workspace (no warm cache for ${TASK_REPO:-?} / no TASK_REPO_URL)"
    fi
    # Never recursively chown an overlayfs /workspace: it recurses the read-only
    # lowerdir (the whole WARM_REPO_DIR warm cache) and forces a copy-up of every
    # file into the upperdir, wedging boot in NFS I/O over a multi-repo cache
    #. The lowerdir is already worker-readable and writes copy-up into
    # /overlay/upper (chowned below), so the overlay needs no recursive chown —
    # only the root-created fallbacks (rsync/clone/empty) do.
    if [ "$_ws_overlay" = 0 ]; then
        chown -R "${WORKER_UID}:${WORKER_UID}" /workspace 2>/dev/null || true
    fi
    [ -d /overlay/upper ] && chown -R "${WORKER_UID}:${WORKER_UID}" /overlay/upper 2>/dev/null || true
}

# ── Phase 3: Context pack ───────────────────────────────────────────────────
# Operator-plane: CONTEXT_PACK_URL/REF (+ optional TOKEN, SUBDIR). git clone
# --depth 1 the pinned ref into /opt/context (root-side, pre-lockdown), then
# wire its CLAUDE.md / guard-rules.md and the dirs its pack.toml [dirs] table
# declares (falling back to the v1 set: skills/rules/docs/style/projects) into
# the locations the agent expects. FAIL-CLOSED: when CONTEXT_PACK_URL is set the
# fetch MUST succeed (the pack defines the guard rules). Skipped entirely when
# CONTEXT_PACK_URL is unset.
#
# Precedence: the pod template (true operator plane) wins. Only when a
# CONTEXT_PACK_* var is NOT already set in the pod env do we fall back to the
# dispatch payload's `env` map (TASK_PAYLOAD_JSON.env, the operator-controlled
# automation dispatcher) — letting a flow select its pack without baking it into the
# template, while a template that pins the pack still overrides the payload.
# [dirs] table from the merged pack manifest ($CONTEXT_DIR/pack.toml), one
# `key srcdir` pair per line. Empty when the pack ships no manifest / no [dirs].
pack_dirs() {
    [ -f "$CONTEXT_DIR/pack.toml" ] || return 0
    sed -n '/^\[dirs\]/,/^\[/s/^[[:space:]]*\([A-Za-z0-9_-][A-Za-z0-9_-]*\)[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1 \2/p' \
        "$CONTEXT_DIR/pack.toml"
}

# Source dir (relative to $CONTEXT_DIR) declared for a [dirs] key in $_dirs;
# empty when the key is not declared.
pack_dir_src() {
    printf '%s\n' "$_dirs" | while read -r _k _s; do
        [ "$_k" = "$1" ] || continue
        printf '%s' "${_s:-$1}"
        break
    done
}

# Home-relative wiring target for a [dirs] key. rules/ = always-on guidance
# (Claude Code auto-loads each *.md); docs/ = on-demand
# reference pulled by path (@~/.claude/docs/<x>.md), not auto-loaded; hooks/ =
# PreToolUse scripts (chmod +x here, registered in phase_permissions). prompts/
# and scripts/ stay in /opt/context (TASK_PROMPT_FILE / absolute paths resolve
# there); projects/ is consumed by pack_wire_instructions, not copied to the
# home. Unknown keys land under ~/.claude/<key> — a per-pod emptyDir, so a new
# pack dir never writes to the NFS-shared home root.
pack_dir_target() {
    case "$1" in
        skills)          printf '.claude/skills' ;;
        rules)           printf '.claude/rules' ;;
        docs)            printf '.claude/docs' ;;
        hooks)           printf '.claude/hooks' ;;
        style)           printf 'style' ;;
        prompts|scripts|projects) ;;
        *)               printf '.claude/%s' "$1" ;;
    esac
}

# Stage the pack's always-on instructions into /workspace, the PARENT of the
# checkout at /workspace/${TASK_REPO}. Both harnesses resolve instructions by
# walking UP from the cwd, and $HOME is not on that path — ~/CLAUDE.md and
# ~/projects/<repo>/CLAUDE.md are never read. Do not "restore" this to $HOME.
#
# One level above the checkout is also the nearest read slot OUTSIDE the repo:
# the repo's own committed AGENTS.md/CLAUDE.md stays deeper in the ancestry, so
# it still loads and wins on conflict, and nothing lands in `git status`.
#
# AGENTS.md is the source (Codex reads it natively); CLAUDE.md beside it is the
# pack's one-line @AGENTS.md import. A pack predating the split ships only
# CLAUDE.md — use it for both.
pack_wire_instructions() {
    _ws="${PACK_WORKSPACE_DIR:-/workspace}"
    [ -d "$_ws" ] || return 0
    _src=""
    [ -f "$CONTEXT_DIR/AGENTS.md" ] && _src="$CONTEXT_DIR/AGENTS.md"
    [ -z "$_src" ] && [ -f "$CONTEXT_DIR/CLAUDE.md" ] && _src="$CONTEXT_DIR/CLAUDE.md"
    [ -n "$_src" ] || return 0

    cp -f "$_src" "$_ws/AGENTS.md" || return 0

    _proj=$(pack_dir_src projects)
    if [ -n "${TASK_REPO:-}" ] && [ -n "$_proj" ] \
            && [ -d "$CONTEXT_DIR/$_proj/$TASK_REPO" ]; then
        for _pf in "$CONTEXT_DIR/$_proj/$TASK_REPO"/*.md; do
            [ -f "$_pf" ] || continue
            printf '\n' >> "$_ws/AGENTS.md"
            cat "$_pf" >> "$_ws/AGENTS.md"
        done
        log "context pack: appended ${_proj}/${TASK_REPO} to ${_ws}/AGENTS.md"
    fi

    if [ -f "$CONTEXT_DIR/CLAUDE.md" ] && [ "$_src" != "$CONTEXT_DIR/CLAUDE.md" ]; then
        cp -f "$CONTEXT_DIR/CLAUDE.md" "$_ws/CLAUDE.md"
    else
        printf '@AGENTS.md\n' > "$_ws/CLAUDE.md"
    fi

    chown "${WORKER_UID}:${WORKER_UID}" "$_ws/AGENTS.md" "$_ws/CLAUDE.md" 2>/dev/null || true
    log "context pack: wired AGENTS.md + CLAUDE.md -> ${_ws}/"
}

phase_context_pack() {
    if [ -n "${TASK_PAYLOAD_JSON:-}" ]; then
        for _k in CONTEXT_PACK_URL CONTEXT_PACK_REF CONTEXT_PACK_TOKEN CONTEXT_PACK_SUBDIR; do
            eval "_cur=\${$_k:-}"
            [ -n "$_cur" ] && continue   # pod-template value wins
            _v=$(printf '%s' "$TASK_PAYLOAD_JSON" | jq -r --arg k "$_k" '.env[$k] // empty' 2>/dev/null || true)
            [ -n "$_v" ] && export "$_k=$_v"
        done
        # Single-token model: one GITHUB_TOKEN pulls the pack AND (via the daemon
        # applying it to the session) clones/pushes the work repo, so the tenant
        # ships exactly one credential. If no dedicated CONTEXT_PACK_TOKEN was
        # given, resolve a GitHub token for the pack clone, in priority order:
        #   1. payload.env.GITHUB_TOKEN — explicit override (tests / ad-hoc).
        #   2. GITHUB_TOKEN_<IDENTITY> from the pod env — the per-identity secret
        #      Vault injects operator-side (identity-as-root; the job carries only
        #      the `identity` selector, never the secret). Same source the
        #      credentials helper materializes. A future dispatcher-side secret
        #      broker can populate this var without changing the job contract.
        # Operator-named source for the pack-clone token: CONTEXT_PACK_TOKEN_FROM
        # holds the NAME of an env var holding a token that can read the pack repo
        # (e.g. a privileged identity), set in the worker template. The
        # indirection keeps any specific identity name out of this image while
        # letting the operator point the pack clone at a different credential than
        # the task identity (whose token may not have pack-repo access).
        if [ -z "${CONTEXT_PACK_TOKEN:-}" ] && [ -n "${CONTEXT_PACK_TOKEN_FROM:-}" ]; then
            case "$CONTEXT_PACK_TOKEN_FROM" in
                [A-Za-z_][A-Za-z0-9_]*)
                    eval "_v=\${${CONTEXT_PACK_TOKEN_FROM}:-}"
                    [ -n "${_v:-}" ] && export CONTEXT_PACK_TOKEN="$_v" ;;
            esac
        fi
        if [ -z "${CONTEXT_PACK_TOKEN:-}" ]; then
            # Primary: GITHUB_TOKEN already in the pod env — the dispatcher
            # promotes payload.env to pod env and the vault-env webhook resolves
            # any vault: reference before this entrypoint runs, so by here it is a
            # real token. Fallbacks cover non-promoting callers.
            _gh="${GITHUB_TOKEN:-}"
            [ -z "$_gh" ] && _gh=$(printf '%s' "$TASK_PAYLOAD_JSON" | jq -r '.env.GITHUB_TOKEN // empty' 2>/dev/null || true)
            if [ -z "$_gh" ]; then
                _id=$(printf '%s' "$TASK_PAYLOAD_JSON" | jq -r '.identity // empty' 2>/dev/null || true)
                if [ -n "$_id" ]; then
                    # GITHUB_TOKEN_<ID> with ID upper-cased and non-alnum -> _
                    _idv=$(printf '%s' "$_id" | tr '[:lower:]' '[:upper:]' | tr -c 'A-Z0-9' '_')
                    _idv=${_idv%_}
                    eval "_gh=\${GITHUB_TOKEN_${_idv}:-}"
                fi
            fi
            [ -n "$_gh" ] && export CONTEXT_PACK_TOKEN="$_gh"
        fi
    fi
    [ -n "${CONTEXT_PACK_URL:-}" ] || { log "context pack: CONTEXT_PACK_URL unset, skipping"; return 0; }

    # The URL may carry an optional `@<ref>` (in the path) and `#<subdir>`
    # fragment, so a single CONTEXT_PACK_URL can pin ref + subdir; explicit
    # CONTEXT_PACK_REF / CONTEXT_PACK_SUBDIR still take precedence. REF is
    # OPTIONAL — absent ⇒ the remote's default branch (pin it in prod for
    # reproducibility). The `@` is only split out of the path, never the
    # authority, so an embedded `user@host` credential is left intact.
    _raw="$CONTEXT_PACK_URL"
    case "$_raw" in
        *\#*) _frag="${_raw##*#}"; _raw="${_raw%%#*}"
              [ -z "${CONTEXT_PACK_SUBDIR:-}" ] && [ -n "$_frag" ] && CONTEXT_PACK_SUBDIR="$_frag" ;;
    esac
    _scheme=""; _rest="$_raw"
    case "$_raw" in *://*) _scheme="${_raw%%://*}://"; _rest="${_raw#*://}" ;; esac
    case "$_rest" in
        */*) _auth="${_rest%%/*}"; _path="/${_rest#*/}" ;;
        *)   _auth="$_rest"; _path="" ;;
    esac
    case "$_path" in
        *@*) [ -z "${CONTEXT_PACK_REF:-}" ] && CONTEXT_PACK_REF="${_path##*@}"; _path="${_path%@*}" ;;
    esac
    _url="${_scheme}${_auth}${_path}"

    if [ -n "${CONTEXT_PACK_TOKEN:-}" ]; then
        # Inject an HTTPS basic token (https://<token>@host/...). Never logged.
        _url=$(printf '%s' "$_url" | sed "s,^https://,https://${CONTEXT_PACK_TOKEN}@,")
    fi

    _tmp=$(mktemp -d)
    _giterr=$(mktemp)
    # Root egress bypasses the uid-scoped redirect — only WORKER_UID traffic
    # reaches the proxy's credential injection.
    if [ "$(id -u)" = "0" ] && command -v setpriv >/dev/null 2>&1; then
        chown "${WORKER_UID}:${WORKER_UID}" "$_tmp"
        pack_git() {
            setpriv --reuid="$WORKER_UID" --regid="$WORKER_UID" --clear-groups \
                env HOME="$_tmp" GIT_TERMINAL_PROMPT=0 git "$@" 2>>"$_giterr"
        }
    else
        pack_git() { GIT_TERMINAL_PROMPT=0 git "$@" 2>>"$_giterr"; }
    fi
    pack_fatal() {
        echo "cctui-worker: FATAL $1" >&2
        sed 's/^/cctui-worker: git: /' "$_giterr" | tail -5 >&2
        exit 1
    }
    if [ -z "${CONTEXT_PACK_REF:-}" ]; then
        # No ref ⇒ clone the remote's default branch.
        pack_git clone --depth 1 "$_url" "$_tmp" \
            || pack_fatal "context-pack clone failed (default branch)"
    elif ! pack_git clone --depth 1 --branch "$CONTEXT_PACK_REF" "$_url" "$_tmp"; then
        # Some refs are SHAs (not clonable by --branch): fetch the ref directly.
        rm -rf "$_tmp"; _tmp=$(mktemp -d)
        [ "$(id -u)" = "0" ] && chown "${WORKER_UID}:${WORKER_UID}" "$_tmp"
        { pack_git -C "$_tmp" init -q \
            && pack_git -C "$_tmp" remote add origin "$_url" \
            && pack_git -C "$_tmp" fetch --depth 1 -q origin "$CONTEXT_PACK_REF" \
            && pack_git -C "$_tmp" checkout -q FETCH_HEAD; } \
            || pack_fatal "context-pack fetch failed (CONTEXT_PACK_URL set, ref=$CONTEXT_PACK_REF)"
    fi
    rm -f "$_giterr"

    _src="$_tmp"
    [ -n "${CONTEXT_PACK_SUBDIR:-}" ] && _src="$_tmp/${CONTEXT_PACK_SUBDIR#/}"
    if [ ! -d "$_src" ]; then
        echo "cctui-worker: FATAL context-pack subdir not found: ${CONTEXT_PACK_SUBDIR}" >&2
        exit 1
    fi

    # Optional shared base layer merged UNDER the selected pack. A monorepo of
    # packs keeps universal material (home CLAUDE.md, guard-rules.md, universal
    # rules) once in a `_base` dir; each pack subdir overlays its own files on
    # top. The subdir's pack.toml declares its location via `base = "../_base"`
    # (path relative to the subdir); absent that we fall back to a repo-root
    # `_base` when a subdir is selected. Copied FIRST so the pack wins on
    # conflict (cp -a "$X/." merges trees, later copy overwrites same-named files).
    _base_dir=""
    if [ -n "${CONTEXT_PACK_SUBDIR:-}" ]; then
        _base_rel=""
        [ -f "$_src/pack.toml" ] && _base_rel=$(sed -n 's/^[[:space:]]*base[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$_src/pack.toml" | head -n1)
        if [ -n "$_base_rel" ]; then
            # Resolve relative to the subdir, then confine to the clone tree so a
            # crafted pack.toml can't escape $_tmp (e.g. base = "../../etc").
            _base_dir=$(cd "$_src/$(dirname "$_base_rel")" 2>/dev/null && pwd)/$(basename "$_base_rel")
            case "$_base_dir" in "$_tmp"/*|"$_tmp") : ;; *) _base_dir="" ;; esac
        elif [ -d "$_tmp/_base" ]; then
            _base_dir="$_tmp/_base"
        fi
    fi

    rm -rf "$CONTEXT_DIR"
    mkdir -p "$CONTEXT_DIR"
    # Base layer first (if any), then the pack overlays on top; both drop .git.
    if [ -n "$_base_dir" ] && [ -d "$_base_dir" ]; then
        rm -rf "$_base_dir/.git"
        cp -a "$_base_dir/." "$CONTEXT_DIR/" 2>/dev/null || true
        log "context pack: merged base layer from ${_base_rel:-_base}"
    fi
    # Copy the pack contents (drop .git) into the read-only context dir.
    rm -rf "$_src/.git"
    cp -a "$_src/." "$CONTEXT_DIR/" 2>/dev/null || true
    rm -rf "$_tmp"

    # Wire the pack into the locations the agent expects. Copies (not symlinks)
    # so landlock RO on /opt/context covers them.
    _home="/home/${WORKER_USER}"

    # The pack manifest's [dirs] table is authoritative when present:
    # `key = "srcdir"` lines declare which pack dirs get wired into the home, so
    # a pack can add a new dir (e.g. hooks/) without an entrypoint change. Absent
    # a manifest/table we fall back to the v1 hardcoded set below.
    _dirs=$(pack_dirs)
    [ -n "$_dirs" ] || _dirs="skills skills
rules rules
docs docs
style style
projects projects"

    # Per-pod isolation of the home paths the pack overwrites. /home/worker is a
    # ReadWriteMany NFS volume shared across concurrent workers, so writing the
    # pack's style dir straight onto it would race-corrupt other in-flight
    # dispatches. When a pack is active we bind an empty per-pod dir (under the
    # /overlay emptyDir) over it, so the pack's writes are private to this pod
    # and the shared NFS copy is untouched. (~/.claude is already a per-pod
    # emptyDir, so skills/rules/docs need no isolation.) Best-effort: if /overlay
    # or mount is unavailable we fall through to direct copy.
    if [ -d /overlay ]; then
        _iso="/overlay/pack-home"
        mkdir -p "$_iso"
        for _p in style; do
            _s=$(pack_dir_src "$_p")
            if [ -n "$_s" ] && [ -d "$CONTEXT_DIR/$_s" ]; then
                mkdir -p "$_iso/$_p" "${_home}/$_p"
                mount --bind "$_iso/$_p" "${_home}/$_p" 2>/dev/null || true
            fi
        done
    fi

    pack_wire_instructions
    printf '%s\n' "$_dirs" | while read -r _key _srcd; do
        [ -n "$_key" ] || continue
        [ -n "$_srcd" ] || _srcd="$_key"
        # Confine the declared source to the pack tree: a crafted pack.toml must
        # not pull from outside $CONTEXT_DIR.
        case "$_srcd" in
            /*|*..*) log "WARNING: context pack: ignoring unsafe [dirs] path ${_key}=\"${_srcd}\""; continue ;;
        esac
        _tgt=$(pack_dir_target "$_key")
        [ -n "$_tgt" ] || continue
        [ -d "$CONTEXT_DIR/$_srcd" ] || continue
        mkdir -p "${_home}/${_tgt}"
        cp -a "$CONTEXT_DIR/$_srcd/." "${_home}/${_tgt}/" 2>/dev/null || true
        [ "$_key" = hooks ] && chmod +x "${_home}/${_tgt}"/*.sh 2>/dev/null || true
        chown -R "${WORKER_UID}:${WORKER_UID}" "${_home}/${_tgt}" 2>/dev/null || true
        log "context pack: wired ${_srcd}/ -> ~/${_tgt}"
    done
    # Merge (not overwrite): a credential helper may already own ~/.mcp.json.
    if [ -f "$CONTEXT_DIR/mcp.json" ] && ! adapter_is_codex; then
        _mcpf="${_home}/.mcp.json"
        _t=$(mktemp)
        if [ -f "$_mcpf" ]; then
            jq -s '.[0] * {mcpServers: ((.[0].mcpServers // {}) + (.[1].mcpServers // {}))}' \
                "$_mcpf" "$CONTEXT_DIR/mcp.json" > "$_t" 2>/dev/null \
                && mv "$_t" "$_mcpf" || rm -f "$_t"
        else
            jq '{mcpServers: (.mcpServers // {})}' "$CONTEXT_DIR/mcp.json" > "$_t" 2>/dev/null \
                && mv "$_t" "$_mcpf" || rm -f "$_t"
        fi
        [ -f "$_mcpf" ] && chown "${WORKER_UID}:${WORKER_UID}" "$_mcpf" 2>/dev/null || true
        log "context pack: merged mcp.json -> ${_mcpf}"
    fi

    # /opt/context stays root-owned + RO under landlock, but must be world-
    # READABLE/traversable: the dropped-privilege daemon (worker uid) reads the
    # prompt + guard-rules from here. mkdir inherits root's umask (077 -> 700),
    # which blocks the worker from traversing it, so force a+rX.
    chown -R 0:0 "$CONTEXT_DIR" 2>/dev/null || true
    chmod -R a+rX "$CONTEXT_DIR" 2>/dev/null || true

    # Resolve the guard-rules seam. The pack's guard-rules.md is the override/
    # extend layer; any operator-provided GUARD_RULES_FILE becomes the base
    # (GUARD_RULES_BASE), which cctui-guard parses first so the pack can reuse a
    # common set (net-dev, …), extend it (`[name]+:`), or override it (`[name]:`).
    if [ -f "$CONTEXT_DIR/guard-rules.md" ]; then
        if [ -n "${GUARD_RULES_FILE:-}" ] && [ -z "${GUARD_RULES_BASE:-}" ]; then
            export GUARD_RULES_BASE="$GUARD_RULES_FILE"
        fi
        export GUARD_RULES_FILE="$CONTEXT_DIR/guard-rules.md"
    fi
    log "context pack: fetched ref=${CONTEXT_PACK_REF:-<default-branch>} into $CONTEXT_DIR"
}

# Resolve the dispatched prompt file path. TASK_PROMPT_FILE resolves under the
# context pack's prompts/ dir; an absolute path is honored as-is. Echoes the
# resolved path (empty if none).
resolve_prompt_path() {
    [ -n "${TASK_PROMPT_FILE:-}" ] || return 0
    case "$TASK_PROMPT_FILE" in
        /*) printf '%s' "$TASK_PROMPT_FILE" ;;
        *)  printf '%s/prompts/%s' "$CONTEXT_DIR" "$TASK_PROMPT_FILE" ;;
    esac
}

# ── Phase 4: Extension hooks ────────────────────────────────────────────────
phase_extensions() {
    [ -d /opt/worker-entrypoint.d ] || return 0
    for _ext in /opt/worker-entrypoint.d/*.sh; do
        [ -e "$_ext" ] || continue
        log "ext: sourcing $(basename "$_ext")"
        . "$_ext" || log "WARNING: extension ${_ext} failed (continuing)"
    done
}

# ── Phase 4b: Codex model provider ──────────────────────────────────────────
# The platform injects OPENAI_API_KEY + OPENAI_BASE_URL (the cctui openai
# gateway) into the agent env, but the `codex` CLI IGNORES those
# vars — it reads its model provider only from ~/.codex/config.toml. The pod's
# config.toml has just `trust_level = "trusted"` (codex writes that itself), so
# codex's default provider connects straight to api.openai.com with no bearer →
# `401 Unauthorized (Missing bearer)`, and the dual-reviewer review-pr flow
# silently degrades to Claude-only.
#
# When OPENAI_API_KEY is set, MERGE a `[model_providers.cctui]` block + a
# `model_provider = "cctui"` selector into config.toml, pointing codex's
# `responses` wire transport at OPENAI_BASE_URL and reading the bearer from the
# OPENAI_API_KEY env var (codex DOES read env_key from env at request time).
#
# We ALSO pin standard service tier + disable fast mode (CCT: codex spend).
# Codex "fast mode" is a 1.5x speed lever that bills subscription credits at
# 2-2.5x the standard rate (same model, same quality) and can persist via
# `service_tier = "fast"` + `[features].fast_mode = true`. Unattended dispatched
# workers must never silently run on the expensive tier, so we force the inverse
# (`service_tier = "default"`, `fast_mode = false`) into the managed region.
#
# The block is delimited by BEGIN/END markers and rewritten in place, so this
# is idempotent (safe to re-run) and preserves codex's own keys (trust_level).
# No TOML-aware tool ships in the worker image (jq is JSON-only, no python3), so
# the merge is plain shell: drop any prior cctui-managed region + a stray
# top-level `model_provider`, then append a fresh region. Skipped silently when
# OPENAI_API_KEY is unset.
CODEX_MARKER_BEGIN="# >>> cctui codex model_provider >>>"
CODEX_MARKER_END="# <<< cctui codex model_provider <<<"

# Server names must be TOML-bare-key safe (emitted unquoted in the table header).
codex_mcp_toml() {
    [ -f "$1" ] || return 0
    jq -r '
      (.mcpServers // {}) | to_entries[] |
      "[mcp_servers." + .key + "]",
      (if .value.command then "command = " + (.value.command|@json) else empty end),
      (if (.value.args|type)=="array" then "args = " + (.value.args|tojson) else empty end),
      (if (.value.env|type)=="object" then "env = { " + ((.value.env|to_entries|map((.key|@json)+" = "+(.value|@json))|join(", "))) + " }" else empty end),
      (if .value.url then "url = " + (.value.url|@json) else empty end),
      (if .value.bearer_token_env_var then "bearer_token_env_var = " + (.value.bearer_token_env_var|@json) else empty end),
      ""
    ' "$1" 2>/dev/null || true
}

phase_codex_config() {
    [ -n "${OPENAI_API_KEY:-}" ] || { log "codex: OPENAI_API_KEY unset, skipping model_provider"; return 0; }
    _base="${OPENAI_BASE_URL:-}"
    if [ -z "$_base" ]; then
        log "WARNING: codex: OPENAI_API_KEY set but OPENAI_BASE_URL empty; skipping model_provider"
        return 0
    fi
    _cfgdir="${CODEX_HOME:-/home/${WORKER_USER}/.codex}"
    _cfg="$_cfgdir/config.toml"
    mkdir -p "$_cfgdir"

    # Model + reasoning effort, pinned per-pod so one-off `codex exec` needs no
    # --model/-c flags — config.toml owns them. Model names are BARE (no `-codex` suffix); default is the gpt-5.6-sol frontier.
    # Overridden only by TASK_CODEX_MODEL — NEVER by TASK_MODEL, which is the
    # primary (Claude) agent's model (e.g. `opus`); feeding that to codex writes
    # an OpenAI-invalid model the gateway rejects.
    _model="${TASK_CODEX_MODEL:-gpt-5.6-sol}"
    case "${TASK_EFFORT:-medium}" in
        low)          _effort=low ;;
        medium)       _effort=medium ;;
        high)         _effort=high ;;
        xhigh)        _effort=xhigh ;;
        max)          _effort=max ;;
        ultra)        _effort=ultra ;;
        *)            _effort=medium ;;
    esac

    # Preserve any existing config MINUS our managed region and the top-level
    # keys we re-set. awk drops the marker-delimited block; the trailing grep
    # removes bare re-set keys outside the block so a re-run can't leave a
    # duplicate TOML key.
    _kept=""
    if [ -f "$_cfg" ]; then
        _kept=$(awk -v b="$CODEX_MARKER_BEGIN" -v e="$CODEX_MARKER_END" '
            $0==b {skip=1; next} $0==e {skip=0; next} skip{next} {print}
        ' "$_cfg" | grep -vE '^[[:space:]]*(model_provider|service_tier|model|model_reasoning_effort|approval_policy|sandbox_mode)[[:space:]]*=' || true)
    fi

    {
        [ -n "$_kept" ] && printf '%s\n' "$_kept"
        printf '%s\n' "$CODEX_MARKER_BEGIN"
        # Per-account settings first, so the managed keys below win on a
        # duplicate. Rendered server-side by cctui_proto::codex_config (the same
        # block the daemon turns into `-c` flags) — dotted assignments only,
        # never a [table] header, which would capture the bare keys that follow.
        if [ -n "${CCTUI_CODEX_CONFIG_TOML:-}" ]; then
            printf '%s\n' "$CCTUI_CODEX_CONFIG_TOML"
        fi
        printf 'model_provider = "cctui"\n'
        # Standard tier, fast mode off — never bill credits at the 2-2.5x fast rate.
        printf 'service_tier = "default"\n'
        # Model + effort pinned per-pod (from TASK_MODEL / TASK_EFFORT above).
        printf 'model = "%s"\n' "$_model"
        printf 'model_reasoning_effort = "%s"\n' "$_effort"
        # Approvals off + full access: the pod is ALREADY a hardened sandbox
        # (Landlock+seccomp+guard-proxy), and codex's inner bubblewrap sandbox is
        # blocked by our seccomp filter — so a codex sandbox makes every read
        # fail. This is the config.toml equivalent of the yolo CLI flag; it lets
        # `codex exec` run with no approval/sandbox flags.
        # (`--skip-git-repo-check` has NO config equivalent and stays on the CLI.)
        printf 'approval_policy = "never"\n'
        printf 'sandbox_mode = "danger-full-access"\n'
        printf '[model_providers.cctui]\n'
        printf 'name = "cctui-gateway"\n'
        printf 'base_url = "%s"\n' "$_base"
        printf 'env_key = "OPENAI_API_KEY"\n'
        printf 'wire_api = "responses"\n'
        # Unlocks codex's built-in image_gen tool (gated on
        # uses_openai_actor_authorization); the gateway strips the dummy value.
        printf 'http_headers = { "x-openai-actor-authorization" = "cctui-gateway" }\n'
        printf '[features]\n'
        printf 'fast_mode = false\n'
        # MCP tables MUST stay last: a bare key after a [table] binds to it.
        if adapter_is_codex && [ -f "$CONTEXT_DIR/mcp.json" ]; then
            codex_mcp_toml "$CONTEXT_DIR/mcp.json"
        fi
        printf '%s\n' "$CODEX_MARKER_END"
    } > "$_cfg"
    chown -R "${WORKER_UID}:${WORKER_UID}" "$_cfgdir" 2>/dev/null || true
    log "codex: model_provider 'cctui' wired into $_cfg (base_url from OPENAI_BASE_URL)"
}

# ── Phase 4c: Codex context-pack staging ────────────────────────────────────
# Codex ignores ~/.claude and ~/CLAUDE.md: it reads AGENTS.md walked up from cwd
# (+ the ~/.codex/AGENTS.md global) and prompts from ~/.codex/prompts/. MCP
# servers are wired into config.toml by phase_codex_config, not here.
phase_codex_pack() {
    adapter_is_codex || return 0
    [ -d "$CONTEXT_DIR" ] || return 0
    _home="/home/${WORKER_USER}"
    _cfgdir="${CODEX_HOME:-${_home}/.codex}"

    # /workspace/AGENTS.md is already staged by pack_wire_instructions and is in
    # the cwd ancestry, so codex reads it without a copy here. Staging into
    # CCTUI_DISPATCH_WORKDIR would write INSIDE the checkout (it resolves to
    # /workspace/${TASK_REPO}) and clobber a repo that commits its own AGENTS.md.
    _instr=""
    [ -f "$CONTEXT_DIR/AGENTS.md" ] && _instr="$CONTEXT_DIR/AGENTS.md"
    [ -z "$_instr" ] && [ -f "$CONTEXT_DIR/CLAUDE.md" ] && _instr="$CONTEXT_DIR/CLAUDE.md"
    if [ -n "$_instr" ]; then
        mkdir -p "$_cfgdir"
        cp -f "$_instr" "$_cfgdir/AGENTS.md" 2>/dev/null || true
        log "codex: staged AGENTS.md (<- $(basename "$_instr")) into $_cfgdir"
    fi

    if [ -d "$CONTEXT_DIR/prompts" ]; then
        mkdir -p "$_cfgdir/prompts"
        cp -a "$CONTEXT_DIR/prompts/." "$_cfgdir/prompts/" 2>/dev/null || true
        log "codex: staged prompts/ -> $_cfgdir/prompts"
    fi

    chown -R "${WORKER_UID}:${WORKER_UID}" "$_cfgdir" 2>/dev/null || true
}

# ── Phase 5: Result callback trap ───────────────────────────────────────────
# If REPLY_URL is set, install an EXIT/INT/TERM trap that POSTs the result JSON
# once (RESULT_FILE if the session wrote a valid one, else a synthesized
# failure). Wire shape per files/claude/automation-contract.md — tenant-visible, keep
# identical. Skipped (no trap) when REPLY_URL is unset.
RESULT_FILE="${RESULT_FILE:-/tmp/cctui-result.json}"
export RESULT_FILE
CALLBACK_SENT=0
send_callback() {
    [ "$CALLBACK_SENT" = 1 ] && return 0
    CALLBACK_SENT=1
    [ -z "${REPLY_URL:-}" ] && return 0
    # REPLY_URL is a bearer capability — never log it.
    if curl -4 -sS -X POST "$REPLY_URL" \
        -H 'content-type: application/json' --data "$1" \
        -o /dev/null --connect-timeout 10 --max-time 30 2>/dev/null; then
        log "callback posted"
    else
        log "callback POST failed; caller falls back to its timeout"
    fi
}
worker_on_exit() {
    _code=$?
    if [ -s "$RESULT_FILE" ] && jq -e . "$RESULT_FILE" >/dev/null 2>&1; then
        send_callback "$(cat "$RESULT_FILE")"
    else
        send_callback "$(jq -nc --arg id "${TASK_ID:-}" --arg code "$_code" \
            '{task_id:$id, status:"failed", error:("worker exited (code "+$code+") without a valid result")}')"
    fi
}
phase_callback() {
    [ -n "${REPLY_URL:-}" ] || { log "callback: no REPLY_URL, skipping trap"; return 0; }
    trap worker_on_exit EXIT INT TERM
    log "callback: REPLY_URL trap installed (RESULT_FILE=$RESULT_FILE)"
}

# ── Phase 6: Workflow guard daemon ──────────────────────────────────────────
# If the resolved prompt contains step definitions (`# Step` + `[allowed]`),
# start cctui-guard (root) against the prompt + GUARD_RULES_FILE (default
# /opt/context/guard-rules.md), always-allowing the seeded structural hosts, and
# export the PreToolUse hook env for Claude Code. Skipped when there is no
# prompt or no step markers.
GUARD_ON=off
phase_guard() {
    _rules="${GUARD_RULES_FILE:-$CONTEXT_DIR/guard-rules.md}"
    if [ ! -f "$_rules" ]; then
        log "guard: DEFAULT engaged (no guard-rules.md at $_rules) — tools=ALL (no gating); network=deny-default + seeded structural hosts only. Ship a pack guard-rules.md to gate tools per workflow step."
    fi
    _prompt=$(resolve_prompt_path)
    [ -n "$_prompt" ] || { log "guard: no prompt, running without guard"; return 0; }
    if [ ! -f "$_prompt" ]; then
        log "guard: prompt $_prompt not found, running without guard"
        return 0
    fi
    # Step markers: a `# Step` heading AND at least one `[allowed]` line.
    if ! grep -qiE '^#+[[:space:]]+step[[:space:]]+[0-9]' "$_prompt" 2>/dev/null \
       || ! grep -qiE '^\[allowed\]' "$_prompt" 2>/dev/null; then
        log "guard: no step markers in prompt, running without guard"
        return 0
    fi

    mkdir -p "$(dirname "$GUARD_STATE")" "$(dirname "$POLICY_FILE")"

    set -- --prompt "$_prompt" \
           --rules "$_rules" \
           --listen "127.0.0.1:${GUARD_PORT}" \
           --state "$GUARD_STATE" \
           --policy-out "$POLICY_FILE"
    # When a pack supplies guard-rules, GUARD_RULES_BASE holds the operator base
    # parsed first; the pack's --rules then overrides/extends it.
    [ -n "${GUARD_RULES_BASE:-}" ] && [ -f "${GUARD_RULES_BASE}" ] \
        && set -- "$@" --rules-base "$GUARD_RULES_BASE"
    # Always-allow the structural hosts so the agent keeps the model gateway and
    # the callback can fire even under a deny-default step policy.
    _cctui_hp=$(url_hostport "$CCTUI_BASE_URL")
    [ -n "$_cctui_hp" ] && set -- "$@" --always-allow "$_cctui_hp"
    _reply_hp=$(url_hostport "${REPLY_URL:-}")
    [ -n "$_reply_hp" ] && set -- "$@" --always-allow "$_reply_hp"
    # Operator-plane SNI allow-list (CDN/multi-IP hosts) must survive every
    # per-step rewrite too, mirroring the seeded policy in phase_network.
    if [ -n "${WORKER_NET_ALLOW:-}" ]; then
        _OLDIFS=$IFS; IFS=,
        for _na in $WORKER_NET_ALLOW; do
            IFS=$_OLDIFS
            _na=$(printf '%s' "$_na" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
            [ -n "$_na" ] && set -- "$@" --always-allow "$_na"
            IFS=,
        done
        IFS=$_OLDIFS
    fi

    cctui-guard "$@" &
    GUARD_PID=$!
    _i=0
    while [ "$_i" -lt 20 ]; do
        curl -fsS "http://127.0.0.1:${GUARD_PORT}/health" >/dev/null 2>&1 && break
        _i=$((_i + 1)); sleep 0.25
    done
    GUARD_ON=on
    # PreToolUse hook env for Claude Code (the agent's settings reference these).
    export CCTUI_GUARD_URL="http://127.0.0.1:${GUARD_PORT}"
    export PROMPT_FILE="$_prompt"
    log "guard: started (prompt=$_prompt, rules=$_rules, pid $GUARD_PID)"
}

# ── Phase 7: Hardening report ───────────────────────────────────────────────
# Collect entrypoint-side state; the supervisor appends landlock/seccomp via
# --report. Exposed as WORKER_HARDENING_JSON (env) + a file the daemon can
# attach as session metadata.
HARDENING_FILE=/tmp/cctui-hardening.json
phase_hardening() {
    WORKER_HARDENING_JSON=$(jq -nc \
        --arg net "$NET_MODE" \
        --arg guard "$GUARD_ON" \
        '{net_mode:$net, guard:$guard, supervisor_report:"/tmp/hardening.json"}')
    export WORKER_HARDENING_JSON
    printf '%s\n' "$WORKER_HARDENING_JSON" > "$HARDENING_FILE"
    export WORKER_HARDENING_FILE="$HARDENING_FILE"
    log "hardening: net_mode=$NET_MODE guard=$GUARD_ON"
}

# ── Phase 7b: Permission bypass seeding ─────────────────────────────────────
# A dispatched headless session runs in claude's bypassPermissions mode, which
# REFUSES to start (prompts the disclaimer) until `bypassPermissionsModeAccepted`
# is recorded in .claude.json, and prompts a trust dialog until
# `projects.<cwd>.hasTrustDialogAccepted` is set. The per-pod .claude is a fresh
# emptyDir every pod, so we seed both. Belt-and-suspenders in settings.json:
# `skipDangerousModePermissionPrompt` (the disclaimer) + `permissions.defaultMode
# = bypassPermissions` (unattended, no prompts). Merged into any existing files
# so the daemon's --settings hooks and managed-settings are untouched.
phase_permissions() {
    _cfgdir="${CLAUDE_CONFIG_DIR:-/home/${WORKER_USER}/.claude}"
    _cwd="${CCTUI_DISPATCH_WORKDIR:-/workspace}"
    mkdir -p "$_cfgdir"
    _cfg="$_cfgdir/.claude.json"
    if [ -f "$_cfg" ]; then
        _t=$(mktemp) && jq --arg cwd "$_cwd" \
            '. + {bypassPermissionsModeAccepted: true}
             | .projects = ((.projects // {}) | .[$cwd] = ((.[$cwd] // {}) + {hasTrustDialogAccepted: true}))' \
            "$_cfg" > "$_t" && mv "$_t" "$_cfg"
    else
        jq -nc --arg cwd "$_cwd" \
            '{bypassPermissionsModeAccepted: true, projects: {($cwd): {hasTrustDialogAccepted: true}}}' \
            > "$_cfg"
    fi
    _settings="$_cfgdir/settings.json"
    if [ -f "$_settings" ]; then
        _t=$(mktemp) && jq --arg cwd "$_cwd" \
            '. + {skipDangerousModePermissionPrompt: true}
             | .permissions = ((.permissions // {}) + {defaultMode: "bypassPermissions"}
               | .additionalDirectories = (((.additionalDirectories // []) + [$cwd]) | unique))' \
            "$_settings" > "$_t" && mv "$_t" "$_settings"
    else
        jq -nc --arg cwd "$_cwd" \
            '{skipDangerousModePermissionPrompt: true, permissions: {defaultMode: "bypassPermissions", additionalDirectories: [$cwd]}}' \
            > "$_settings"
    fi
    # Register context-pack PreToolUse hooks: every *.sh the pack
    # staged into ~/.claude/hooks becomes a PreToolUse entry. Claude Code unions
    # hooks across settings sources, so these run ALONGSIDE the daemon-managed
    # hooks (all matching hooks get the same stdin; any deny blocks). Matcher
    # "*": the scripts self-filter on tool_name, and a deny-only hook is safe on
    # every tool. Idempotent — a command already present is not re-added.
    _hooksdir="/home/${WORKER_USER}/.claude/hooks"
    if [ -d "$_hooksdir" ]; then
        for _hk in "$_hooksdir"/*.sh; do
            [ -f "$_hk" ] || continue
            _t=$(mktemp) && jq --arg cmd "$_hk" \
                '.hooks = ((.hooks // {})
                 | .PreToolUse = ((.PreToolUse // [])
                   | if any(.[]?; ((.hooks // [])[]?.command // "") == $cmd) then .
                     else . + [{matcher: "*", hooks: [{type: "command", command: $cmd}]}] end))' \
                "$_settings" > "$_t" && mv "$_t" "$_settings"
            log "permissions: registered PreToolUse hook $(basename "$_hk")"
        done
    fi
    # .claude is a per-pod emptyDir (not the NFS home), so a recursive chown
    # is safe here (unlike the home).
    chown -R "${WORKER_UID}:${WORKER_UID}" "$_cfgdir" 2>/dev/null || true
    log "permissions: seeded bypass + trust gates (cwd=$_cwd)"
}


# ── Run the phases (each individually skippable) ─────────────────────────────
# Derive the task-shape env the prompts + workspace expect from the dispatch
# payload. The dispatcher forwards the whole payload as TASK_PAYLOAD_JSON but
# only injects a few magic vars, so map the conventional fields here. Each is
# only set when unset, so an explicit pod-template/dispatcher value still wins.
if [ -n "${TASK_PAYLOAD_JSON:-}" ]; then
    _pj() { printf '%s' "$TASK_PAYLOAD_JSON" | jq -r "$1 // empty" 2>/dev/null || true; }
    [ -z "${TASK_IDENTITY:-}" ] && { _v=$(_pj '.identity'); [ -n "$_v" ] && export TASK_IDENTITY="$_v"; }
    [ -z "${TASK_REPO:-}" ]     && { _v=$(_pj '.repo');     [ -n "$_v" ] && export TASK_REPO="$_v"; }
    [ -z "${TASK_EFFORT:-}" ]   && { _v=$(_pj '.effort');   [ -n "$_v" ] && export TASK_EFFORT="$_v"; }
    [ -z "${TASK_MODEL:-}" ]    && { _v=$(_pj '.model');    [ -n "$_v" ] && export TASK_MODEL="$_v"; }
    [ -z "${TASK_ADAPTER:-}" ]  && { _v=$(_pj '.adapter');  [ -n "$_v" ] && export TASK_ADAPTER="$_v"; }
    if [ -z "${TASK_CONTEXT_JSON:-}" ]; then
        _v=$(printf '%s' "$TASK_PAYLOAD_JSON" | jq -c '.context // empty' 2>/dev/null || true)
        [ -n "$_v" ] && export TASK_CONTEXT_JSON="$_v"
    fi
    # Repo acquisition (no warm cache ⇒ clone): build the URL from context.owner +
    # repo, and the ref from context.head_sha, unless explicitly provided.
    if [ -z "${TASK_REPO_URL:-}" ] && [ -n "${TASK_REPO:-}" ]; then
        _owner=$(_pj '.context.owner')
        [ -n "$_owner" ] && export TASK_REPO_URL="https://github.com/${_owner}/${TASK_REPO}"
    fi
    [ -z "${TASK_REPO_REF:-}" ] && { _v=$(_pj '.context.head_sha'); [ -n "$_v" ] && export TASK_REPO_REF="$_v"; }
fi
phase_network
phase_workspace
# The CLI grants edit-in-place only when the session cwd IS a git repo,
# so default the workdir to the repo (must run after phase_workspace clones it).
if [ -z "${CCTUI_DISPATCH_WORKDIR:-}" ] && [ -n "${TASK_REPO:-}" ] \
        && [ -d "/workspace/${TASK_REPO}" ]; then
    export CCTUI_DISPATCH_WORKDIR="/workspace/${TASK_REPO}"
    log "dispatch: CCTUI_DISPATCH_WORKDIR defaulted to /workspace/${TASK_REPO} (edit-in-place)"
fi
phase_context_pack
# When a pack is active, drive cctui-guard from the dispatched prompt: derive
# TASK_PROMPT_FILE from payload.prompt_file so resolve_prompt_path finds the
# pack's prompt and the guard enforces its [allowed]/[network] steps. Scoped to
# pack flows (CONTEXT_PACK_URL set) so legacy configMap dispatches keep their
# current — unguarded — behavior until they migrate to a pack.
if [ -z "${TASK_PROMPT_FILE:-}" ] && [ -n "${CONTEXT_PACK_URL:-}" ] && [ -n "${TASK_PAYLOAD_JSON:-}" ]; then
    TASK_PROMPT_FILE=$(printf '%s' "$TASK_PAYLOAD_JSON" | jq -r '.prompt_file // empty' 2>/dev/null || true)
    [ -n "${TASK_PROMPT_FILE:-}" ] && export TASK_PROMPT_FILE \
        && log "prompt: TASK_PROMPT_FILE=${TASK_PROMPT_FILE} (from payload; pack active → guard will engage if the prompt has steps)"
fi
phase_extensions
phase_codex_config
phase_codex_pack
phase_callback
phase_guard
phase_permissions
phase_hardening

# ── Phase 8: Drop privileges + run ──────────────────────────────────────────
# cctui-supervisor applies landlock + seccomp, drops all caps, setuids to the
# worker, then exec's the daemon. RO: system + context/prompts; RW: workspace,
# home, tmp, the guard/proxy state dirs. The supervisor's own --report captures
# landlock/seccomp/uid for the hardening metadata.
run_supervised_daemon() {
    cctui-supervisor \
        --ro /usr --ro /lib --ro /lib64 --ro /bin --ro /sbin --ro /etc --ro /proc \
        --ro /prompts --ro /var/run/guard-proxy-ca \
        --ro "$CONTEXT_DIR" \
        $(extra_ro_flags) \
        --rw /dev --rw /tmp --rw /workspace --rw "/home/${WORKER_USER}" \
        --rw /var/run/workflow-guard --ro /var/run/guard-proxy --rw /var/run/gpg-agent \
        $(extra_rw_flags) \
        --user "$WORKER_UID" \
        --report /tmp/hardening.json \
        -- cctui-daemon run --no-auto-update "$@"
}

# SIGTERM the background daemon and wait up to WORKER_TEARDOWN_GRACE_SECS for it
# to flush and exit; SIGKILL if it overstays. The graceful stop lets the daemon
# push the transcript tail before the pod is reaped.
stop_daemon_gracefully() {
    kill -TERM "$DAEMON_PID" 2>/dev/null || return 0
    _grace=0
    while [ "$_grace" -lt "$WORKER_TEARDOWN_GRACE_SECS" ]; do
        kill -0 "$DAEMON_PID" 2>/dev/null || { log "teardown: daemon flushed and exited"; return 0; }
        sleep 1
        _grace=$((_grace + 1))
    done
    log "teardown: daemon still up after ${WORKER_TEARDOWN_GRACE_SECS}s grace; SIGKILL"
    kill -KILL "$DAEMON_PID" 2>/dev/null || true
}

# ── Phase 9: Dual-signal "work done" wait ─────────────────────────
# With `claude -p`, "work is done" (semantic) and "process is gone" (liveness)
# were the same event, so `wait $PID` sufficed. `claude daemon` splits them:
# the dispatch op acks instantly and the daemon stays up — there is no blocking
# "wait until done" primitive. So a DISPATCHED worker (SESSION_ID +
# TASK_PAYLOAD_JSON present) runs the daemon in the BACKGROUND and blocks on two
# signals:
#
#   PRIMARY (done)     — the agent declares completion via cctui-guard ->
#                        POST /transition {"step":"exit"}, which flips the guard
#                        state file to STEP_EXITED (-1) AND relaxes the egress
#                        proxy so the result callback can leave. Guard-less
#                        workers (no step markers) fall back to watching the
#                        RESULT_FILE appear with valid JSON, or the daemon's
#                        dispatch_done turn-complete marker.
#   BACKSTOP (crashed) — the dispatched session dies WITHOUT signalling done.
#                        Sourced from the daemon's authoritative registration:
#                        the server row for $SESSION_ID leaves "active" (the
#                        daemon deregistered it on SessionEnded, or its heartbeat
#                        went stale). Gated on "seen registered once" so a slow
#                        cold start is not mistaken for a crash.
#
# Either signal ends the wait; the EXIT trap (phase_callback) then POSTs the
# preserved clean/failed verdict from RESULT_FILE. A non-dispatched (thin)
# worker has no task to finish, so it keeps the original exec-forever behavior.
# GUARD_STATE is already the state FILE path (--state, default
# /var/run/workflow-guard/state) that cctui-guard's engine writes {"step":N} to.
GUARD_STATE_FILE="$GUARD_STATE"
WAIT_POLL_SECS="${WORKER_DONE_POLL_SECS:-2}"
# Fail-closed boot bound. A `claude daemon run` that crash-loops
# `exited code=1` (often behind a network deny) keeps cctui-daemon up but never
# dispatches a session, so it never registers and the backstop (gated on "seen
# registered once") never fires — the wait would block to the 24h
# activeDeadlineSeconds, burning a pod slot for a day with no callback. Bound the
# time-to-first-registration: if the dispatched session never registers within
# this window, write a failed RESULT_FILE and exit non-zero so the EXIT trap
# POSTs the callback promptly.
WORKER_BOOT_DEADLINE_SECS="${WORKER_BOOT_DEADLINE_SECS:-120}"

# Belt-and-suspenders result grace: under GUARD_ON a finished session
# that wrote a valid RESULT_FILE but never POSTed /transition exit would block to
# the 24h activeDeadlineSeconds. Arm a countdown on a valid RESULT_FILE and exit
# the wait even under guard once it elapses; guard-exit stays the fast path.
WORKER_RESULT_GRACE_SECS="${WORKER_RESULT_GRACE_SECS:-60}"

# Teardown flush grace: after the done-signal, SIGTERM the daemon and give it
# this long to flush the transcript tail (final tool_use + error) to the server
# before the pod is reaped, then SIGKILL. cctui-daemon handles SIGTERM as a
# graceful shutdown; a hard kill here would drop the last events.
WORKER_TEARDOWN_GRACE_SECS="${WORKER_TEARDOWN_GRACE_SECS:-10}"

# Turn-complete marker: cctui-daemon writes
# ~worker/.claude/jobs/<short>/dispatch_done once the session it dispatched at
# boot has been busy at least once and then settled idle (default 60s,
# CCTUI_DISPATCH_DONE_SETTLE_SECS). Catches the "finished its turn but stays
# idle-and-alive under `claude daemon`" case none of the other signals fires
# for: no guard step=-1, no RESULT_FILE, the daemon stays up and the session
# stays registered — the pod would otherwise idle to activeDeadlineSeconds.
# <short> is the first 8 chars of the session id, mirroring the daemon's
# `session_id[..8]` (control.rs).
_SHORT=$(printf %s "${SESSION_ID:-}" | cut -c1-8)
DISPATCH_DONE_MARKER="/home/${WORKER_USER}/.claude/jobs/${_SHORT}/dispatch_done"

dispatch_done_marker() {
    [ -n "$_SHORT" ] && [ -e "$DISPATCH_DONE_MARKER" ]
}

# Guard signalled completion: state file says {"step":-1}.
guard_exited() {
    [ "$GUARD_ON" = on ] || return 1
    [ -f "$GUARD_STATE_FILE" ] || return 1
    _step=$(jq -r '.step // empty' "$GUARD_STATE_FILE" 2>/dev/null || true)
    [ "$_step" = "-1" ]
}

# The session wrote a valid result JSON, ignoring the guard gate.
result_valid() {
    [ -s "$RESULT_FILE" ] && jq -e . "$RESULT_FILE" >/dev/null 2>&1
}

# Guard-less done: the session wrote a valid result JSON.
result_ready() {
    [ "$GUARD_ON" = on ] && return 1
    result_valid
}

# Per-session liveness backstop — sourced from the cctui-daemon's own
# registration, NOT a grep of claude's private jobs dir: claude's internal job
# id rotates on resume/clear and isn't reliably discoverable for
# cold-dispatched worker sessions. The daemon is the source of truth: it
# launches claude with `--session-id $SESSION_ID` and registers THAT stable
# id with the server, immune to claude's id rotation. Ask the server for it.
#
# Use the LIST endpoint, not GET /sessions/{id}: the per-object route's
# Resource(Session) guard is `admin || owner==principal`, and a machine-key
# principal is NOT the session's resolved owner (machine_uuid -> user_id), so
# it 403s even for the pod's OWN session. GET /sessions is self-scoped via
# owner_filter(): a machine key sees its OWN machine's sessions — a short list
# (dispatch pods are single-session) that includes our dispatched id.
_SESSIONS_URL="${CCTUI_BASE_URL%/}/api/v1/sessions"
_PROBE_BODY=/tmp/cctui-liveness-probe.json
WORKER_LIVENESS_POLL_SECS="${WORKER_LIVENESS_POLL_SECS:-10}"
# The server folds TWO different things into `inactive` (routes/sessions.rs): a
# real SessionEnded deregistration, and a heartbeat merely older than its 5m
# STATUS_WINDOW. Only the first is death. The heartbeat is bumped by adapter
# events alone, so one long blocking tool call — a `codex exec` over 5m — silences
# it and reads identical to a crash. Tell the two apart by the heartbeat's own
# age, and give a merely-quiet session this much slack before giving up on it.
WORKER_LIVENESS_STALE_SECS="${WORKER_LIVENESS_STALE_SECS:-1800}"
# Mirrors the server's STATUS_WINDOW_SECS; only used to attribute an `inactive`
# to deregistration rather than to heartbeat staleness.
WORKER_SERVER_STATUS_WINDOW_SECS="${WORKER_SERVER_STATUS_WINDOW_SECS:-300}"
# Codex dispatch runs headless `codex exec` inside the daemon and never
# registers a server session, so the registration-sourced liveness probe and its
# boot deadline can never fire for it. Its done-signal is RESULT_FILE and its
# backstop is daemon death (`kill -0 $DAEMON_PID`); pre-seed seen-alive so the
# boot bound is a no-op and a codex run over WORKER_BOOT_DEADLINE_SECS isn't
# guillotined mid-work.
case "${TASK_ADAPTER:-}" in
    codex|codex-cli) _SEEN_ALIVE=1 ;;
    *)               _SEEN_ALIVE=0 ;;
esac
_PROBE_LOGGED_CODE=""
_PROBE_LOGGED_NOHB=""
_QUIET_LOGGED=""
# Probe the daemon's server-side registration for OUR session id. Echoes:
#   registered — our id present with status "active"/"new": the daemon holds
#                this session live (registered, heartbeat within STATUS_WINDOW=5m).
#   ended      — 'inactive' AND the heartbeat explains it as a real deregistration
#                (still fresh) or as staleness past WORKER_LIVENESS_STALE_SECS.
#                Only trusted as death after we have seen it registered once.
#   quiet      — 'inactive' only because the heartbeat aged past the server's
#                window: alive but mid-blocking-call. Keep waiting.
#   unknown    — transient curl/non-200, or our id not (yet) in the roster;
#                never read as death.
# `-4` mirrors the callback curl (the worker forces IPv4 egress).
probe_session() {
    _code=$(curl -4 -sS -o "$_PROBE_BODY" -w '%{http_code}' --max-time 5 \
        -H "Authorization: Bearer $CCTUI_MACHINE_KEY" "$_SESSIONS_URL" 2>/dev/null) \
        || { echo unknown; return; }
    if [ "$_code" != 200 ]; then
        # Surface a persistent auth/URL fault ONCE rather than failing blind —
        # the first fix died silently on a 403 from the wrong endpoint.
        if [ "$_PROBE_LOGGED_CODE" != "$_code" ]; then
            log "wait: liveness probe HTTP $_code from $_SESSIONS_URL (treating as unknown)"
            _PROBE_LOGGED_CODE="$_code"
        fi
        echo unknown
        return
    fi
    _row=$(jq -r --arg id "$SESSION_ID" '
        .sessions[]? | select(.id == $id)
        | ((.last_heartbeat // "") | sub("\\.[0-9]+Z$"; "Z")
           | (try fromdateiso8601 catch null)) as $hb
        | "\(.status) \(if $hb == null then -1 else (now - $hb | floor) end)"
    ' "$_PROBE_BODY" 2>/dev/null | head -n1)
    _st=${_row%% *}
    _hb_age=${_row##* }
    case "$_hb_age" in
        ''|*[!0-9-]*) _hb_age=-1 ;;
    esac
    case "$_st" in
        active|new) echo registered; return ;;
        inactive)   ;;
        *)          echo unknown; return ;;
    esac
    # An unreadable heartbeat can't clear the session, so keep the old
    # fail-closed reading rather than blocking to activeDeadlineSeconds.
    if [ "$_hb_age" -lt 0 ]; then
        if [ "$_PROBE_LOGGED_NOHB" != 1 ]; then
            log "wait: session ${SESSION_ID%%-*} inactive with unreadable last_heartbeat; treating as ended"
            _PROBE_LOGGED_NOHB=1
        fi
        echo ended
        return
    fi
    # Inside the server's window the row can only have gone inactive on an
    # explicit SessionEnded — the heartbeat itself is still fresh.
    if [ "$_hb_age" -lt "$WORKER_SERVER_STATUS_WINDOW_SECS" ] \
        || [ "$_hb_age" -ge "$WORKER_LIVENESS_STALE_SECS" ]; then
        echo ended
        return
    fi
    echo quiet
}

await_dispatch_done() {
    log "wait: blocking on dual signal (guard=$GUARD_ON, session=${SESSION_ID:-none})"
    _waited=0
    _next_probe=0
    _result_armed=-1
    while :; do
        # The daemon process going away is itself terminal — nothing left to
        # finish the task, so stop waiting and let the trap synthesize a verdict.
        if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
            log "wait: cctui-daemon (pid $DAEMON_PID) exited; ending wait"
            return 0
        fi
        # PRIMARY done-signals — cheap local file reads, every loop.
        if guard_exited; then
            log "wait: guard signalled completion (step=-1)"
            return 0
        fi
        if result_ready; then
            log "wait: result file ready (guard-less done)"
            return 0
        fi
        if dispatch_done_marker; then
            log "wait: daemon wrote dispatch_done marker (turn complete)"
            return 0
        fi
        # Fallback: intentionally uses result_valid (no guard gate), so it
        # also fires under GUARD_ON where result_ready cannot; guard_exited above
        # stays the immediate fast path.
        if result_valid; then
            if [ "$_result_armed" -lt 0 ]; then
                _result_armed="$_waited"
                log "wait: valid RESULT_FILE seen; arming ${WORKER_RESULT_GRACE_SECS}s grace exit (guard=$GUARD_ON)"
            elif [ "$((_waited - _result_armed))" -ge "$WORKER_RESULT_GRACE_SECS" ]; then
                log "wait: RESULT_FILE grace elapsed (${WORKER_RESULT_GRACE_SECS}s) without a done-signal; exiting wait"
                return 0
            fi
        fi
        # Daemon-sourced liveness — throttled server probe (not every 2s).
        if [ "$_waited" -ge "$_next_probe" ]; then
            _next_probe=$((_waited + WORKER_LIVENESS_POLL_SECS))
            case "$(probe_session)" in
                registered)
                    if [ "$_SEEN_ALIVE" = 0 ]; then
                        _SEEN_ALIVE=1
                        log "wait: session ${SESSION_ID%%-*} registered with the daemon (seen alive)"
                    fi
                    _QUIET_LOGGED=""
                    ;;
                quiet)
                    if [ "$_QUIET_LOGGED" != 1 ]; then
                        log "wait: session ${SESSION_ID%%-*} quiet (heartbeat stale, no SessionEnded); holding up to ${WORKER_LIVENESS_STALE_SECS}s"
                        _QUIET_LOGGED=1
                    fi
                    ;;
                ended)
                    # Only terminal once we have seen it alive: a pre-registration
                    # 404 is just a slow cold start, not a crash.
                    if [ "$_SEEN_ALIVE" = 1 ]; then
                        log "wait: daemon reports session ${SESSION_ID%%-*} ended without a done-signal"
                        return 0
                    fi
                    ;;
            esac
        fi
        # Fail-closed boot bound: the daemon is still up but never
        # registered the dispatched session within the boot window — a wedged
        # `claude daemon run` (crash-loop / network deny). Surface a fast failure
        # instead of blocking to the 24h deadline. Once the session has been seen
        # alive ($_SEEN_ALIVE=1), the bound no longer applies; long legitimate
        # work is governed by activeDeadlineSeconds as before.
        if [ "$_SEEN_ALIVE" = 0 ] && [ "$_waited" -ge "$WORKER_BOOT_DEADLINE_SECS" ]; then
            log "wait: claude daemon failed to boot a session within ${WORKER_BOOT_DEADLINE_SECS}s; failing closed"
            jq -nc --arg id "${TASK_ID:-}" --arg secs "$WORKER_BOOT_DEADLINE_SECS" \
                '{task_id:$id, status:"failed", error:("claude daemon failed to boot a session within "+$secs+"s")}' \
                > "$RESULT_FILE"
            exit 1
        fi
        sleep "$WAIT_POLL_SECS"
        _waited=$((_waited + WAIT_POLL_SECS))
    done
}

if [ -n "${SESSION_ID:-}" ] && [ -n "${TASK_PAYLOAD_JSON:-}" ]; then
    log "dispatched worker -> background cctui-daemon + dual-signal wait"
    run_supervised_daemon "$@" &
    DAEMON_PID=$!
    await_dispatch_done
    stop_daemon_gracefully
    exit 0
fi

log "thin worker -> exec cctui-supervisor -> cctui-daemon (run forever)"
exec cctui-supervisor \
    --ro /usr --ro /lib --ro /lib64 --ro /bin --ro /sbin --ro /etc --ro /proc \
    --ro /prompts --ro /var/run/guard-proxy-ca \
    --ro "$CONTEXT_DIR" \
    $(extra_ro_flags) \
    --rw /dev --rw /tmp --rw /workspace --rw "/home/${WORKER_USER}" \
    --rw /var/run/workflow-guard --ro /var/run/guard-proxy --rw /var/run/gpg-agent \
    $(extra_rw_flags) \
    --user "$WORKER_UID" \
    --report /tmp/hardening.json \
    -- cctui-daemon run --no-auto-update "$@"
