# cctui worker contract — v1

The contract between a **dispatcher** (the docker / kubernetes dispatcher that
spawns a worker per session) and the **worker image**
(`ghcr.io/dorskfr/cctui-worker`, built from `deploy/worker.Dockerfile`). It
defines every environment variable, the optional mounts, the capability
requirements per network mode, the result-callback wire shape, the hardening
report, and the context-pack layout. Derived-image authors
(`FROM ghcr.io/dorskfr/cctui-worker`) and dispatcher operators build against
this document.

Images that honor this contract carry the OCI label `dev.cctui.contract=1`.

## Plane ownership

Every variable belongs to one plane. The boundary is a security boundary, not a
convention:

- **platform** — set by cctui-server / the dispatcher core. The worker's
  identity and the daemon wiring. Never tenant-controlled.
- **operator** — set by the dispatcher operator in pod template / dispatcher
  config (the context pack, network exemptions, warm repos). Defines the
  sandbox the tenant runs inside. **Never** sourced from the task payload —
  whoever controls these controls the sandbox policy.
- **tenant** — carried in the dispatch payload (`TASK_PAYLOAD_JSON`). The tenant
  may select *what* runs (a prompt file, an identity) but never *the rules it
  runs under*.

## The degenerate worker (parity with the thin image)

With **only** the three platform vars set —

```
CCTUI_URL  CCTUI_MACHINE_KEY  SESSION_ID
```

— every entrypoint phase is skipped on absent input and the worker behaves
exactly like the pre-v1 thin image: it falls straight through to
`exec cctui-supervisor … -- cctui-daemon run --no-auto-update`. No phase
hard-fails on missing input. The **one** exception: if `CONTEXT_PACK_URL` is
set, the context-pack fetch must succeed or the boot fails closed (the pack
defines the guard rules; proceeding without it would weaken the sandbox).

All **three** platform vars are themselves a hard precondition, checked before
any phase runs: a worker missing `CCTUI_URL`/`CCTUI_SERVER_URL`,
`CCTUI_MACHINE_KEY`, or `SESSION_ID` exits non-zero rather than degrade. Absent
`SESSION_ID` in particular is a broken dispatch — it would skip the
dispatch/wait phase and leave a plain exec-forever daemon that never registers
the session or runs the task, so it fails closed. The thin/degenerate worker
above still carries `SESSION_ID`; the thin path is the *no-`TASK_PAYLOAD_JSON`*
case, never a *no-`SESSION_ID`* case.

## Environment variables

### Platform plane

| Var | Required | Meaning |
| --- | --- | --- |
| `CCTUI_URL` (or `CCTUI_SERVER_URL`) | yes | cctui-server base URL the daemon dials. Its host is always-allowed in the egress policy. |
| `CCTUI_MACHINE_KEY` | yes | Shared machine key; the daemon runs without enroll (`Config::from_env`). |
| `SESSION_ID` | yes | Pre-minted session id to register. |
| `TASK_PAYLOAD_JSON` | no | Opaque dispatch payload (tenant fields live inside it). |
| `TASK_NAME` | no | Human label for the session. |
| `TASK_ID` | no | Task id echoed in the synthesized failure callback. |

### Operator plane

| Var | Default | Meaning |
| --- | --- | --- |
| `WORKER_NET_MODE` | auto | `transparent`, `transparent-external`, or `forward`. Auto = `transparent` when iptables is usable (CAP_NET_ADMIN), else `forward`. `transparent-external` is never auto-selected — set it only when the pod runs the guard-proxy sidecar + net-init init container (see below). |
| `WORKER_NET_EXEMPT` | — | Comma-separated `host:port` that **bypass** the proxy (resolved to a single IP at boot, iptables `RETURN`'d). Transparent mode only. IP-pinned — use only for **IP-stable** hosts; CDN/multi-IP hosts will rotate off the exempted IP. |
| `WORKER_NET_ALLOW` | — | Comma-separated `host:port` allowed **through** the proxy by **SNI** (IP-independent). Folded into the seeded `policy.json` and re-applied as `cctui-guard --always-allow` so it survives per-step rewrites. The right tool for CDN/multi-IP SaaS APIs (e.g. a YouTrack host). |
| `WARM_REPO_DIR` | — | Mounted warm-repo dir; becomes the overlayfs lowerdir for `/workspace` (rsync-copy fallback). |
| `TASK_REPO_URL` | — | Repo to shallow-clone into `/workspace` when no warm repo. |
| `TASK_REPO_REF` | — | Branch/tag to clone (`git clone --depth 1 --branch`). |
| `CONTEXT_PACK_URL` | — | Git repo of the context pack. May carry `@<ref>` (path) and `#<subdir>` to pin in one value. Set ⇒ fetch is **fail-closed**. Its `host:port` is seeded into the boot guard-proxy policy (boot-only — not `--always-allow`'d, so the first per-step rewrite closes it), and the clone runs as `WORKER_UID` so it flows through the proxy: with no `CONTEXT_PACK_TOKEN`, a matching inject rule (optionally `path_prefix`-scoped to the pack repo) supplies the credential. |
| `CONTEXT_PACK_REF` | default branch | Branch/tag/sha. Optional — absent ⇒ the remote's default branch. Pin it in prod. Overrides any `@<ref>` in the URL. |
| `CONTEXT_PACK_TOKEN` | — | HTTPS basic token for a private pack (injected as `https://<token>@host`). Never logged. Falls back to `payload.env.GITHUB_TOKEN` when unset, so a tenant can ship one token for pack-clone + repo clone/push. |
| `CONTEXT_PACK_SUBDIR` | — | Subdirectory within the pack repo to use as the pack root. |
| `CONTEXT_PACK_TOKEN_FROM` | — | Name of an env var holding the pack-clone token, when it differs from the task identity's (e.g. the task identity can't read the pack repo). Resolved before the `GITHUB_TOKEN` fallbacks; keeps a specific identity name out of the image. |
| `GUARD_RULES_FILE` | `/opt/context/guard-rules.md` | Guard rules path; defaults into the fetched pack (the override/extend layer). |
| `GUARD_RULES_BASE` | — | Operator base guard-rules parsed **before** `GUARD_RULES_FILE`. When a pack ships `guard-rules.md`, the entrypoint moves any prior `GUARD_RULES_FILE` here so the pack reuses/extends/overrides it. |
| `CCTUI_DISPATCH_WORKDIR` | `/workspace/$TASK_REPO` if that dir exists (else `/workspace`) | Session working directory. Defaulted to the checked-out repo after `phase_workspace` so the CLI grants edit-in-place (a cwd inside the repo) instead of blocking background edits with "call `EnterWorktree` first"; an explicit operator/dispatcher value always wins. |

### Tenant plane (from `TASK_PAYLOAD_JSON`)

| Var | Meaning |
| --- | --- |
| `TASK_PROMPT_FILE` | Prompt file; resolves under `/opt/context/prompts/` (absolute path honored as-is). Drives guard activation. When a pack is active (`CONTEXT_PACK_URL` set) and this is unset, the entrypoint derives it from `payload.prompt_file` so the guard engages on the pack's prompt; legacy (no-pack) dispatches are left unguarded as before. |
| `TASK_IDENTITY` | Selects the credential env set (`GITHUB_TOKEN_<ID>`, …). Absent ⇒ image default. |
| `REPLY_URL` | Result-callback target (a bearer capability — never logged). Set ⇒ exit trap installed. Its host is always-allowed. |
| `RESULT_FILE` | Where the session writes its verdict. Default `/tmp/cctui-result.json`. |

### Credential env — placeholders only (CCT-719)

The worker holds **no real secrets**, in env or on disk. The guard-proxy sidecar
TLS-terminates github/npm/… and injects the real `Authorization` from its own
secret source (CCT-716/717/718); the worker's job is only to make each tool
**emit** an `Authorization` for the proxy to rewrite. So the credential helper
(`worker-credentials.sh`) materializes **placeholder** tokens, never real ones:

| Tool | Materialized (placeholder) | Kept real |
| --- | --- | --- |
| github | `GH_TOKEN=cctui-placeholder` + git credential helper `!gh auth git-credential` (always) | `GITHUB_NAME` / `GITHUB_EMAIL` (non-secret identity) |
| npm | `~/.npmrc` `_authToken=cctui-placeholder` (always) | — |
| mcp | `~/.mcp.json` http entry, `Authorization: Bearer cctui-placeholder` | `MCP_<NAME>_URL` server URL |

github/npm placeholders are written **unconditionally** (those tools always need
a token); the host-targeted blocks (mcp) are skipped when their
non-secret host/workspace config is absent (nothing to target). Every block is
idempotent. `GPG_PRIVATE_KEY` is **sidecar-only** (CCT-721) — never imported into
the worker; a key seen in the worker env is a misconfiguration. `ANTHROPIC_*` /
`OPENAI_*` model auth is unchanged (platform env passthrough / gateway token).

No identity resolve/scrub runs on the worker: real per-identity secrets never
reach it (the sidecar selects them via `GUARD_PROXY_IDENTITY`). `TASK_IDENTITY`
is still set from `payload.identity` and labels the placeholders + the sidecar's
secret selection.

**No pod-wide secret injection.** The worker overlay carries the Vault
`vault-role` annotation but **not** `vault-env-from-path` — from-path injected
the whole secret path into every container (the worker included). The sidecar
pulls its per-identity `CRED_*` via explicit per-value `vault:` env refs, which
need only the role.

> **Dev caveat.** End-to-end auth (placeholder → proxy → real credential) needs
> the sidecar's `CRED_*` populated in the secret store. Dev lacks them, so the
> injector forwards the placeholder unchanged and authenticated github/npm calls
> fail there until an operator wires `CRED_<IDENTITY>_<SERVICE>` on the sidecar.
> Anonymous reads still work. The boundary (no real secret in the worker) holds
> regardless.

### Secret references in `payload.env` (CCT-490)

The dispatch payload's `env` map is the **one secret-injection surface**, and it
carries **references, never secret values**. The kube dispatcher lifts
`payload.env` out of `TASK_PAYLOAD_JSON` and promotes each entry to **pod
container env**, choosing the form by value prefix:

| `payload.env` value | becomes | resolved by |
| --- | --- | --- |
| `vault:<path>#<field>` | literal env var | the in-cluster **vault-env** webhook, at exec — before the entrypoint |
| `k8s:[<ns>/]<secret>#<key>` | `valueFrom.secretKeyRef` (namespace prefix dropped; must exist in the pod's ns) | the kubelet, at pod start |
| anything else | literal env var | passthrough (plain value / test override) |

So by the time the entrypoint runs, every var holds a **real value** — the
dispatcher never reads the secret, and a `vault:`/`k8s:` value never lands in the
Job spec or etcd. The boundary is the worker's Vault role scope (tenant prefix)
and the namespace's k8s secrets, not the reference string (which is untrusted).
The entrypoint stays product-aware (wires `gh`); the dispatcher stays
secret-agnostic. In `transparent-external`, `GPG_PRIVATE_KEY` is delivered to the
**guard-proxy sidecar** (not the worker) — see [Remote GPG signing](#remote-gpg-signing-transparent-external-cct-721).
`env` is stripped from
`TASK_PAYLOAD_JSON` so the daemon cannot re-apply an unresolved reference over
the resolved pod env.

## Optional mounts

| Path | Mode | Purpose |
| --- | --- | --- |
| `WARM_REPO_DIR` (any path) | RO | overlayfs lowerdir / rsync source for `/workspace`. |
| `/workspace` | RW | The task working tree (overlay, clone, or empty). |
| `/opt/context` | RO after fetch | Context pack (prompts, docs, skills, guard rules). |
| `/home/worker` | RW | Per-session agent state (`.claude`, `.codex`, `.mcp.json`, `.npmrc`, `.gnupg`). |
| `/var/run/guard-proxy`, `/var/run/workflow-guard` | RW | Proxy policy + guard state. |
| `/var/run/guard-proxy-ca` | RO | Per-pod guard-proxy CA (`ca.pem`); in the supervisor's Landlock RO set so `NODE_EXTRA_CA_CERTS` is readable after privdrop. |
| `/var/run/gpg-agent` | RW | Shared `gpg-agent` emptyDir carrying the forwarded restricted signing socket (`transparent-external` only, CCT-721). |
| `/tmp` | RW | Scratch, `RESULT_FILE`, hardening report. |

## Network modes & capability requirements

Egress is always gated by `cctui-guard-proxy` (uid 1337), deny-default. The
policy is seeded at boot to always-allow the `CCTUI_URL` + `REPLY_URL` hosts;
`cctui-guard` rewrites it per workflow step when a guarded prompt runs.

### Built-in guard default (no `guard-rules.md`)

A pack's `guard-rules.md` is optional. When none is present — a manual job with
an inline prompt and no pack, or a pack that ships no rules file — the worker
applies a **documented built-in default**, logged loudly at boot
(`guard: DEFAULT engaged (no guard-rules.md …)`):

- **Tools = all / no gating.** No `cctui-guard` tool-set is enforced; every tool
  the agent can invoke is allowed. Per-tool gating exists only when a pack ships
  `guard-rules.md` **and** the prompt carries `# Step N` + `[allowed]` blocks.
- **Network = deny-default + seeded structural hosts.** Egress stays the boot
  `policy.json`: deny-default with only `CCTUI_URL`, `REPLY_URL`, and any
  `WORKER_NET_ALLOW`/`CONTEXT_PACK_URL` host allowed. The sandbox floor holds
  even with no rules file — an unguarded run is never an *open* run.

To gate tools per workflow step, ship a `guard-rules.md` in the pack and give the
prompt step blocks; the default is the safe fallback, not a recommendation.

| Mode | Capability at start | Mechanism |
| --- | --- | --- |
| `transparent` (default w/ NET_ADMIN) | **CAP_NET_ADMIN** | iptables REDIRECTs worker-uid (1000) TCP egress to `:15001`. Exempts root, the proxy uid, loopback, DNS, the `CCTUI_URL` host, and `WORKER_NET_EXEMPT`. IPv6 egress denied (proxy is IPv4-only — forces IPv4 fallback). |
| `transparent-external` (explicit only) | **none** (in this container) | Same wire behaviour as `transparent`, but the proxy runs in a **separate sidecar container** and the iptables rules are installed by a **NET_ADMIN init container**. The entrypoint only seeds `policy.json` and waits for the sidecar's `:15002/ready`. |
| `forward` (no NET_ADMIN) | **none** | No iptables. `HTTP_PROXY`/`HTTPS_PROXY=http://127.0.0.1:15001` exported for the worker tree; `NO_PROXY=127.0.0.1,localhost`. For rootless Docker / gVisor / Apple container. |

The proxy listens on `:15001` (traffic) and `:15002` (`/health`, `/ready`).
Capabilities are dropped entirely before the daemon runs — see hardening below.

### Daemon child-env capability scrub (CCT-719)

The daemon runs inside the worker and inherits the platform capability vars
(`CCTUI_MACHINE_KEY[_FILE]`, `REPLY_URL`). A `Command` inherits the daemon's full
environment by default, so every `claude`/`codex` child it exec's is scrubbed of
those vars (`childenv::CHILD_ENV_REMOVALS`, applied via `ScrubChildEnv` at every
spawn site) — the agent (untrusted code that can read its own env) never sees the
machine key it could impersonate the machine with, or the result-callback bearer
it could spoof completion with.

### Metadata/credential deny-list (CCT-720)

The whole credential model depends on the **worker** container being unable to
reach the credential backend directly — credential-backend reachability is a
property of the **sidecar** only. Two independent layers enforce this, so neither
a buggy/malicious `policy.json` nor a compromised proxy can open the hole:

1. **guard-proxy built-in deny** (`crates/cctui-guard-proxy/src/denylist.rs`) —
   an always-deny set that **overrides** the allow-list. Even if `policy.json`
   lists them (or sets `default: allow`), these are refused with a `DENY
   (builtin)` log line:
   - `169.254.0.0/16` (link-local): AWS/GCP/Azure IMDS (`169.254.169.254`) and
     the EKS Pod Identity Agent (`169.254.170.23`).
   - `metadata.google.internal` / `metadata` (GCP metadata DNS names).
   - Any host in `GUARD_PROXY_DENY_HOSTS` (comma-separated) — in dev this carries
     the OpenBao host (`openbao.security.svc.cluster.local`). The sidecar reaches
     OpenBao **directly** (via the `vault-env` webhook, not through its own egress
     proxy), so denying it here only affects the worker path.

   The check matches on **both** the recovered hostname (SNI/Host) **and** the
   resolved original-destination IP, so an IP-literal request straight to
   `169.254.169.254` with no SNI is still caught.

2. **net-init iptables REJECT** (`deploy/worker-net-init.sh`) — belt-and-
   suspenders at the packet layer: the worker uid's egress to `169.254.0.0/16` is
   `RETURN`ed from the nat REDIRECT (so it keeps its original dst) and hard-
   `REJECT`ed in the filter table, independent of the proxy being up.

**Expectations** (verifiable once on the cluster; the live 169.254 refusal cannot
be exercised in unit tests):
- Dev: OpenBao is unreachable from the worker (`DENY openbao…` — already observed
  in live logs before this ticket; now un-allowlistable).
- Prod (EKS path): `curl http://169.254.169.254/latest/meta-data/` and the Pod
  Identity Agent at `169.254.170.23` are refused; `aws sts get-caller-identity`
  from the worker fails to obtain any identity (no IMDS/IRSA reach). The worker
  carries a non-secret `AWS_REGION` only — a region hint, not a credential.

### Sidecar mode (`transparent-external`, CCT-716)

Promotes the guard-proxy out of the worker container so its memory, env, and
`/proc` live in a namespace the agent cannot read, and lets the worker container
drop `privileged`/NET_ADMIN. The pod wires three pieces (all runnable from the
base `ghcr.io/dorskfr/cctui-worker` image — no extra image needed):

1. **`net-init` init container** — `command: ["cctui-worker-net-init"]`, runs as
   root with `capabilities.add: ["NET_ADMIN"]` (drop ALL otherwise, NOT
   privileged). The pod network namespace is shared, so the REDIRECT it installs
   (uid-1000 TCP → `:15001`, exemptions for uid 0 / uid 1337 / loopback / DNS /
   `WORKER_NET_EXEMPT`, IPv6 deny) governs every container. Honors
   `WORKER_UID`/`PROXY_UID`/`PROXY_PORT`/`WORKER_NET_EXEMPT` env.
2. **`guard-proxy` sidecar** — a native sidecar (initContainer with
   `restartPolicy: Always`) running
   `cctui-guard-proxy --mode transparent --listen 0.0.0.0:15001
   --health-listen 0.0.0.0:15002 --policy /var/run/guard-proxy/policy.json` as
   **uid 1337** (must match the net-init exemption), all caps dropped. Shares
   with the worker ONLY the `proxy-policy` emptyDir (`/var/run/guard-proxy`) and
   the pod network — **no `shareProcessNamespace`**. The proxy is fail-closed:
   it starts deny-all with no policy file and hot-reloads (1s mtime poll) once
   the worker entrypoint seeds it; `/ready` stays 503 until a policy is loaded.
3. **Worker container** — `WORKER_NET_MODE=transparent-external`. The entrypoint
   skips iptables and does not start a proxy; it still seeds `policy.json`
   (deny-default + structural hosts) into the shared emptyDir and best-effort
   waits (≤30s) for the sidecar's `/ready` over pod-local net. `cctui-guard`'s
   per-step policy rewrites keep working unchanged through the shared file.

Worker-container capabilities in this mode: `privileged` and NET_ADMIN are
gone. The entrypoint still boots as root, so it keeps `drop: ALL` plus the
minimal add set: `SYS_ADMIN` (overlayfs + `mount --bind` context-pack/home
isolation), `CHOWN`/`DAC_OVERRIDE`/`FOWNER`/`FSETID` (workspace + home chowns as
root), `SETUID`/`SETGID` (cctui-supervisor's setresuid/setresgid drop to uid
1000), `SETPCAP` (the supervisor clears the bounding set), and `KILL` (the
dispatched-worker wait kills the uid-1000 daemon tree from root). The container
mounts need an unconfined AppArmor profile where the runtime's default profile
denies `mount(2)` (previously masked by `privileged: true`).

### Sidecar secret source (CCT-717)

Every secret the sidecar can inject is named by an **explicit, engine-qualified
secret ref** — the proxy knows engines, never a service taxonomy of its own, so
what a ref points at is a deployment decision (see the inject config below).
Resolved secrets live only in an in-memory TTL cache (default 120s, never
persisted, never logged); a fetch past TTL re-reads the backend, so store-side
rotation lands within one TTL. Lookups fail closed: "no credential configured"
and "backend failure" are distinct errors, and a backend failure never
degrades to a blank secret.

| Ref form | Engine | Notes |
|---|---|---|
| `env:VAR` | the sidecar's OWN environment | Empty values count as not configured. The sanctioned dev path: the dev cluster's vault-env webhook materializes `vault:` refs into the **sidecar** container env, so the worker container never sees the values. |
| `vault:<mount>/data/<path>#<field>` | Vault/OpenBao KV v2 (dev) | Needs `--vault-addr` + `--vault-role`; Kubernetes auth with the pod SA token. `bao:` is an accepted alias for refs carried in pod **env values** — the bank-vaults webhook would otherwise hijack a literal `vault:` prefix before the proxy sees it. |
| `aws-sm:<name>` / `aws-sm:<name>#<json-field>` | AWS Secrets Manager (prod) | SDK default credential chain (EKS Pod Identity / IRSA — the sidecar's ambient identity). Which secrets a pod may read is enforced in IAM by secret-name prefix, not in code. |
| `k8s:[<namespace>/]<secret>#<key>` | Kubernetes Secret | Read via the pod's own `ServiceAccount`; RBAC decides what it may read. |

Refs may carry `${IDENTITY}` (the task identity, ASCII alphanumerics uppercased
and every other character mapped to `_` — `acme-corp` → `ACME_CORP`) or
`${identity}` (verbatim). A ref that uses a placeholder while `--identity` is
unset is a **startup error**, never a silently wrong path.

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--secret-ttl-secs` | `GUARD_PROXY_SECRET_TTL_SECS` | `120` | In-memory cache TTL. |
| `--identity` | `GUARD_PROXY_IDENTITY` | *(empty)* | Task identity substituted into ref placeholders. |
| `--vault-addr` | `GUARD_PROXY_VAULT_ADDR` | — | Vault/OpenBao base URL (enables the `vault:` engine). |
| `--vault-role` | `GUARD_PROXY_VAULT_ROLE` | — | Kubernetes auth role (required with `--vault-addr`). |
| `--vault-token-path` | `GUARD_PROXY_VAULT_TOKEN_PATH` | `/var/run/secrets/kubernetes.io/serviceaccount/token` | SA token for `POST /v1/auth/kubernetes/login`. |

An engine that is not configured fails its refs as a *backend* error (closed),
never as "not found".

The `fetch-secret <ref>` subcommand resolves one ref (identity templating
applied) and prints it to stdout — how the sidecar entrypoint obtains the GPG
signing key.

### TLS-terminating credential injection (CCT-718)

For an **injection allow-list** of hosts the sidecar TLS-terminates the agent's
connection, STRIPS whatever credential the agent supplied, substitutes the real
one from the secret source, and re-encrypts to the upstream over real TLS
(validating the upstream's real cert). This is the strip-then-substitute /
phantom-token pattern: the task carries only a credential *selector* (its
identity), never the secret. Hosts NOT on the allow-list keep the SNI-peek
passthrough splice, so the MITM surface is only the allow-list. Injection is
**inert** unless the inject config file exists — default behaviour is unchanged
pure passthrough.

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--inject-config` | `GUARD_PROXY_INJECT_CONFIG` | `/etc/guard-proxy/inject.json` | The injection config (below). Absent ⇒ zero rules ⇒ passthrough. MUST NOT live in a worker-writable location. |
| `--ca-cert-out` | `GUARD_PROXY_CA_CERT_OUT` | `/var/run/guard-proxy-ca/ca.pem` | Where the sidecar writes the PUBLIC per-pod CA cert. |

**Inject config.** A JSON array; each entry names one host, the auth shape, and
the explicit secret ref(s) to inject. It is the single source of truth for
injection — the proxy ships no built-in `host → service` table, so every
injected host is a deliberate deployment entry. A **malformed config fails
startup**: a partially applied credential config is worse than none.

| Field | Required | Meaning |
|---|---|---|
| `host` | yes | Host to TLS-terminate (lowercased). Repeat the host for several path-scoped rules. |
| `secret` | yes (except `headers`) | Secret ref of the credential to inject. |
| `shape` | no (`bearer`) | `bearer`, `basic`/`git`, `bearer+cookie`/`cookie`, `sigv4`, or `headers`. |
| `service` | no (`host`) | Label for logs and GitHub-App provider matching — never a lookup key. |
| `path_prefix` | no | Scopes the rule to a request path; must start with `/`. The **longest** matching prefix wins, ties keep config order, and a request matching no rule forwards the agent's head unchanged. |
| `username` | no (`x-access-token`) | Basic-auth username. |
| `cookie_name` / `cookie_secret` | `cookie_secret` required for `bearer+cookie` | Companion session cookie (default name `d`). |
| `headers` | required for `headers` | Map of header name → secret ref (no `Authorization`). Replaces `secret`. |
| `region` / `aws_service` / `key_id_secret` | `key_id_secret` required for `sigv4` | `SigV4` signing params; `region`/`aws_service` default from a `<service>.<region>.amazonaws.com` host. `secret` is the secret access key, `key_id_secret` the access key id. |

```jsonc
[
  { "host": "api.github.com", "service": "github",
    "secret": "vault:kvmount/data/cctui/workers#GITHUB_TOKEN_${IDENTITY}" },
  { "host": "github.com", "service": "github", "shape": "git",
    "secret": "vault:kvmount/data/cctui/workers#GITHUB_TOKEN_${IDENTITY}" },
  { "host": "registry.npmjs.org", "service": "npm", "secret": "env:CRED_NPM" },
  { "host": "api.datadoghq.com", "service": "datadog", "shape": "headers",
    "headers": { "DD-API-KEY": "env:DD_API_KEY", "DD-APPLICATION-KEY": "env:DD_APP_KEY" } }
]
```

`sigv4` requests are buffered (≤1 MiB) so the signature can cover the body hash;
a chunked or oversized body cannot be hashed and forwards unchanged, exactly
like a credential miss.

**Per-pod CA.** At boot the sidecar mints a CA (via `rcgen`), keeps the private
key **in memory only** (never on disk), and writes only the public cert (PEM,
0644) to `--ca-cert-out` on a shared `emptyDir`. Leaf certs for injection hosts
are minted on demand from the SNI and cached, signed by that CA. In
`transparent-external` mode the worker entrypoint waits (bounded) for the CA
file and installs it — into the system trust store via `update-ca-certificates`
when writable, and by exporting `NODE_EXTRA_CA_CERTS` (additive) plus
`GIT_SSL_CAINFO` / `REQUESTS_CA_BUNDLE` / `SSL_CERT_FILE` / `CURL_CA_BUNDLE` at a
bundle that trusts the public roots **and** the guard CA (never replacing the
public roots — passthrough hosts keep their real certs). Single-container modes
never MITM, so they skip this.

**Shapes.** `bearer` covers most REST APIs and the npm registry (npm's on-disk
`_authToken` rides as a Bearer header). `basic`/`git` covers git-over-HTTPS,
rewritten to the GitHub-blessed `x-access-token:<token>` Basic form.
`bearer+cookie` covers browser-session tokens that need a companion cookie (e.g.
Slack's `xoxc` + `d`). `sigv4` covers AWS APIs. `headers` covers APIs keyed by
named non-`Authorization` headers (Datadog's `DD-API-KEY` + `DD-APPLICATION-KEY`):
the agent's copies of those headers are stripped and every ref must resolve or
the request forwards unchanged.

**Fail-closed on lookup problems.** The agent never holds a real secret, so on
`NotFound` **or** backend error the injector forwards the agent's ORIGINAL
`Authorization` UNCHANGED (the upstream rejects the placeholder) — never a blank
or wrong secret. Only a successful fetch triggers the strip-and-substitute. A
host whose ref resolves to nothing therefore keeps working off the agent's own
header: the safe intermediate state while a deployment's refs are being wired.

**Cert-pinning hosts are passthrough-only.** A CLI that pins its server cert
would break under this MITM, so such hosts must never appear in the inject
config; they get their credential another way.

### GitHub App installation tokens (CCT-722)

For the `github` service the sidecar can inject a short-lived, repo-scoped
GitHub **App installation token** instead of a stored long-lived PAT, so even
in-session misuse (which the boundary can't prevent) is time- and
scope-bounded. The App **private key (PEM)** lives in the secret store, named by
the `--github-app-key-secret` ref (identity placeholders apply) — never on the
worker. At use-time the sidecar signs a
short RS256 JWT (`iss`=App id, ~9 min lifetime), exchanges it at
`POST /app/installations/<id>/access_tokens` (scoping to `--github-app-repos`
when set), and injects the returned ~1h installation token. The token is cached
until ~5 min before its `expires_at` and re-minted on expiry — never per
request; neither the token nor the key ever touches disk.

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--github-app-id` | `GUARD_PROXY_GITHUB_APP_ID` | *(unset)* | GitHub App id (JWT `iss`). |
| `--github-app-installation-id` | `GUARD_PROXY_GITHUB_APP_INSTALLATION_ID` | *(unset)* | Installation id the token is minted for. |
| `--github-app-key-secret` | `GUARD_PROXY_GITHUB_APP_KEY_SECRET` | *(unset)* | Secret ref of the App private-key PEM. **Required** once the two ids above are set — startup fails without it. |
| `--github-app-repos` | `GUARD_PROXY_GITHUB_APP_REPOS` | *(empty)* | Comma list of bare repo names to scope the token to (empty = installation default). |
| `--github-app-api-base` | `GUARD_PROXY_GITHUB_APP_API_BASE` | `https://api.github.com` | REST base for the exchange; override only for testing. |

**Inert until configured.** The App path activates only when BOTH
`--github-app-id` and `--github-app-installation-id` are set. Even then, the key
fetch being `NotFound` falls back to the rule's own `github` credential ref,
which itself fail-closes to passthrough — so with no App configured behaviour is
exactly today's. A token-exchange failure
(e.g. 401) is a backend error → the injector forwards the agent's original
header unchanged (fail-closed).

**Operator action required.** Registering the GitHub App (App id, installation,
private key) and storing the key in the secret store is a one-time human step —
the mechanism ships **inert** until then. The App path is opt-in **per
identity**.

**Personal-repo caveat.** A GitHub App bot cannot CREATE PRs on a repo it isn't
a collaborator on. Where PR creation is needed, keep that identity on the
injected `NanachiBot` machine-user PAT path (the `github` service credential)
instead of the App path.

### Remote GPG signing (`transparent-external`, CCT-721)

GPG never touches the network, so the header-injection above cannot deliver a
signing key — and shipping `GPG_PRIVATE_KEY` to the worker would put the raw key
in the agent's own container. Instead the key lives **only in the sidecar** and
the worker signs over a forwarded gpg-agent socket:

1. **Sidecar boot wrapper** — the sidecar's container command is
   `cctui-guard-proxy-entrypoint` (shipped in the base image) instead of
   `cctui-guard-proxy` directly. It receives the same flags. When
   `GPG_PRIVATE_KEY` is present (materialized into the **sidecar** env by the
   dev vault-env webhook, same pattern as the `CRED_*` vars) it: imports the key
   into a container-local keyring under `/tmp`, launches gpg-agent with an
   `extra-socket` at `/var/run/gpg-agent/S.gpg-agent.extra` on the shared
   `gpg-agent` emptyDir, scrubs the armored key from the env, publishes the
   PUBLIC key (`pubkey.asc`) + signing key id (`signingkey`) to that emptyDir,
   then `exec`s `cctui-guard-proxy`. With no key it is a pure passthrough.
2. **Restricted socket.** gpg-agent's `--extra-socket` can USE the key for
   signing but **cannot export the secret key** (`gpg --export-secret-keys` over
   it fails). Only that socket is forwarded — the full socket and the private
   keyring never leave the sidecar.
3. **Worker side** — under `transparent-external` the entrypoint bounded-waits
   for the extra socket, imports the public key into `~worker/.gnupg`, symlinks
   gpg's expected agent-socket (`gpgconf --list-dirs agent-socket`, plus a
   homedir `S.gpg-agent` fallback) to the forwarded socket, and sets
   `user.signingkey` + `commit.gpgsign true` **only when the socket is present**.
   Absent socket ⇒ logged loud, signing left off (commits unsigned, never
   failing the boot). The `gpg-agent` emptyDir is added to the supervisor
   Landlock RW set so `git commit -S` can reach the socket after privilege drop.

**Key setup (recommended).** Use a per-identity **signing subkey with a short
expiry**, passphrase-less (a headless sidecar has no pinentry). The mechanism
works with a subkey — gpg selects the signing-capable subkey automatically, and
`signingkey` is published as the primary fingerprint. Rotating/expiring the
subkey limits blast radius without touching the worker image.

## Result callback (tenant-visible wire shape)

When `REPLY_URL` is set, an EXIT/INT/TERM trap POSTs the result exactly once
(`Content-Type: application/json`, IPv4-forced, 30s max). It prefers a valid
`RESULT_FILE` the session wrote; otherwise it synthesizes a failure. This wire
shape is identical to the homelab `automation-contract.md` and must stay so — callers
branch on the structured fields, never on prose.

Common envelope (every flow):

```jsonc
{
  "task_id":   "<from payload, if present>",
  "flow":      "<this flow's name>",
  "status":    "success",   // success | failed | needs_human
  "dedup_key": "<from payload, if present>",
  "error":     null         // one-line reason when status != success
}
```

Synthesized failure (no valid `RESULT_FILE`):

```json
{ "task_id": "<TASK_ID>", "status": "failed", "error": "worker exited (code N) without a valid result" }
```

Flows add their own fields on top (e.g. `review-pr` adds `review_id`, `verdicts`,
`review_body`). Do not invent fields a flow does not consume. `REPLY_URL` is a
bearer capability — whoever holds it can forge the verdict — so it is never
logged.

An implementation flow that opens a PR carries a required **`evidence[]`** array
alongside the envelope — the proof that backs a `success` verdict (see *Evidence-
required done gate*). Each entry is `{ kind, surface, summary, detail }`:

```jsonc
{
  "task_id": "…",
  "flow":    "implement",
  "status":  "success",
  "evidence": [
    { "kind": "test-run", "surface": "pure-calc",
      "summary": "all 14 parser tests pass",
      "detail":  "$ cargo test -p cctui-guard\n… 14 passed; 0 failed" },
    { "kind": "diff", "surface": "pure-calc",
      "summary": "adds evidence kind to the parser",
      "detail":  "@@ … @@\n+…" }
  ]
}
```

`evidence[]` is **required to be non-empty for a `success` finalize** on a flow
that touches code — a `success` callback with an empty (or absent) `evidence[]`
is a contract violation; the gate must instead report `needs_human` with what is
blocked. The array is consumed by the PR renderer (one section per surface) and,
longer-term, by `cctui-github` (diff viewer + review-draft store + evidence
attachments) — the not-yet-built crate is the natural home for richer rendering.

## Codex-native dispatch (CCT-643)

A dispatch payload carries an optional **`adapter`** key selecting which harness
the worker runs:

- `adapter` absent, `"claude"`, or `"claude-code"` (the default — backward
  compatible) → the claude worker path: `cctui-daemon` self-issues a `dispatch`
  on the `claude daemon` control socket and observes the session.
- `adapter: "codex"` → the **codex dispatch runner** (`crates/cctui-daemon/src/
  dispatch_codex.rs`): the daemon runs one headless `codex exec --json` in
  `/workspace`, parses its JSONL event stream, and writes the verdict to
  `RESULT_FILE` — the same envelope the callback trap POSTs. `payload.model` /
  `payload.effort` become `codex exec -m` / `-c model_reasoning_effort`;
  approvals, sandbox mode, and the cctui gateway provider still come from the
  per-pod `~/.codex/config.toml` the entrypoint hardens (`phase_codex_config`).

This runner is **separate from, and does not replace,** the interactive Rust
app-server adapter in `crates/cctui-daemon/src/adapters/codex/`. That adapter
drives long-lived, attachable Codex threads for machine-hosted sessions; a
dispatch is a one-shot, fire-and-report job with no attach surface.

### Spike: Python Codex SDK vs `codex exec`

The runner shells out to `codex exec` rather than embedding the Python Codex
SDK. Rationale:

- **Symmetry with the claude path.** The claude dispatch runner shells out to a
  CLI (`claude` via the control socket). Shelling out to `codex exec` keeps both
  runners the same shape — build an argv, run a process, parse its structured
  output into `RESULT_FILE` — with one result-reporting seam instead of two.
- **No Python runtime in the worker.** The SDK is a Python library that pins its
  own Codex runtime; adopting it would add a Python layer (and a second,
  independently-versioned Codex) to an image that already ships the `codex`
  binary (`ARG CODEX_VERSION`, the floor drift-checked against
  `contract::CODEX_MIN_VERSION`). `codex exec` reuses that one binary.
- **Reuses existing hardening.** `phase_codex_config` already writes a locked-down
  `config.toml` (approvals off, full-access sandbox — the pod is the sandbox — and
  the cctui gateway model provider). `codex exec` honours it verbatim; the SDK
  would need that posture re-expressed programmatically.

The SDK's advantage — a typed, programmatic turn API — buys little here: a
dispatch runs exactly one turn and reports a verdict, which `codex exec --json`
already emits as a parseable event stream (`item.completed` agent messages,
`turn.completed` usage, `error`/`turn.failed`). So `codex exec` wins on
simplicity, image weight, and consistency.

## Hardening report

Two parts, surfaced for the daemon to attach as session metadata:

- **Entrypoint state** — `WORKER_HARDENING_JSON` (env) and
  `WORKER_HARDENING_FILE` (`/tmp/cctui-hardening.json`):

  ```json
  { "net_mode": "transparent", "guard": "on", "supervisor_report": "/tmp/hardening.json" }
  ```

  `net_mode` ∈ `transparent|transparent-external|forward`; `guard` ∈ `on|off`.

- **Supervisor report** — `cctui-supervisor --report /tmp/hardening.json`:

  ```json
  {
    "landlock": "V5 (fully-enforced)",
    "seccomp_applied": true,
    "seccomp_blocked": ["ptrace", "..."],
    "caps_dropped": true,
    "uid": 1000,
    "ro_paths": ["/usr", "..."],
    "rw_paths": ["/tmp", "..."],
    "command": ["cctui-daemon", "run", "--no-auto-update"]
  }
  ```

  `landlock` is `"unavailable"` when the kernel does not enforce the ruleset.

## Entrypoint phases

The container starts as root, runs these phases (each skippable on absent
input), then drops to uid 1000:

1. **Network** — choose mode, seed the deny-default policy, install iptables
   (transparent) or export proxy env (forward), start `cctui-guard-proxy` — or,
   in `transparent-external`, only seed the policy and wait for the sidecar.
2. **Workspace** — overlayfs/rsync from `WARM_REPO_DIR`, else shallow-clone
   `TASK_REPO_URL`, else empty `/workspace`; chown worker.
3. **Context pack** — fetch the pinned ref to `/opt/context` (fail-closed when
   `CONTEXT_PACK_URL` set); wire `CLAUDE.md`/skills/style/projects into the
   worker home; default `GUARD_RULES_FILE`.
4. **Extensions** — source any `*.sh` in `/opt/worker-entrypoint.d/` (lexical
   order). The generic seam derived images use to inject boot phases (e.g.
   credential materialization) without forking the entrypoint. No-op on the
   public image (empty dir).
5. **Codex config** — write the `cctui` model-provider region into
   `~/.codex/config.toml` (see Codex-native dispatch, below).
6. **Codex pack** — under the Codex adapter, stage `AGENTS.md` + `prompts/` from
   the context pack (see `docs/context-packs.md`).
7. **Callback** — install the `REPLY_URL` exit trap.
8. **Guard** — start `cctui-guard` if the resolved prompt has step blocks
   (`# Step N` + `[allowed]`); always-allow the structural hosts.
9. **Permissions** — seed Claude's bypass-permissions + trust-dialog gates for
   the dispatch workdir and register any context-pack `PreToolUse` hooks.
10. **Hardening** — assemble `WORKER_HARDENING_JSON`.
11. **Drop + run** — `exec cctui-supervisor --ro … --rw … --user 1000
    --report /tmp/hardening.json -- cctui-daemon run --no-auto-update`.

## Context pack

A git-hosted bundle of prompts/docs/skills/guard-rules the worker fetches at
boot, replacing per-cluster ConfigMaps as the universal way to give a dispatched
agent its documentation environment. Anyone can define their own dispatch types
(prompts + skills + rules) with a pack — no image builds, no k8s.

### Layout

```
<pack repo root or CONTEXT_PACK_SUBDIR>/
  CLAUDE.md          # home-level instructions  -> /home/worker/CLAUDE.md
  AGENTS.md          # codex instructions (optional; falls back to CLAUDE.md)
  rules/             # always-on guidance (push) -> ~/.claude/rules/ (auto-loaded)
  docs/              # on-demand reference (pull) -> ~/.claude/docs/
  schemas/           # per-flow JSON schemas       -> ~/.claude/schemas/
  prompts/           # dispatch prompts; TASK_PROMPT_FILE resolves here
  skills/            # skill dirs (SKILL.md …)   -> ~/.claude/skills/
  projects/          # per-repo CLAUDE.md overlays -> /home/worker/projects/
  style/             # output styles               -> /home/worker/style/
  mcp.json           # adapter-neutral MCP servers (mcpServers map)
  guard-rules.md     # tool-set + network-set definitions for cctui-guard
  pack.toml          # optional manifest; [dirs] table + optional base layer
```

`rules/` is **always-on** (auto-loaded on every task) and `docs/` is **pull-only**
(referenced on demand) — always-on conventions belong in `rules/`, not `docs/`. A
`schemas/` dir carries per-flow JSON schemas (e.g. `result.json`) a prompt can
validate the result envelope against before writing `RESULT_FILE`; it wires to
`~/.claude/schemas/` via the generic unknown-key seam. The fixture pack at
`deploy/examples/context-pack/` exercises **every** wired seam (`rules/`,
`schemas/`, `projects/`, `style/`, `pack.toml`, plus `skills/`/`docs/`).

A neutral fixture pack lives at `deploy/examples/context-pack/`.

The above targets are the **Claude** staging. A pack is **portable**: the same
neutral content stages to Codex targets (AGENTS.md, `~/.codex/config.toml` MCP
entries, `~/.codex/prompts/`) when a dispatch selects `adapter: "codex"`. See
*Codex context-pack packaging* below and `docs/context-packs.md` for the full
adapter-target matrix.

### Shared base layer (monorepo of packs)

A repo can host several packs plus a shared `_base` dir holding universal
material (home `CLAUDE.md`, `guard-rules.md`, universal `rules/`) once. A pack
subdir selected via `CONTEXT_PACK_SUBDIR` declares the shared layer in its
`pack.toml`:

```toml
base = "../_base"   # path relative to the subdir; confined to the clone tree
```

The entrypoint copies `_base` **first**, then overlays the pack subdir on top, so
the pack wins on any same-named file (`cp -a "$X/." dest/` merges trees). Absent
a `base =` line, it falls back to a repo-root `_base` dir when a subdir is
selected; with no `_base` present the pack is used as-is (unchanged behaviour).

```
<repo>/
  _base/             # merged under every pack (CLAUDE.md, guard-rules.md, rules/…)
  <pack>/            # e.g. acme/ — pack.toml (base="../_base") + its own dirs
```

### Mechanics

- The fetch is `git clone --depth 1` (or a depth-1 fetch of a SHA) of
  `CONTEXT_PACK_REF` into `/opt/context`, **root-side, before guard lockdown and
  before the agent can write anything**. `/opt/context` is read-only under
  landlock afterwards.
- It happens **pre-lockdown from the root side**, so no egress policy hole is
  needed for the pack host.
- Seams: `TASK_PROMPT_FILE` resolves under `/opt/context/prompts/`;
  `GUARD_RULES_FILE` defaults to `/opt/context/guard-rules.md`; the pack's
  `CLAUDE.md`/rules/docs/skills/style/projects are copied to the locations the
  agent expects.
- **Base merge:** when a subdir pack declares `base = "…"` (or a repo-root
  `_base` exists), that shared layer is copied into `/opt/context` first and the
  pack subdir overlaid on top, before any of the seams above resolve — so
  `guard-rules.md`, `CLAUDE.md`, and universal `rules/` can live once in `_base`
  while each pack ships only its deltas.
- **Home isolation:** `/home/worker` is a ReadWriteMany volume shared by
  concurrent workers, so when a pack is active the entrypoint bind-mounts a
  per-pod dir (under the `/overlay` emptyDir) over the paths the pack overwrites
  (`~/CLAUDE.md`, `~/projects`, `~/style`) before copying — the pack's writes
  stay private to the pod and never mutate the shared copy. `~/.claude` is
  already a per-pod emptyDir.
- **Guard-rules layering:** a pack's `guard-rules.md` is parsed as a layer on top
  of `GUARD_RULES_BASE` (the operator base). `[name]: …` overrides a base set;
  `[name]+: …` extends it (appends); a new name adds a set. So a pack reuses
  common sets (`net-dev`, …) and only states its deltas.
- `rules/` vs `docs/` (CCT-490): `rules/` is **push** — copied to
  `~/.claude/rules/`, which Claude Code auto-loads as instructions on every task
  (always-on guardrails/conventions). `docs/` is **pull** — copied to
  `~/.claude/docs/` and referenced on demand by a prompt that needs it
  (`@~/.claude/docs/<x>.md`); it is not auto-loaded.

### Codex context-pack packaging (CCT-644)

The staging above is Claude-shaped (`.claude` conventions). A pack is
**adapter-portable**: it declares content once, in an adapter-neutral form, and
the entrypoint stages it to the harness the dispatch selects (`payload.adapter`).
For `adapter: "codex"` the entrypoint **additively** stages the Codex targets
(the Claude path is byte-for-byte unchanged and never sees these):

- **Instructions → `AGENTS.md`.** Codex reads project instructions from an
  `AGENTS.md` walked up from the working dir plus a `~/.codex/AGENTS.md` global —
  never `~/CLAUDE.md`. `phase_codex_pack` stages the pack's `AGENTS.md` (or, when
  absent, its `CLAUDE.md`) to `AGENTS.md` at the dispatch workdir root
  (`CCTUI_DISPATCH_WORKDIR`, defaulting to the checked-out `/workspace/$TASK_REPO`
  when present, else `/workspace`) and to `~/.codex/AGENTS.md`.
- **MCP servers → `config.toml`.** A pack's neutral `mcp.json` (standard
  `mcpServers` map) is translated by `phase_codex_config` into
  `~/.codex/config.toml` `[mcp_servers.<name>]` tables, appended inside the
  managed model-provider region. stdio servers map `command`/`args`/`env`;
  streamable-HTTP servers map `url` + `bearer_token_env_var`. The same `mcp.json`
  merges into `~/.mcp.json` on the Claude path. Server names must be
  TOML-bare-key safe.
- **Prompts → `~/.codex/prompts/`.** The pack's `prompts/` are copied into
  `~/.codex/prompts/` as custom slash-prompts (in addition to remaining under
  `/opt/context/prompts/`, where `TASK_PROMPT_FILE` resolves for both adapters).
- **Model provider + account config.** The `cctui` gateway provider, model,
  effort, approvals, and sandbox mode come from `phase_codex_config` as before
  (from `OPENAI_API_KEY`/`OPENAI_BASE_URL` + `TASK_CODEX_MODEL`/`TASK_EFFORT`).

`skills/`, `hooks/`, and Claude-only conventions have no Codex target and are
skipped for Codex; always-on `rules/` are best folded into `AGENTS.md` by the
pack author. Full adapter-target matrix: `docs/context-packs.md`.

### Security rationale

Whoever controls the pack controls the sandbox policy (`guard-rules.md` defines
the tool/network sets the guard enforces). Therefore the pack URL+ref is
**operator-plane** config. A configured-but-unfetchable pack fails the boot
closed — silently proceeding without it would weaken the sandbox.

Precedence (entrypoint `phase_context_pack`):

1. **Pod template env** — the true operator plane. If the worker pod template
   sets `CONTEXT_PACK_URL` (etc.), that value wins and the payload cannot
   override it. Pin the pack here when the dispatcher must not be trusted to
   choose it.
2. **Dispatch payload `env`** — fallback, used only for a `CONTEXT_PACK_*` var
   the template leaves unset. This lets an **operator-controlled** dispatcher
   (e.g. the automation flows, which set `payload.env.CONTEXT_PACK_URL/REF/TOKEN`)
   select the pack per dispatch without baking it into the template. Because the
   pack defines the guard fence, this is only safe when the dispatch channel
   itself is operator-controlled — do **not** expose it to an untrusted tenant.
   A template that pins the pack (step 1) closes this door.

The task payload may also select a *prompt file within* the pack
(`TASK_PROMPT_FILE`).

## Intent+Acceptance ratify gate

An implementation prompt should make its **first** step an Intent+Acceptance
ratify gate — a structural pause that surfaces what the agent thinks "done"
means before it writes any code. The pattern lives in the example pack as the
`intent-acceptance` skill plus the first step of `prompts/example-task.md`; a
real pack adapts it to its dispatch types.

After gathering context (task, linked discussion, relevant code), the agent
emits a small **Intent+Acceptance artifact**:

```yaml
intent: >
  <one or two sentences: the outcome that means "done">
acceptance:
  - <checkable success condition>
  - <checkable success condition>
surfaces: [<class>, ...]   # pure-calc | frontend | backend | external-api
                           # | webhook | payments | brand-visible
blast_radius: <low|medium|high>   # max over surfaces
ratify: <auto|human>              # auto only when blast_radius == low
```

The gate is enforced by the guard, not by convention: the early step's
`[transition]` is the only path into the implement step, so the agent cannot
reach implementation without passing through it. Ratification routes back to the
human via the **`needs_human`** result-callback status (see *Result callback*)
carrying the artifact; a corrected intent replaces the artifact and becomes the
spec. Changes whose surfaces are **all** low blast radius (`pure-calc`)
auto-ratify and skip the round-trip.

The artifact is **persisted and reused verbatim** as the acceptance script at
the end of the run — the success condition promised up front is the one the
deliverable is checked against — and is attached to the session + PR.

## Conditional classifier pipeline

The pipeline is effectively linear, but **which** oracles run, **how** autonomous
the run may be, and **whether** a human must sign off all depend on what the task
touches. Between the Intent+Acceptance gate and implementation, an implementation
prompt should run a **classifier** that turns the ratified `surfaces[]` into a
pipeline plan. The pattern lives in the example pack as the `classify-surface`
skill plus the second step of `prompts/example-task.md`.

The plan is a **pure, deterministic function of the surface set** — same surfaces
always yield the same plan, so the routing is auditable and the agent cannot talk
itself into a softer gate:

```yaml
plan:
  oracles: [<skill>, ...]        # the oracle (verification) skills the surfaces demand
  autonomy: <auto-merge|human-gate>
  brand_gate: <true|false>
  required_evidence: [<kind>, ...]   # union over surfaces; feeds the evidence gate
```

Each surface class maps to a fixed row (oracles, autonomy, brand gate, required
evidence); a multi-surface change takes the **union** of the oracle/evidence
columns and the **strictest** autonomy. The `classify-surface` skill carries the
full table. Two routing decisions matter:

- **Autonomy** — `auto-merge` is granted **only** when *every* surface is
  `pure-calc`: a deterministic change backed by golden tests can merge on green
  without a human, because the oracle is the reviewer (this mirrors the ratify
  gate's `blast_radius: low` ⇔ auto path). `payments`, auth, and migrations are
  **always** `human-gate` regardless of how green the oracles are — a silent
  auto-merge there is the most dangerous case.
- **Brand / taste gate** — `brand_gate: true` whenever a `brand-visible` surface
  is present. Taste (copy, layout, pricing, naming, emails) cannot be oracle'd,
  only routed: no test asserts that wording is on-brand, so the change routes to
  a human for a sign-off that is explicitly *not* a correctness check. The brand
  gate is independent of and additional to the autonomy gate.

The plan conditions the rest of the run: implementation runs exactly the
`oracles[]` selected; the evidence gate demands at least the `required_evidence`
union; and the finalize step grants the `auto-merge` capability **only** when the
plan says so — otherwise the finalized PR routes to a human via the `needs_human`
callback for the merge decision (the PR is opened, the merge is not auto). A
`brand_gate: true` plan additionally routes the rendered copy/layout to a human
taste sign-off, independent of the merge decision. The guard enforces the
capability split, not convention.

## Per-surface oracle skills

Tasks are novel, but the **surfaces** they verify against are a small fixed set,
so the verification harness can be reused per surface instead of hand-built each
time. The classifier names an `oracles[]` set per surface (see the table in the
`classify-surface` skill); each name resolves to an **oracle skill** in the pack
that exercises that surface in-pod against test-mode / replay and produces the
evidence the gate later demands. The example pack ships one oracle skill per
surface class:

| Surface | Oracle skill | Exercises | Evidence |
| --- | --- | --- | --- |
| `pure-calc` | `golden-tests` | unit + property tests + golden-file diffs (no I/O) | `test-run` |
| `frontend` / `brand-visible` | `render-check` | headless-browser render of the changed UI in-pod | `screenshot` / `video` |
| `backend` | `endpoint-tests` | route/job against a throwaway test instance in-pod | `test-run` |
| `external-api` | `roundtrip-check` | third party against test-mode / VCR cassette + contract tests | `transcript` |
| `webhook` | `contract-check` | recorded payload fixtures replayed at the handler (sig / idempotency) | `transcript` |
| `payments` | `roundtrip-check` + `contract-check` | outbound charge (test mode) + inbound event (replayed) | `transcript` |

Three properties make the oracles safe to run unattended in the pod:

- **Test-mode / replay, never prod.** No oracle touches a third party's
  production endpoint. `roundtrip-check` replays a recorded cassette (or hits the
  provider's sandbox); `contract-check` replays checked-in payload fixtures at
  the local handler. The cassettes/fixtures/goldens are reviewed code — a
  re-recorded one shows up in the `diff`, and unexplained churn is a red flag.
- **Net-allow scoped per surface.** The guard grants each oracle only the
  network its surface needs: `pure-calc`/`webhook` get no third-party host at
  all (local goldens / replayed fixtures), `frontend`/`backend` reach only the
  loopback dev server, and `external-api`/`payments` get the provider's
  **sandbox** host — never its production host. `guard-rules.md` carries the
  per-surface `net-*-sandbox` sets. A change that needs a host its surface's set
  does not grant was mis-classified — stop and re-run `intent-acceptance`.
- **Oracle ⇒ autonomy.** `golden-tests` is the only oracle whose green run
  unlocks `auto-merge` (the surface is deterministic, the oracle *is* the
  reviewer). Every other oracle proves the behaviour *works* but not that it is
  *wanted*, so its surface stays `human-gate` regardless of how green the run is.

Step 3 of an implementation prompt runs **exactly** the `oracles[]` the
classifier selected — no more, no fewer — and the evidence they emit is what the
done gate consumes. A multi-surface change runs the union of oracles.

## Evidence-required done gate

The mirror of the ratify gate at the *other* end of the run. The Intent+
Acceptance gate catches a misread before code is written; the evidence gate stops
a confident-but-wrong deliverable from being finalized on an **assertion** —
"evidence, not assertions". An implementation prompt should make its **last** step
an evidence gate that refuses the finalize / open-PR transition until the result
carries a populated `evidence[]` for the surfaces the change touched. The pattern
lives in the example pack as the `evidence-gate` skill plus the final step of
`prompts/example-task.md`.

After the change is made and the deployed change is run against the Acceptance
script (verbatim, from Step 1), the agent assembles `evidence[]` — typed,
self-contained artifacts that *show* each acceptance condition holds:

- `test-run` — the command + its full output (exit status visible).
- `diff` — the unified diff / key hunks (required for every surface; bounds what
  was touched).
- `screenshot` / `video` — UI behaviour for `frontend` / `brand-visible`.
- `transcript` — the real round-trip for `external-api` / `webhook` / `payments`.
- `coverage` — the coverage delta where a `test-run` is required (never a
  substitute for one).

The required kind(s) are keyed per surface class (`pure-calc` → `test-run`+`diff`,
`frontend` → `screenshot`+`diff`, `external-api`/`webhook`/`payments` →
`transcript`+`diff`, etc.); the `evidence-gate` skill carries the full table.

The gate is enforced by the guard, not by convention: `remote-write` (push /
open-PR) is granted **only** in the final step, so the agent cannot finalize from
an earlier step — the evidence step is the only door to a PR. A `success` callback
on a code-touching flow **must** carry a non-empty `evidence[]` (see *Result
callback*); a deliverable that cannot produce its required evidence reports
`needs_human` instead. The evidence is rendered on the PR body (one section per
surface) so human review is a glance at the proof, not a re-run of the app.

## Guard hardening: re-injection + deterministic transition gates

Long sessions drift. Past the halfway mark, the agent's working context dilutes
(the "dumb zone") and any ticket, comment, or web page it fetched is sitting in
that context as if it were an instruction — a prompt-injection surface. And every
step transition so far has trusted the agent's *claim* that the step is done.
CCT-440 hardens both ends of a transition; both are enforced by the guard, not by
convention.

**Re-inject the step on every transition.** A numeric transition (and the
`SessionStart`/compact hook) returns the **authoritative next-step prompt body
verbatim** — the trusted instructions from the pack, not the agent's own summary.
The agent re-anchors on the trusted spec instead of a diluted or injected one.
The body is captured from the prompt step's prose lines; no annotation is needed.

**Compaction is opt-in (`[compact]`).** A step may add a `[compact]` line to also
emit a directive to compact the working context to `{plan, current diff, the step
instructions}` and to treat any fetched ticket/comment/web content as untrusted
input rather than instructions. It is **off by default** (CCT-450): compaction is
lossy and counter-productive on large-context models, so re-injection re-anchors
context without discarding it unless the step explicitly asks for it. Bare
`[compact]` turns it on; `[compact]: false` (or `no`/`off`/`0`) keeps it off so a
template can carry the line and toggle it per task.

**Deterministic transition gates.** Where completion is machine-checkable, the
step carries a `[gate]: <command>` annotation — a deterministic check the guard
runs (in the worker's `/workspace`) before it will allow the transition *out* of
that step. A non-zero exit **refuses** the transition and returns the command's
output; the agent cannot advance past a finalize-type step on an assertion of
"done". This is the structural form of "evidence, not assertions": prefer a gate
(`cargo test`, an artifact check, CI status) wherever the outcome is
deterministically checkable, and reserve the adversarial-agent validator only for
transitions that are *not* — a judge sharing the agent's blind spots is a weaker
gate than a green test. `Exit` always bypasses the gate (bail-out must work; the
agent reports the blocked outcome via the `needs_human` callback rather than
finalizing).

The two compose with the existing gates: the Intent+Acceptance ratify gate
(Step 1) catches a misread before code; the per-surface oracles (Step 3) produce
evidence; a `[gate]` makes the *transition into finalize* refuse to fire unless
that evidence is real; and the re-injection keeps the agent anchored on the
ratified spec the whole way. The example pack wires a `[gate]` on the implement
step of `prompts/example-task.md`.

## Consistency gates + discoverability

The oracles prove the change *works* and the evidence gate proves it is
finalized on proof — but neither catches the change that is **correct yet
inconsistent**: a banned API, a crossed layering boundary, logic re-implemented
that already exists. That class is ~80% **human-caught** at review (the
review-mining finding); the bot passes correctness but is blind to "we already
have this" and "we don't do it that way here". These are not verification
failures — they are **retrieval** (the agent didn't know the helper existed) and
**codification** (the convention lived in a reviewer's head) failures. CCT-441
closes both, and the close lives in the repo so it protects humans too.

**Deterministic consistency gates (codification + detection).** Three
repo-resident gates run in the implement step and chain into its `[gate]`:
structural lint (`ast-grep` / Semgrep — banned APIs, house patterns), layering
boundaries (`dependency-cruiser`), and copy-paste detection (`jscpd`). A
correct-but-inconsistent change cannot transition to finalize until they are
green; a violation is fixed (not suppressed), and a **recurring** one is codified
as a **one-rule change in the same diff** — the *ratchet* that turns a recurring
review comment into a mechanical gate. The pattern is the `consistency-gates`
skill plus the implement step's `[gate]` (a real pack chains `make
consistency-check` into the oracle gate). A suppressed rule is a `judgment`
decision, visible in the `diff` evidence.

**Prior-art search (retrieval, up front).** jscpd catches a *paste* after the
fact but cannot catch a *reinvention* — code written from scratch because the
agent never knew the helper existed. So a **generated helper/utility index**
(`docs/helper-index.md`, regenerated in CI from an export/doc-comment scan, never
hand-maintained) makes the repo's reusable surface discoverable, and the
implement step requires a **cite-prior-art** sub-step: before introducing any new
util/type/component, the agent searches the index and either cites the existing
one it will reuse or justifies a new one (`prior_art:` block). The pattern is the
`prior-art` skill. The two bracket reinvention from both sides — the citation
makes the agent reuse before it writes, the gate catches it if it pasted anyway.

The acceptance for the epic: a new mechanical convention becomes a one-rule PR,
not a recurring review comment, and the implement step cites prior art before
introducing a new utility.

## Comment handling (classify, defend-don't-cave)

The run does not end when the PR opens — reviewers comment, and the agent has to
act on those comments without **capitulating** to them. The dangerous failure
mode is not ignoring a comment but rewriting good, evidence-backed code because a
comment was *raised*: LLMs flip a correct answer to an incorrect one under mild
pushback at a measurable rate (~15% in published sycophancy evals), and the
*format* of the objection (confident tone, authority framing) drives the cave
more than its substance. The pattern lives in the example pack as the
`comment-handling` skill; a real pack adapts it to its review tooling.

The skill is the inbound mirror of the evidence gate — the gate proved the
deliverable right when the PR opened; this keeps it right while the PR is
reviewed. It runs **per inbound comment** and is deterministic on the comment's
class, so the agent cannot talk itself into "just make the change" for a judgment
comment any more than it could into a softer merge gate:

```yaml
comment:
  class: <mechanical|judgment|unclear>   # pure function of WHAT is asked, not WHO
  action: <auto-fix|defend-or-propose|escalate>
```

- **`mechanical`** (objective, one right answer — lint, rename, dead code, a
  *demonstrated* bug) → **auto-fix**, re-run the surface's oracle so the fix stays
  evidence-backed, reply with the commit, resolve the thread. A bug *asserted*
  without a failing case is `judgment` (ask for the repro), not `mechanical`.
- **`judgment`** (taste, scope, architecture, "is this worth it") → **propose or
  defend, never silently comply**. Reply with a reason that ties back to the
  ratified Intent/Acceptance and the evidence; the default for a judgment comment
  on code that already passed the gate is to **hold**. A proposal that alters
  scope/surfaces re-opens the `intent-acceptance` gate — it is not silently
  absorbed.
- **`unclear`** / contradicts the ratified Acceptance → **ask or `needs_human`**;
  a spec dispute is resolved by a human, not by changing code.

Two structural guards keep this safe: the **human merge gate is never bypassed**
(the skill answers and fixes; it does not merge — a capitulating agent still
cannot ship, so the only thing caving can damage is quality), and an unresolved
judgment thread **escalates** rather than churns — after one reasoned round-trip
the agent reports `status: "needs_human"` carrying both positions instead of
entering an edit/revert loop under repeated pushback.

> **Auto-address loop (server-side, not yet wired).** The webhook ingress
> (`crate::webhook` in `cctui-github`) already parses + stores
> `pull_request_review` / `pull_request_review_comment` events but does **not**
> dispatch a worker from them — there is no event→dispatch mechanism in the
> connector today. Wiring the review-comment event to dispatch this skill is a
> separate server change tracked outside this contract; the worker-side behaviour
> (classify + defend-don't-cave) is what the `comment-handling` skill specifies
> and is reusable the moment that loop exists.

## Deliverable-acceptance agent + preview environments

The evidence gate proves the deliverable on artifacts the **implementer** assembled
— necessary, but the implementer shares its own blind spots and an incentive to
declare success, and its evidence is built from the source tree, not a running
deployment. The one oracle neither the guard, the cross-model review, nor the
implementer's own evidence covers is *does the change actually do the intended
thing, end to end, in a running deployment*. Today a human fills that gap by
checking out the branch and exercising it in the dev stack — the primary
bottleneck in both flows. This step moves that check into the pipeline as an
attached **independent verdict**, so the human reviews a result instead of
re-running the app.

It is the **deployed-end mirror** of the bracketing gates: `intent-acceptance`
states the success condition up front, the evidence gate makes the implementer
prove it at finalize, and the **deliverable-acceptance agent** re-confirms it
against the live deployment from a clean context. The pattern lives in the example
pack as the `acceptance-agent` skill.

**Clean context — it cannot mark its own homework.** The acceptance run is
executed in a context that **never saw the implementation**: it is handed the
ratified Intent+Acceptance artifact only (the `acceptance[]` conditions and the
`surfaces[]`), **not** the diff, the implementer's transcript, or the implementer's
`evidence[]`. It re-derives a concrete test plan from each acceptance condition by
itself and drives the **deployed preview** — Playwright for a `frontend` /
`brand-visible` surface, HTTP for a `backend` / `external-api` / `payments`
surface, a payload replay for a `webhook` — never the source tree. It can only
emit a verdict + evidence; it cannot push, edit code, or merge. Separating the
grader from the author is the whole point — that is the structural reason this
verdict is worth more than the implementer's self-assertion.

It runs **when the change has observable deployed behaviour** (any medium/high
blast-radius surface — exactly the classes a human used to check out and test). A
`pure-calc`-only change is skipped: there is nothing deployed to drive and the
deterministic golden test already is the end-to-end proof. The output is an
`acceptance_run` block — a per-condition `pass|fail` plus the screenshot /
transcript / driver-output that backs it, in the same `{kind, surface, summary,
detail}` evidence shape so it renders on the PR body alongside the implementer's.
A `fail` blocks the merge regardless of what the implementer asserted; the agent
**never** edits code to make an assertion pass. The independent verdict and the
implementer's evidence gate are **composed** — both must be green for an
unattended merge.

> **Per-PR preview environments (infra — not the context pack).** The skill
> assumes a deployed preview exists at a `PREVIEW_BASE_URL` and drives whatever it
> is handed; **standing up that environment is infra, not part of the pack.** It
> needs the cluster (k8s + ArgoCD) to deploy *this PR's* build to an isolated,
> ephemeral namespace on PR open and tear it down on close (e.g. an ArgoCD
> `ApplicationSet` over a PR generator), plus the dispatch wiring that injects the
> resulting URL into a fresh, clean-context acceptance run. That provisioning +
> dispatch loop lives in the infra repo / the dispatch layer and is tracked
> outside this contract; the repo-resident half — the clean-context driver and the
> verdict contract — is what the `acceptance-agent` skill specifies and is reusable
> the moment a preview URL is provided.

## `CctuiAgent` — native subagent spawning (CCT-758)

A session can spawn **real cctui child sessions** instead of shelling out to a
runner script. The child is a first-class session: nested under its caller in
the UI (`sessions.parent_id`), metered through the gateway like any other
session, budgeted, and killable. This replaces the pattern where a session
launched a harness itself and the resulting agent was invisible to cctui.

### The tool

`cctui-daemon` serves a local **stdio MCP server** exposing exactly one tool:

```
CctuiAgent(
  adapter:       string   # required — "opencode" | "codex" | "claude_code"
  prompt:        string   # required — the task for the child
  model:         string?  # account-catalog model id; omit for the account default
  agent_profile: string?  # e.g. "cctui-reviewer" (opencode agent profile)
  budget_usd:    number?  # child's own dollar ceiling; omit to inherit the max
  cwd:           string?  # defaults to the caller's working directory
  timeout_secs:  int?     # default 1800, max 7200
) -> the child's final message
```

The call **blocks until the child finishes** and returns its last assistant
message. A child that fails, crashes, or is refused returns error text — the
call never hangs silently. On timeout the tool returns and the child is left for
inspection in the UI rather than being silently reaped.

`adapter` accepts the spellings a model is likely to produce: `claude_code` /
`claude` → `claude-code`, `codex-cli` → `codex`.

### Surviving a daemon restart mid-call

The relay talks to the daemon over a Unix socket, and an auto-update re-exec
(`execve`) drops **every** open agent-tool connection at once — which used to
fail every in-flight call together with `the daemon closed the connection
without a result`, while the children themselves kept running.

Socket protocol 3 makes the wait resumable:

- the daemon writes an `{"attached": "<child-id>"}` frame as soon as the child
  is known, before the first wait, so the relay always has an id to come back
  to — not only after the first 15s progress frame;
- on EOF the relay reconnects (up to 3min, 5 reattaches) and issues
  `{"kind": "follow_agent", "args": {"session_id": "<child-id>"}}`, which
  reattaches to the running child and **sends it nothing**: re-prompting would
  make it redo work;
- a read timeout is never treated as a restart — the daemon owes a result
  there, and reattaching would loop forever.

When the daemon dies before naming the child, or keeps restarting, the call
still fails, but the text says the daemon restarted and points at the child
rather than blaming it.

### Registration

Only **claude sessions** get the tool today. At launch the daemon writes a
per-session MCP config (`mcp-agent-<short>.json`) beside the managed
`--settings` file and passes it as `--mcp-config`. Argv bakes in the session id
and the daemon's tool socket:

```
cctui-daemon mcp-agent --session <session-id> --sock <daemon-agent-socket>
```

Because the session id is fixed in argv by the daemon, a session can never make
a call on another session's behalf. The config is written **only when the server
returns a spawn capability** for that session, so an unprivileged session does
not even see the tool.

When the tool is written, the spawn's `<session-context>` block also names it:
the permitted adapters, the budget ceiling and one example call. A tool listed
only in the MCP inventory goes unused.

An account-bound spawn mints gateway env for **every provider family the account
carries**, not just the adapter's own — so a claude session holds the
`OPENAI_*` keys too and can spawn (or shell out to) codex under the same
account. The launch still fails when the adapter's own family has no provider.

### Capability (fail-closed)

What a session may spawn is declared by whoever launches it and is never
writable by the session:

- interactive spawns: `spawn_capability` on the `SpawnRequest`. Omitted → the
  session gets the **machine default**: every known adapter, a $20 per-child
  ceiling, no child cap. A session launched on the user's own machine is trusted
  to spawn there; send an explicit capability to narrow it.
- dispatched workers: `payload.spawn_capability`, which the server **strips from
  the forwarded payload** so the worker cannot read or restate it. Absent here
  still means no tool — dispatched workers get no default.

```jsonc
"spawn_capability": {
  "adapters": ["opencode"],   // exact ids; empty or absent = spawning denied
  "max_budget_usd": 0.50,     // ceiling AND the default when a call omits one
  "max_children": 3           // total children over the session's life
}
```

Enforcement lives server-side in
`POST /api/v1/daemon/sessions/{id}/spawn-child` (machine-key auth), which the
daemon relays to. It denies when: no capability is recorded, the adapter is not
listed, `budget_usd` exceeds `max_budget_usd` (or one is requested with no
ceiling set), or the session already has `max_children`. The daemon grants
nothing on its own. Capabilities are persisted in `session_spawn_capabilities`
and cached in server memory, so a restart no longer silently disarms a live
session's spawn tool; a lookup that errors still denies.

Children are **not** granted a capability, so a child cannot spawn further
children.

### Budgets

`budget_usd` becomes a `session_usd` soft limit on the child (CCT-757 dollar
windows), overlaid at the gateway on top of the account's own limits and
enforced with the existing 429 path. A child's budget always wins over a looser
account-level `session_usd`. The child mints its own gateway credential under
the **parent's account identity** for its own harness family, so a claude parent
can spawn an opencode/Fireworks child on the same account.

`budget_usd` is optional and usually unnecessary: the account's own `session_usd`
cap already bounds a child, gateway-side. Naming a ceiling in the launcher only
adds a second limit that can deny the spawn. Prefer the account cap.

A harness that names its own session (opencode returns `ses_…`) is metered under
the spawn key until it registers; `rebind_spawn_key` moves the token, the recorded
usage, the child's budget and its capability onto the registered id in one step.
Miss that and the usage FK to `sessions(id)` rejects every insert and the child's
spend reads $0.
