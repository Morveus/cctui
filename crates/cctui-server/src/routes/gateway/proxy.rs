use super::{
    Family, FireworksSettings, anthropic_upstream, clear_account_reauth, clear_soft_limit_block,
    clear_soft_limit_block_for_token, current_access_token, durable_block_key, fireworks_upstream,
    flag_account_reauth, mark_soft_limit_block, note_orphan_401, note_token_used, openai_upstream,
    orphan_is_blocked, record_fireworks_usage, resolve_account, session_and_account_name_for_token,
    session_budget_limits, session_id_for_token, session_spend_usd, tees_response,
    usage_for_soft_limit,
};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use futures_util::StreamExt;

use crate::state::AppState;

/// `/gateway/anthropic/*path` — passthrough to api.anthropic.com.
/// Which side of the gateway rejected an authenticated request. The
/// two are easy to confuse from a worker's point of view — both surface as a
/// 401 — but they need opposite remedies, so we label every gateway 401 with
/// one of these in both the body message and the `x-cctui-auth-stage` header.
#[derive(Clone, Copy)]
pub enum AuthStage {
    /// cctui itself rejected the inbound `cctui_s_…` session token: unknown,
    /// revoked, or not bound to an account. The LLM login is irrelevant here.
    SessionToken,
    /// cctui accepted the session token and mapped it to an account, but the
    /// upstream LLM provider rejected that account's OAuth credentials (expired
    /// / revoked refresh token, failed refresh, upstream 401). The cctui token
    /// is fine; the account needs re-authenticating.
    ProviderOauth,
}

/// Build a labeled 401 response. The body uses the provider's native
/// error envelope so the CLI surfaces the message verbatim, and the
/// `x-cctui-auth-stage` header makes the cause machine-readable in logs/clients.
pub fn auth_error(stage: AuthStage, is_anthropic: bool) -> Response {
    let (stage_tag, message) = match stage {
        AuthStage::SessionToken => (
            "session-token",
            "cctui gateway rejected the session token: the cctui_s_ credential is \
             unknown, revoked, or not bound to an account. This is a cctui gateway \
             credential problem, NOT an LLM provider login problem — re-create or \
             re-resume the session to mint a fresh token.",
        ),
        AuthStage::ProviderOauth => (
            "provider-oauth",
            "cctui accepted the session token, but the upstream LLM provider returned \
             401 for the bound account's OAuth credentials. The cctui token is valid — \
             re-authenticate the LLM account in cctui.",
        ),
    };
    // Native error envelopes: Anthropic `{type:error, error:{type,message}}`;
    // OpenAI `{error:{message,type}}`. Both render the message in the CLI.
    let body = if is_anthropic {
        serde_json::json!({
            "type": "error",
            "error": { "type": "authentication_error", "message": message },
        })
    } else {
        serde_json::json!({
            "error": { "message": message, "type": "authentication_error" },
        })
    };
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(http::header::CONTENT_TYPE, "application/json")
        .header("x-cctui-auth-stage", stage_tag)
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| StatusCode::UNAUTHORIZED.into_response())
}

/// Refuse a request the soft limit blocks: fail it over to a sibling account
/// with headroom if there is one, else flag the session and 429.
///
/// `model` is what the sibling must have room for; `None` (model not yet read
/// off the body) elects conservatively, counting every window.
#[allow(clippy::too_many_arguments)]
async fn soft_limit_refusal(
    state: &AppState,
    session_token: &str,
    acct: &super::Account,
    is_anthropic: bool,
    model: Option<&str>,
    retry_after_secs: i64,
    reason: String,
    block_key: String,
) -> Result<Response, StatusCode> {
    // Before refusing with the account's own reset horizon, try to rebind the
    // session to a sibling with headroom — the worker's 429 retry then lands on
    // the new account instead of stalling.
    if let Some(target) = super::pick_failover_target(state, session_token, acct.id, model).await
        && super::rebind_session(state, &target, acct.id).await
    {
        return Ok(super::failover_retry_response(
            &target.account_name,
            target.reason,
            is_anthropic,
        ));
    }
    // Surface the block as a per-session signal so the webui can offer
    // "continue on another account". Best-effort + dedup'd.
    if let Some((session_id, account_name)) =
        session_and_account_name_for_token(state, session_token).await
    {
        mark_soft_limit_block(
            state,
            &session_id,
            acct.id,
            &account_name,
            &reason,
            &block_key,
            retry_after_secs,
        )
        .await;
    }
    Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header(http::header::RETRY_AFTER, retry_after_secs.to_string())
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::json!({ "error": reason }).to_string()))
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)
}

pub async fn anthropic(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response, StatusCode> {
    passthrough(state, req, "/gateway/anthropic", &anthropic_upstream()).await
}

/// `/gateway/openai/*path` — passthrough to api.openai.com.
pub async fn openai(State(state): State<AppState>, req: Request) -> Result<Response, StatusCode> {
    passthrough(state, req, "/gateway/openai", &openai_upstream()).await
}

/// `/gateway/fireworks/*path` — passthrough to Fireworks' OpenAI-compatible API.
///
/// A sibling route rather than a branch inside [`openai`]: the two differ in
/// upstream, in the worker env pair they are reached by, and in that this one
/// mutates the request (per-account [`FireworksSettings`]). Sharing the openai
/// route would put a per-account conditional on codex's hot path for nothing.
pub async fn fireworks(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response, StatusCode> {
    passthrough(state, req, "/gateway/fireworks", &fireworks_upstream()).await
}

pub fn skip_request_header(lower_name: &str) -> bool {
    // `x-openai-actor-authorization` is a dummy the codex provider config carries
    // purely to unlock the built-in image_gen tool; it must never reach upstream.
    matches!(
        lower_name,
        "authorization" | "host" | "content-length" | "connection" | "x-openai-actor-authorization"
    )
}

pub fn skip_response_header(lower_name: &str) -> bool {
    matches!(lower_name, "connection" | "transfer-encoding" | "content-length")
}

/// The exact bytes to forward upstream, and whether they differ from what the
/// client sent.
///
/// Re-serializing a parsed body re-emits every JSON object with its keys sorted
/// (workspace `serde_json` has no `preserve_order`), which reorders
/// `tool_use.input` and changes every token after the first tool call — the
/// whole prompt cache is lost. So only a provider that must be reshaped gets a
/// re-serialized body, and Anthropic never does: its bytes are forwarded
/// verbatim no matter which features are on.
fn upstream_payload(
    original: &axum::body::Bytes,
    parsed: Option<&serde_json::Value>,
    fireworks: Option<&FireworksSettings>,
    affinity_session: Option<&str>,
) -> (axum::body::Bytes, bool) {
    let (Some(fw), Some(json)) = (fireworks, parsed) else {
        return (original.clone(), false);
    };
    let mut reshaped = json.clone();
    fw.apply_body(&mut reshaped, affinity_session);
    if reshaped == *json {
        return (original.clone(), false);
    }
    (axum::body::Bytes::from(reshaped.to_string()), true)
}

// Linear proxy pipeline (auth, account-resolve, refresh, forward, stream);
// complexity/length are per-stage handling, not nesting.
#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
pub async fn passthrough(
    state: AppState,
    req: Request,
    prefix: &str,
    upstream_base: &str,
) -> Result<Response, StatusCode> {
    let is_anthropic = prefix.contains("anthropic");

    // The worker's bearer is the session token; map it to an account. A missing
    // bearer or one that doesn't resolve is a *cctui* rejection — distinguish it
    // from a provider rejection so the worker/operator knows which to fix.
    let Some(session_token) = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
    else {
        return Ok(auth_error(AuthStage::SessionToken, is_anthropic));
    };

    // Orphan-spam guard: an unbound worker retries `/gateway` forever; each retry
    // that reaches the DB starves the pool. Fingerprint the token and, if it is
    // already flagged as a spamming orphan, drop the request *before* the DB.
    let token_fp = crate::auth::sha256_hex(&session_token);
    if orphan_is_blocked(&state, &token_fp) {
        return Ok(auth_error(AuthStage::SessionToken, is_anthropic));
    }

    let acct = match resolve_account(&state, &session_token).await {
        Ok(Some(acct)) => {
            note_token_used(&state, &token_fp);
            acct
        }
        // Genuinely unknown/revoked/unbound token — a real orphan. Count it
        // toward the spam guard and reject as a cctui auth failure.
        Ok(None) => {
            note_orphan_401(&state, &token_fp);
            return Ok(auth_error(AuthStage::SessionToken, is_anthropic));
        }
        // The DB lookup itself failed (cold/starved pool during a server
        // restart, transient network). This is NOT an orphan — a valid bound
        // token can land here while the pool warms up. Returning a retryable
        // 503 (and crucially NOT feeding the orphan-spam block) keeps a server
        // restart from poisoning live tokens for 300s.
        Err(e) => {
            tracing::warn!(
                stage = "session-token",
                error = %e,
                "gateway token resolution failed transiently (DB) — returning 503, not orphaning"
            );
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
    };

    // Soft limit: cap cctui's own share of the account's usage windows
    // so it leaves headroom for the human sharing the subscription. Only the
    // configured windows gate; bypass near reset.
    //
    // The usage cache is warmed only by the accounts-page route, which headless
    // dispatch never opens, so on the dispatch path we refresh it from upstream
    // when cold/stale (throttled by the same TTL to avoid spamming Anthropic's
    // rate-limited endpoint) before evaluating. Fetch errors fail open.
    // A `CctuiAgent` child carries its own `session_usd` cap, which the account's
    // stored limits know nothing about. Overlay it here; the map is empty on the
    // ordinary path, so this costs a lock-free length check per request.
    let effective_limits = session_budget_limits(&state, &acct, &session_token).await;
    let mut model_gate: Option<Vec<crate::soft_limit::UsageWindow>> = None;
    if !effective_limits.is_unset() {
        let cached = usage_for_soft_limit(&state, acct.id).await;
        let mut windows =
            cached.as_ref().map(crate::soft_limit::normalize_usage_windows).unwrap_or_default();
        // The per-session budget is session-scoped, so it can't come from the
        // per-account usage cache — resolve it here, and only when one is set.
        if effective_limits.limits.contains_key(crate::soft_limit::KEY_SESSION_USD)
            && let Some(session_id) = session_id_for_token(&state, &session_token).await
            && let Some(spent) = session_spend_usd(&state, acct.id, &session_id).await
        {
            windows.push(crate::soft_limit::usd_window(
                crate::soft_limit::KEY_SESSION_USD,
                spent,
                None,
            ));
        }
        // A `weekly_model:` cap can only be judged once the request's model is
        // known, which needs the body. Gate the model-blind windows here so the
        // zero-copy passthrough survives for every account without such a cap,
        // and defer the scoped ones to `model_soft_limit_gate` below.
        let scoped_caps =
            effective_limits.limits.keys().any(|k| crate::soft_limit::is_model_scoped_key(k));
        let unscoped: Vec<crate::soft_limit::UsageWindow> = windows
            .iter()
            .filter(|w| !crate::soft_limit::is_model_scoped_key(&w.key))
            .cloned()
            .collect();
        if let crate::soft_limit::Decision::Block { retry_after_secs, reason, key } =
            crate::soft_limit::evaluate_soft_limit(&unscoped, &effective_limits, None, Utc::now())
        {
            tracing::info!(account = %acct.id, retry_after_secs, "soft limit hit: {reason}");
            return soft_limit_refusal(
                &state,
                &session_token,
                &acct,
                is_anthropic,
                None,
                retry_after_secs,
                reason,
                durable_block_key(&acct.soft_limits, &effective_limits, &key),
            )
            .await;
        }
        if scoped_caps {
            model_gate = Some(windows);
        }
    }

    // Gateway rate limit: a pay-per-token provider's RPM/TPM tier is shared by
    // every session on the account, so throttle at the proxy. Requests count on
    // admission; tokens accrue when a response's usage lands below. Unset ⇒ skip.
    if !acct.rate_limits.is_unset()
        && let Err(retry_after_secs) = super::admit_request(&state, acct.id, &acct.rate_limits)
    {
        tracing::info!(account = %acct.id, retry_after_secs, "gateway rate limit hit");
        let resp = Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header(http::header::RETRY_AFTER, retry_after_secs.to_string())
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "error": "gateway rate limit exceeded" }).to_string(),
            ))
            .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
        return Ok(resp);
    }

    // The session token is valid (resolved above); a failure to obtain an
    // upstream access token here is a provider-credential problem (no/expired
    // refresh token, failed refresh) — label it as such.
    let Ok(access_token) = current_access_token(&state, &acct).await else {
        tracing::warn!(account = %acct.id, stage = "provider-oauth", "gateway 401: no upstream access token for account");
        flag_account_reauth(&state, acct.id, "no upstream access token (refresh failed)");
        return Ok(auth_error(AuthStage::ProviderOauth, is_anthropic));
    };

    // Per-account upstream: a compatible endpoint overrides the
    // built-in upstream with its stored `base_url`; native subscription accounts
    // fall back to the built-in `api.anthropic.com`/`chatgpt.com`.
    let upstream =
        acct.base_url.as_deref().filter(|u| !u.trim().is_empty()).unwrap_or(upstream_base);

    // Build the upstream URL: strip the gateway prefix, keep path + query.
    let path = req.uri().path();
    let tail = path.strip_prefix(prefix).unwrap_or(path);
    let query = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
    let url = format!("{}{tail}{query}", upstream.trim_end_matches('/'));

    let method = reqwest::Method::from_bytes(req.method().as_str().as_bytes())
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    // Preserve every client header verbatim except hop-by-hop + the bearer we
    // are swapping and the Host (reqwest sets it from the upstream URL).
    let mut headers = HeaderMap::new();
    for (name, value) in req.headers() {
        let n = name.as_str().to_ascii_lowercase();
        if skip_request_header(&n) {
            continue;
        }
        if let (Ok(hn), Ok(hv)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            headers.insert(hn, hv);
        }
    }
    headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("Bearer {access_token}"))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    );
    // ChatGPT-backed Codex requests must carry the account id upstream. Other
    // families store an unrelated id in this column (Fireworks: the billing
    // account slug), so the header is scoped rather than sent on every request.
    if Family::from_provider(&acct.provider) == Family::Openai
        && let Some(account_id) = acct.provider_account_id.as_deref()
        && let Ok(hv) = reqwest::header::HeaderValue::from_str(account_id)
    {
        headers.insert("chatgpt-account-id", hv);
    }

    // Fireworks: the account's settings shape the request here, where the real
    // key lives — a worker can neither supply nor defeat them.
    let fireworks = (Family::from_provider(&acct.provider) == Family::Fireworks)
        .then(|| FireworksSettings::resolve(acct.provider_settings.as_ref()));
    let affinity_session = match fireworks.as_ref() {
        Some(fw) if fw.session_affinity => session_id_for_token(&state, &session_token).await,
        _ => None,
    };
    if let Some(sid) = affinity_session.as_deref()
        && let Ok(hv) = reqwest::header::HeaderValue::from_str(sid)
    {
        headers.insert("x-session-affinity", hv);
    }

    // Langfuse tracing sink: only when configured AND this call is
    // sampled do we reconstruct the bodies — otherwise the gateway stays a pure
    // zero-copy passthrough (request streamed, response streamed). When tracing,
    // we buffer the request body (it is the prompt, already fully in flight) so it
    // can be both forwarded upstream and used as the generation input; the trace
    // reads a parsed copy and the original bytes still go upstream.
    let langfuse = state.langfuse.clone().filter(|lf| lf.should_sample());
    let trace_session_id =
        if langfuse.is_some() { session_id_for_token(&state, &session_token).await } else { None };

    if tees_response(langfuse.is_some(), fireworks.is_some()) {
        headers.remove(reqwest::header::ACCEPT_ENCODING);
    }

    // Stream the request body through without buffering (default), OR buffer it
    // once when something needs to read or reshape it. A body that isn't JSON
    // falls through to the original bytes, so non-`/v1/messages` calls are
    // untouched either way.
    let mut request_model: Option<String> = None;
    let mut rewrote_body = false;
    let (upstream_body, traced_request) =
        if langfuse.is_some() || fireworks.is_some() || model_gate.is_some() {
            let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            let parsed = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
            request_model = parsed
                .as_ref()
                .and_then(|r| r.get("model"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            // The deferred half of the soft limit: a `weekly_model:` cap gates only
            // the model it names, so a spent weekly Fable budget must let an Opus
            // request through. Same `window_applies` the account election uses, so
            // the two can never disagree.
            if let Some(windows) = model_gate
                && let crate::soft_limit::Decision::Block { retry_after_secs, reason, key } =
                    crate::soft_limit::evaluate_soft_limit(
                        &windows,
                        &effective_limits,
                        request_model.as_deref(),
                        Utc::now(),
                    )
            {
                tracing::info!(
                    account = %acct.id,
                    model = request_model.as_deref().unwrap_or("unknown"),
                    retry_after_secs,
                    "soft limit hit: {reason}"
                );
                return soft_limit_refusal(
                    &state,
                    &session_token,
                    &acct,
                    is_anthropic,
                    request_model.as_deref(),
                    retry_after_secs,
                    reason,
                    durable_block_key(&acct.soft_limits, &effective_limits, &key),
                )
                .await;
            }
            let (payload, changed) = upstream_payload(
                &bytes,
                parsed.as_ref(),
                fireworks.as_ref(),
                affinity_session.as_deref(),
            );
            rewrote_body = changed;
            (reqwest::Body::from(payload), parsed.filter(|_| langfuse.is_some()))
        } else {
            let body_stream = req.into_body().into_data_stream();
            (reqwest::Body::wrap_stream(body_stream), None)
        };

    let upstream = state
        .http_client
        .request(method, &url)
        .headers(headers)
        .body(upstream_body)
        .send()
        .await
        .map_err(|e| {
            tracing::error!(account = %acct.id, "gateway upstream error: {e}");
            StatusCode::BAD_GATEWAY
        })?;

    // Opportunistic stats: request count + response byte count (no buffering).
    let resp_len = i64::try_from(upstream.content_length().unwrap_or(0)).unwrap_or(i64::MAX);
    let acct_id = acct.id;
    let pool = state.pool.clone();
    tokio::spawn(async move {
        let _ = sqlx::query(
            "UPDATE account_providers SET request_count = request_count + 1, \
                    bytes_transferred = bytes_transferred + $2, last_used_at = now() \
             WHERE id = $1",
        )
        .bind(acct_id)
        .bind(resp_len)
        .execute(&pool)
        .await;
    });

    // Mirror status + headers back to the client untouched (retry-after, 429,
    // 529, SSE content-type — all verbatim) and stream the body.
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    // The session token was accepted by cctui (we got this far), so a 401 from
    // the upstream provider means the account's OAuth credentials are bad, not
    // the cctui token. Replace the opaque upstream 401 with a labeled one so the
    // worker/operator re-authenticates the account rather than the session.
    if status == StatusCode::UNAUTHORIZED {
        tracing::warn!(account = %acct.id, stage = "provider-oauth", "gateway 401: upstream provider rejected account credentials");
        flag_account_reauth(&state, acct.id, "upstream provider rejected account credentials");
        return Ok(auth_error(AuthStage::ProviderOauth, is_anthropic));
    }

    // Upstream says the account is rate-limited. Mirroring it verbatim strands
    // the session until the window resets (hours, for a spent 5h/weekly
    // window) — if a sibling account has allocation, rebind and send the
    // worker straight back around. A response was received but no body bytes
    // were forwarded yet, so discarding it is safe. Failovers are cooled down
    // per session, so a burst-RPM 429 doesn't ping-pong the binding.
    if status == StatusCode::TOO_MANY_REQUESTS
        && let Some(target) =
            super::pick_failover_target(&state, &session_token, acct.id, request_model.as_deref())
                .await
        && super::rebind_session(&state, &target, acct.id).await
    {
        return Ok(super::failover_retry_response(
            &target.account_name,
            target.reason,
            is_anthropic,
        ));
    }

    // A successful upstream call clears any soft-limit block on this session:
    // after the user switches accounts (or a window resets) the next
    // 2xx dismisses the banner. Unconditional: another replica may hold the block.
    if status.is_success() {
        match &trace_session_id {
            Some(sid) => clear_soft_limit_block(&state, sid).await,
            None => clear_soft_limit_block_for_token(&state, &session_token).await,
        }
    }
    // Usage ticker. Out of band on purpose: it reaches the agent as its own
    // turn, so nothing here can reshape the request that was just forwarded.
    if status.is_success() {
        super::usage_notices::deliver_if_due(&state, &acct, &session_token).await;
    }
    // A successful upstream call means the account's credentials are good again —
    // clear any reauth flag. Gated in-memory, so this is free unless the
    // account was actually flagged.
    if status.is_success() {
        clear_account_reauth(&state, acct.id);
    }
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream.headers() {
        let n = name.as_str().to_ascii_lowercase();
        if skip_response_header(&n) {
            continue;
        }
        if let (Ok(hn), Ok(hv)) = (
            axum::http::HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            builder = builder.header(hn, hv);
        }
    }
    // Fireworks meters per token, so its usage must be recorded from the
    // response itself: the `usage` object (JSON body or terminal SSE frame) plus
    // the two headers, which are what the provider bills against. Read the
    // headers now, before the body is consumed.
    let usage_session = match (&fireworks, status.is_success()) {
        (Some(_), true) => match affinity_session.clone().or_else(|| trace_session_id.clone()) {
            Some(sid) => Some(sid),
            None => session_id_for_token(&state, &session_token).await,
        },
        _ => None,
    };
    let usage_headers = usage_session.as_ref().map(|_| {
        let header = |name: &str| {
            upstream.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned)
        };
        (header("fireworks-prompt-tokens"), header("fireworks-cached-prompt-tokens"))
    });

    // Fast path (nothing to observe): stream the response straight through.
    if langfuse.is_none() && usage_session.is_none() {
        let resp_stream = upstream.bytes_stream();
        return builder.body(Body::from_stream(resp_stream)).map_err(|e| {
            tracing::error!("gateway response build error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        });
    }

    // Observed path: tee the response body. Each chunk is forwarded to the client
    // verbatim AND copied into an accumulator task over a bounded channel. The
    // copy is best-effort — if the task lags, `try_send` drops the chunk
    // (we lose the trace/usage, never the proxied bytes). When the upstream stream
    // ends the channel closes and the task reconstructs the trace and the metered
    // usage. Nothing here blocks or delays the client stream.
    let ctx = crate::langfuse::TraceContext {
        session_id: trace_session_id,
        account_id: Some(acct.id.to_string()),
        model: request_model.clone(),
    };
    // Fireworks speaks the OpenAI wire protocol, so it reconstructs as openai.
    let is_openai = Family::from_provider(&acct.provider) != Family::Anthropic;
    let pool = state.pool.clone();
    let bust_pool = state.pool.clone();
    // TPM accounting rides the same per-response usage the metering path captures
    // (Fireworks): the running window total the next request gates against.
    let rate_windows = acct.rate_limits.tpm.is_some().then(|| state.gateway_rate_windows.clone());
    let rate_provider = acct.id;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    tokio::spawn(async move {
        let mut buf = Vec::new();
        while let Some(chunk) = rx.recv().await {
            buf.extend_from_slice(&chunk);
        }
        if let (Some(session_id), Some((prompt_hdr, cached_hdr))) = (usage_session, usage_headers)
            && let Some(captured) = crate::cost::parse_fireworks_usage(
                &buf,
                prompt_hdr.as_deref(),
                cached_hdr.as_deref(),
            )
        {
            if let Some(windows) = &rate_windows {
                let u = &captured.usage;
                let total = u64::try_from(u.input).unwrap_or(0)
                    + u64::try_from(u.cached_input).unwrap_or(0)
                    + u64::try_from(u.output).unwrap_or(0);
                super::note_tokens(windows, rate_provider, total);
            }
            record_fireworks_usage(pool, session_id, request_model, captured, rewrote_body).await;
        }
        if let Some(langfuse) = langfuse {
            let (output, usage) = if is_openai {
                crate::langfuse::reconstruct_openai(&buf)
            } else {
                crate::langfuse::reconstruct_anthropic(&buf)
            };
            let (level, status_message) = crate::cache_bust::trace_annotation(
                &bust_pool,
                ctx.session_id.as_deref(),
                ctx.model.as_deref(),
                usage.as_ref(),
                rewrote_body,
            )
            .await;
            langfuse.trace(crate::langfuse::TracePayload {
                ctx,
                request: traced_request,
                output,
                usage,
                level,
                status_message,
            });
        }
    });

    let resp_stream = upstream.bytes_stream().map(move |chunk| {
        if let Ok(bytes) = &chunk {
            // Drop on backpressure rather than block the proxied response.
            let _ = tx.try_send(bytes.to_vec());
        }
        chunk
    });
    builder.body(Body::from_stream(resp_stream)).map_err(|e| {
        tracing::error!("gateway response build error: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

#[cfg(test)]
mod tests {
    use super::{FireworksSettings, skip_request_header, upstream_payload};

    #[test]
    fn actor_authorization_dummy_is_stripped_before_forwarding() {
        assert!(skip_request_header("x-openai-actor-authorization"));
        assert!(!skip_request_header("chatgpt-account-id"));
    }

    /// A body whose `tool_use.input` keys are not alphabetical, carried through
    /// the buffered anthropic path with tracing on. Re-serializing would sort
    /// them and invalidate the cached prefix from the first tool call onward.
    const fn unsorted_body() -> axum::body::Bytes {
        axum::body::Bytes::from_static(
            br#"{"model":"claude-opus-5","messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/a","old_string":"x","new_string":"y"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}],"thinking":{"type":"adaptive","display":"omitted"}}"#,
        )
    }

    #[test]
    fn anthropic_forwards_the_client_bytes_verbatim() {
        let original = unsorted_body();
        let parsed = serde_json::from_slice::<serde_json::Value>(&original).unwrap();
        let (payload, rewritten) = upstream_payload(&original, Some(&parsed), None, None);
        assert!(!rewritten);
        assert_eq!(payload, original);
        // The trace may read the parsed copy; it is never what is forwarded.
        assert_ne!(
            axum::body::Bytes::from(parsed.to_string()),
            original,
            "test body must actually be key-order-sensitive"
        );
    }

    #[test]
    fn fireworks_may_reshape_and_reports_it() {
        let original = axum::body::Bytes::from_static(br#"{"model":"kimi","messages":[]}"#);
        let parsed = serde_json::from_slice::<serde_json::Value>(&original).unwrap();
        let fw = FireworksSettings::resolve(None);
        let (payload, rewritten) =
            upstream_payload(&original, Some(&parsed), Some(&fw), Some("s1"));
        assert!(rewritten);
        let out = serde_json::from_slice::<serde_json::Value>(&payload).unwrap();
        assert_eq!(out["context_length_exceeded_behavior"], "error");
        assert_eq!(out["user"], "s1");
    }

    #[test]
    fn a_non_json_body_is_forwarded_untouched() {
        let original = axum::body::Bytes::from_static(b"not json");
        let fw = FireworksSettings::resolve(None);
        let (payload, rewritten) = upstream_payload(&original, None, Some(&fw), None);
        assert!(!rewritten);
        assert_eq!(payload, original);
    }
}
