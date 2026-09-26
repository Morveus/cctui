//! Optional, config-gated Langfuse tracing sink for the `/gateway` proxy.
//!
//! Langfuse is **not** a forward proxy — it is an ingestion API beside the path.
//! The gateway already terminates + re-signs every worker call (worker ->
//! `/gateway` -> Anthropic) and therefore sees cleartext prompt + completion +
//! usage. This module is a **second, async, fire-and-forget sink** on that same
//! handler: when configured, the gateway hands it the (already reconstructed)
//! request/response payload and it POSTs a single trace + `generation` to
//! `<host>/api/public/ingestion`.
//!
//! ## Guarantees
//! - **Config-gated.** [`LangfuseConfig::from_env`] returns `None` unless host +
//!   both keys are set. Absent => the gateway never touches this module => zero
//!   overhead, no behaviour change.
//! - **Fail-open.** [`LangfuseClient::trace`] spawns a detached task and returns
//!   immediately; it never blocks the proxied call. A Langfuse outage, timeout,
//!   or error is logged at `debug` and dropped. Backpressure is shed by simply
//!   not awaiting.

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use dashmap::DashMap;
use serde::Serialize;
use serde_json::{Value, json};
use ts_rs::TS;

/// Langfuse sink configuration (`[langfuse]` block / `CCTUI_LANGFUSE_*` env).
/// Built only when host + public + secret are all present.
#[derive(Debug, Clone)]
pub struct LangfuseConfig {
    /// Base URL of the Langfuse ingestion server (no trailing `/api/...`).
    pub host: String,
    /// Browser-facing base URL for deep links (defaults to `host` when unset).
    pub public_host: String,
    pub public_key: String,
    pub secret_key: String,
    /// Fraction of calls to trace, `0.0..=1.0`. Defaults to `1.0` (trace all).
    pub sample_rate: f64,
}

impl LangfuseConfig {
    /// Parse the Langfuse config from the environment. Returns `None` (feature
    /// dark) unless `CCTUI_LANGFUSE_HOST`, `CCTUI_LANGFUSE_PUBLIC_KEY`, and
    /// `CCTUI_LANGFUSE_SECRET_KEY` are all set and non-empty.
    pub fn from_env() -> Option<Self> {
        let nonempty = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
        let host = nonempty("CCTUI_LANGFUSE_HOST")?;
        let public_host = nonempty("CCTUI_LANGFUSE_PUBLIC_HOST").unwrap_or_else(|| host.clone());
        let public_key = nonempty("CCTUI_LANGFUSE_PUBLIC_KEY")?;
        let secret_key = nonempty("CCTUI_LANGFUSE_SECRET_KEY")?;
        let sample_rate = nonempty("CCTUI_LANGFUSE_SAMPLE_RATE")
            .and_then(|s| s.parse::<f64>().ok())
            .map_or(1.0, |r| r.clamp(0.0, 1.0));
        Some(Self {
            host: host.trim_end_matches('/').to_string(),
            public_host: public_host.trim_end_matches('/').to_string(),
            public_key,
            secret_key,
            sample_rate,
        })
    }

    /// The `Basic <base64(public:secret)>` header value for the ingestion API.
    fn basic_auth(&self) -> String {
        let raw = format!("{}:{}", self.public_key, self.secret_key);
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw))
    }
}

/// Identifying context for a single gateway call, used as trace metadata/tags.
#[derive(Debug, Clone, Default)]
pub struct TraceContext {
    pub session_id: Option<String>,
    pub account_id: Option<String>,
    pub model: Option<String>,
}

/// A fully reconstructed gateway call ready to be turned into a Langfuse trace.
pub struct TracePayload {
    pub ctx: TraceContext,
    /// The parsed request body (Anthropic Messages request: `messages`, `system`,
    /// `model`, ...). Used as the generation `input`.
    pub request: Option<Value>,
    /// The reconstructed completion text (assistant output).
    pub output: Option<String>,
    /// Token usage keyed by Langfuse price usage types: `input`, `output`,
    /// `cache_read_input_tokens`, `cache_creation_input_tokens`.
    pub usage: Option<Value>,
    /// `WARNING` + a message when this turn lost a prompt cache it should have
    /// hit, so busts read red in the trace view.
    pub level: Option<&'static str>,
    pub status_message: Option<String>,
}

/// The Langfuse sink. Cheap to clone (shares the `reqwest::Client`); present in
/// `AppState` only when configured.
#[derive(Clone)]
pub struct LangfuseClient {
    config: LangfuseConfig,
    http: reqwest::Client,
    /// `OnceCell<Option>`: outer = "resolved yet?", inner = the project id (or
    /// `None` when the `GET /projects` lookup could not identify one).
    project_id: Arc<tokio::sync::OnceCell<Option<String>>>,
    usage_cache: Arc<DashMap<String, CachedSessionUsage>>,
}

struct CachedSessionUsage {
    fetched_at: Instant,
    usage: LangfuseSessionUsage,
}

const USAGE_TTL: Duration = Duration::from_mins(1);

/// Cost + `trace_count` are exact off the traces list; token classes are
/// best-effort — only populated when the deployment carries per-trace
/// `usageDetails` (legacy self-hosted trace lists often don't).
#[derive(Debug, Clone, Default, Serialize, TS)]
#[ts(export)]
pub struct LangfuseSessionUsage {
    pub cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub trace_count: u64,
}

impl LangfuseClient {
    pub fn new(config: LangfuseConfig, http: reqwest::Client) -> Self {
        Self {
            config,
            http,
            project_id: Arc::new(tokio::sync::OnceCell::new()),
            usage_cache: Arc::new(DashMap::new()),
        }
    }

    /// Base host for building `<host>/project/<id>/sessions/<uuid>` deep links.
    pub fn host(&self) -> &str {
        &self.config.host
    }

    /// Browser-facing base host for deep links (defaults to [`host`] when the
    /// `CCTUI_LANGFUSE_PUBLIC_HOST` env is unset).
    pub fn public_host(&self) -> &str {
        &self.config.public_host
    }

    /// Resolve the Langfuse project id once (via `GET /api/public/projects`) and
    /// memoize it for deep links. Fail-open: an error yields `None`.
    pub async fn project_id(&self) -> Option<String> {
        self.project_id
            .get_or_init(|| async { self.fetch_project_id().await.ok().flatten() })
            .await
            .clone()
    }

    async fn fetch_project_id(&self) -> Result<Option<String>, String> {
        let resp = self
            .http
            .get(format!("{}/api/public/projects", self.config.host))
            .header(reqwest::header::AUTHORIZATION, self.config.basic_auth())
            .send()
            .await
            .map_err(|e| format!("transport: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("status {}", resp.status()));
        }
        let body: Value = resp.json().await.map_err(|e| format!("decode: {e}"))?;
        Ok(body
            .get("data")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|p| p.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    /// Aggregate one cctui session's Langfuse traces into a cost/usage rollup,
    /// serving a fresh-enough cached value without touching upstream.
    pub async fn session_usage(&self, session_id: &str) -> Result<LangfuseSessionUsage, String> {
        if let Some(hit) = self.usage_cache.get(session_id)
            && hit.fetched_at.elapsed() < USAGE_TTL
        {
            return Ok(hit.usage.clone());
        }
        let resp = self
            .http
            .get(format!("{}/api/public/traces", self.config.host))
            .query(&[("sessionId", session_id)])
            .header(reqwest::header::AUTHORIZATION, self.config.basic_auth())
            .send()
            .await
            .map_err(|e| format!("transport: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("status {}", resp.status()));
        }
        let body: Value = resp.json().await.map_err(|e| format!("decode: {e}"))?;
        let data = body.get("data").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
        let usage = aggregate_traces(data);
        self.usage_cache.insert(
            session_id.to_string(),
            CachedSessionUsage { fetched_at: Instant::now(), usage: usage.clone() },
        );
        Ok(usage)
    }

    /// Whether this call should be sampled (cheap, sync — checked before the
    /// trace task is spawned and before any payload is reconstructed).
    pub fn should_sample(&self) -> bool {
        let r = self.config.sample_rate;
        r >= 1.0 || (r > 0.0 && rand_unit() < r)
    }

    /// Fire-and-forget: spawn a detached task that POSTs the trace + generation
    /// to Langfuse. Returns immediately; never blocks or errors into the caller.
    pub fn trace(&self, payload: TracePayload) {
        let config = self.config.clone();
        let http = self.http.clone();
        tokio::spawn(async move {
            if let Err(e) = post_ingestion(&http, &config, payload).await {
                tracing::debug!("langfuse trace dropped: {e}");
            }
        });
    }
}

/// Build the ingestion batch (one `trace-create` + one `generation-create`) and
/// POST it. Errors are returned to the caller, which logs+drops them.
async fn post_ingestion(
    http: &reqwest::Client,
    config: &LangfuseConfig,
    payload: TracePayload,
) -> Result<(), String> {
    let trace_id = uuid::Uuid::new_v4().to_string();
    let gen_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let ctx = &payload.ctx;

    let mut metadata = serde_json::Map::new();
    if let Some(sid) = &ctx.session_id {
        metadata.insert("session_id".into(), json!(sid));
        // The 8-hex worker shortcode is derived from the session id's leading hex.
        metadata.insert("short".into(), json!(short_of(sid)));
    }
    if let Some(aid) = &ctx.account_id {
        metadata.insert("account_id".into(), json!(aid));
    }

    let tags: Vec<String> = std::iter::once("cctui".to_string())
        .chain(ctx.session_id.clone())
        .chain(ctx.account_id.clone())
        .collect();

    let trace_name = ctx.session_id.clone().unwrap_or_else(|| "gateway".into());

    // First-class Langfuse Sessions/Users grouping. These top-level fields drive
    // the Sessions and Users tabs; the same ids in `metadata`/`tags` above only
    // support filtering, not grouping. The session id is the cctui session UUID;
    // the user dimension is the OAuth account id. Both are well under Langfuse's
    // 200-char limit. Omitted (serde drops nulls) when unknown.
    let mut trace_body = serde_json::Map::new();
    trace_body.insert("id".into(), json!(trace_id));
    trace_body.insert("name".into(), json!(trace_name));
    trace_body.insert("timestamp".into(), json!(now));
    if let Some(sid) = &ctx.session_id {
        trace_body.insert("sessionId".into(), json!(sid));
    }
    if let Some(aid) = &ctx.account_id {
        trace_body.insert("userId".into(), json!(aid));
    }
    trace_body.insert("input".into(), json!(payload.request));
    trace_body.insert("output".into(), json!(payload.output));
    trace_body.insert("metadata".into(), Value::Object(metadata.clone()));
    trace_body.insert("tags".into(), json!(tags));

    let trace_event = json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "type": "trace-create",
        "timestamp": now,
        "body": Value::Object(trace_body),
    });

    let gen_body = json!({
        "id": gen_id,
        "traceId": trace_id,
        "type": "GENERATION",
        "name": "anthropic.messages",
        "startTime": now,
        "endTime": now,
        "model": ctx.model,
        "input": payload.request,
        "output": payload.output,
        "usageDetails": payload.usage,
        "metadata": Value::Object(metadata),
        "level": payload.level,
        "statusMessage": payload.status_message,
    });
    let gen_event = json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "type": "generation-create",
        "timestamp": now,
        "body": gen_body,
    });

    let batch = json!({ "batch": [trace_event, gen_event] });

    let resp = http
        .post(format!("{}/api/public/ingestion", config.host))
        .header(reqwest::header::AUTHORIZATION, config.basic_auth())
        .json(&batch)
        .send()
        .await
        .map_err(|e| format!("transport: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("status {}", resp.status()));
    }
    Ok(())
}

/// The 8-hex worker shortcode is the leading hex of the session id (CCT — the
/// daemon derives `~/.claude/jobs/<short>` the same way). Best-effort.
fn short_of(session_id: &str) -> String {
    session_id.chars().filter(char::is_ascii_hexdigit).take(8).collect()
}

/// A uniform random in `[0, 1)` without pulling in the `rand` crate — derived
/// from the low bits of a v4 UUID (good enough for sampling).
fn rand_unit() -> f64 {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let n = u64::from_le_bytes(bytes[0..8].try_into().unwrap_or([0; 8]));
    (n >> 11) as f64 / (1u64 << 53) as f64
}

/// Reconstruct the completion text + token usage from an Anthropic Messages
/// response body, handling both the buffered single-JSON shape and the SSE
/// stream (`event: ...\ndata: {...}` lines). Returns `(output_text, usage)`.
///
/// SSE reconstruction concatenates `content_block_delta` text deltas and pulls
/// usage from `message_start` (input tokens) + `message_delta` (output tokens).
pub fn reconstruct_anthropic(body: &[u8]) -> (Option<String>, Option<Value>) {
    let text = String::from_utf8_lossy(body);
    // Non-SSE: a single JSON object (e.g. non-streaming `/v1/messages`).
    if let Ok(v) = serde_json::from_str::<Value>(text.trim())
        && (v.get("content").is_some() || v.get("usage").is_some())
    {
        return (anthropic_output_text(&v), v.get("usage").map(normalize_usage));
    }
    reconstruct_sse(&text)
}

/// Extract concatenated text from a non-streaming Messages `content` array.
fn anthropic_output_text(v: &Value) -> Option<String> {
    let blocks = v.get("content")?.as_array()?;
    let s: String = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    (!s.is_empty()).then_some(s)
}

/// Reconstruct from an SSE stream: concatenate text deltas, merge usage from
/// `message_start` + `message_delta`.
fn reconstruct_sse(text: &str) -> (Option<String>, Option<Value>) {
    let mut out = String::new();
    let mut input_tokens: Option<u64> = None;
    let mut output_tokens: Option<u64> = None;
    let mut cache_read: Option<u64> = None;
    let mut cache_creation: Option<u64> = None;
    let mut saw_event = false;

    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else { continue };
        let Ok(ev) = serde_json::from_str::<Value>(data.trim()) else { continue };
        saw_event = true;
        match ev.get("type").and_then(Value::as_str) {
            Some("content_block_delta") => {
                if let Some(t) = ev.pointer("/delta/text").and_then(Value::as_str) {
                    out.push_str(t);
                }
            }
            Some("message_start") => {
                if let Some(u) = ev.pointer("/message/usage") {
                    input_tokens = u.get("input_tokens").and_then(Value::as_u64).or(input_tokens);
                    cache_read =
                        u.get("cache_read_input_tokens").and_then(Value::as_u64).or(cache_read);
                    cache_creation = u
                        .get("cache_creation_input_tokens")
                        .and_then(Value::as_u64)
                        .or(cache_creation);
                }
            }
            Some("message_delta") => {
                if let Some(u) = ev.get("usage") {
                    output_tokens =
                        u.get("output_tokens").and_then(Value::as_u64).or(output_tokens);
                }
            }
            _ => {}
        }
    }

    if !saw_event {
        return (None, None);
    }
    let usage = (input_tokens.is_some() || output_tokens.is_some())
        .then(|| usage_details(input_tokens, output_tokens, cache_read, cache_creation));
    ((!out.is_empty()).then_some(out), usage)
}

/// Reconstruct the completion text + token usage from an `OpenAI` Responses API
/// response body (what codex speaks), handling both the buffered single-JSON
/// shape and the SSE stream (`data: {...}` lines). Returns `(output_text,
/// usage)`.
///
/// SSE reconstruction concatenates `response.output_text.delta` deltas and pulls
/// usage from the terminal `response.completed`/`response.incomplete` event's
/// `response.usage` object. Usage maps into the same Langfuse keys the Anthropic
/// path uses: `input` = `input_tokens - cached_tokens`, `cache_read_input_tokens`
/// = `cached_tokens`, `output` = `output_tokens`.
pub fn reconstruct_openai(body: &[u8]) -> (Option<String>, Option<Value>) {
    let text = String::from_utf8_lossy(body);
    // Non-SSE: a single Responses object with a top-level `usage` and `output`.
    if let Ok(v) = serde_json::from_str::<Value>(text.trim())
        && (v.get("output").is_some() || v.get("usage").is_some())
    {
        return (openai_output_text(&v), v.get("usage").map(openai_usage));
    }
    reconstruct_openai_sse(&text)
}

/// Extract concatenated text from a non-streaming Responses `output` array
/// (`output[].content[]` items of type `output_text`).
fn openai_output_text(v: &Value) -> Option<String> {
    let items = v.get("output")?.as_array()?;
    let s: String = items
        .iter()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .collect();
    (!s.is_empty()).then_some(s)
}

/// Reconstruct from an `OpenAI` Responses SSE stream: concatenate
/// `response.output_text.delta` deltas, pull usage from the terminal
/// `response.completed`/`response.incomplete` event.
fn reconstruct_openai_sse(text: &str) -> (Option<String>, Option<Value>) {
    let mut out = String::new();
    let mut usage: Option<Value> = None;
    let mut saw_event = false;

    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else { continue };
        let trimmed = data.trim();
        if trimmed == "[DONE]" {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(trimmed) else { continue };
        saw_event = true;
        match ev.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                if let Some(t) = ev.get("delta").and_then(Value::as_str) {
                    out.push_str(t);
                }
            }
            Some("response.completed" | "response.incomplete") => {
                if let Some(u) = ev.pointer("/response/usage") {
                    usage = Some(openai_usage(u));
                }
            }
            _ => {}
        }
    }

    if !saw_event {
        return (None, None);
    }
    ((!out.is_empty()).then_some(out), usage)
}

/// Map an `OpenAI` Responses `usage` object into the shared Langfuse
/// `usageDetails` shape. `input` is billed non-cached input, so it subtracts the
/// cached (saturating) tokens; the cached count is reported as
/// `cache_read_input_tokens`. `OpenAI` has no cache-creation class.
fn openai_usage(u: &Value) -> Value {
    let input_tokens = u.get("input_tokens").and_then(Value::as_u64);
    let cached = u.pointer("/input_tokens_details/cached_tokens").and_then(Value::as_u64);
    let output_tokens = u.get("output_tokens").and_then(Value::as_u64);
    let input = input_tokens.map(|i| i.saturating_sub(cached.unwrap_or(0)));
    usage_details(input, output_tokens, cached, None)
}

/// Build a Langfuse `usageDetails` map. Keys must match the model's price usage
/// types so cost is computed per token class (cache reads at ~10% of input);
/// `None` entries are omitted — a JSON null fails ingestion validation.
fn usage_details(
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in [
        ("input", input),
        ("output", output),
        ("cache_read_input_tokens", cache_read),
        ("cache_creation_input_tokens", cache_creation),
    ] {
        if let Some(v) = v {
            m.insert(k.into(), json!(v));
        }
    }
    Value::Object(m)
}

/// Normalize a non-streaming Anthropic `usage` object into the same shape as the
/// SSE reconstruction emits.
fn normalize_usage(u: &Value) -> Value {
    usage_details(
        u.get("input_tokens").and_then(Value::as_u64),
        u.get("output_tokens").and_then(Value::as_u64),
        u.get("cache_read_input_tokens").and_then(Value::as_u64),
        u.get("cache_creation_input_tokens").and_then(Value::as_u64),
    )
}

/// Fold a Langfuse `GET /traces?sessionId=` `data[]` array into a session
/// rollup. `totalCost` and the row count are authoritative; token classes are
/// read from an optional per-trace `usageDetails` object when present.
fn aggregate_traces(data: &[Value]) -> LangfuseSessionUsage {
    let mut out = LangfuseSessionUsage::default();
    for trace in data {
        out.trace_count += 1;
        out.cost_usd += trace.get("totalCost").and_then(Value::as_f64).unwrap_or(0.0);
        if let Some(u) = trace.get("usageDetails").and_then(Value::as_object) {
            let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
            out.input_tokens += n("input");
            out.output_tokens += n("output");
            out.cache_read += n("cache_read_input_tokens");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_dark_without_all_keys() {
        // No env set in test => disabled. (We don't mutate process env here to
        // avoid cross-test races; the absence path is the important guarantee.)
        // Just assert the basic-auth + sampling helpers behave.
        let cfg = LangfuseConfig {
            host: "https://lf.example/".into(),
            public_host: "https://lf.example/".into(),
            public_key: "pk".into(),
            secret_key: "sk".into(),
            sample_rate: 1.0,
        };
        assert!(cfg.basic_auth().starts_with("Basic "));
    }

    #[test]
    fn reconstruct_non_streaming() {
        let body = serde_json::to_vec(&json!({
            "content": [{"type": "text", "text": "hello "}, {"type": "text", "text": "world"}],
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 3}
        }))
        .unwrap();
        let (out, usage) = reconstruct_anthropic(&body);
        assert_eq!(out.as_deref(), Some("hello world"));
        let u = usage.unwrap();
        assert_eq!(u["input"], json!(10));
        assert_eq!(u["output"], json!(5));
        assert_eq!(u["cache_read_input_tokens"], json!(3));
        assert!(u.get("cache_creation_input_tokens").is_none());
    }

    #[test]
    fn reconstruct_sse_stream() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":20,\"cache_read_input_tokens\":4}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n",
        );
        let (out, usage) = reconstruct_anthropic(sse.as_bytes());
        assert_eq!(out.as_deref(), Some("Hello"));
        let u = usage.unwrap();
        assert_eq!(u["input"], json!(20));
        assert_eq!(u["output"], json!(7));
        assert_eq!(u["cache_read_input_tokens"], json!(4));
    }

    #[test]
    fn reconstruct_openai_sse_stream() {
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":100,\"input_tokens_details\":{\"cached_tokens\":40},\"output_tokens\":12,\"output_tokens_details\":{\"reasoning_tokens\":8},\"total_tokens\":112}}}\n\n",
            "data: [DONE]\n\n",
        );
        let (out, usage) = reconstruct_openai(sse.as_bytes());
        assert_eq!(out.as_deref(), Some("Hello"));
        let u = usage.unwrap();
        assert_eq!(u["input"], json!(60));
        assert_eq!(u["output"], json!(12));
        assert_eq!(u["cache_read_input_tokens"], json!(40));
        assert!(u.get("cache_creation_input_tokens").is_none());
    }

    #[test]
    fn reconstruct_openai_non_streaming() {
        let body = serde_json::to_vec(&json!({
            "output": [{
                "type": "message",
                "content": [
                    {"type": "output_text", "text": "hello "},
                    {"type": "output_text", "text": "world"},
                ],
            }],
            "usage": {
                "input_tokens": 30,
                "input_tokens_details": {"cached_tokens": 10},
                "output_tokens": 5,
                "output_tokens_details": {"reasoning_tokens": 2},
                "total_tokens": 35,
            }
        }))
        .unwrap();
        let (out, usage) = reconstruct_openai(&body);
        assert_eq!(out.as_deref(), Some("hello world"));
        let u = usage.unwrap();
        assert_eq!(u["input"], json!(20));
        assert_eq!(u["output"], json!(5));
        assert_eq!(u["cache_read_input_tokens"], json!(10));
        assert!(u.get("cache_creation_input_tokens").is_none());
    }

    #[test]
    fn reconstruct_openai_garbage_is_empty() {
        let (out, usage) = reconstruct_openai(b"not json, not sse");
        assert!(out.is_none());
        assert!(usage.is_none());
    }

    #[test]
    fn usage_details_omits_absent_token_classes() {
        let u = usage_details(Some(2), None, Some(100), None);
        assert_eq!(u, json!({"input": 2, "cache_read_input_tokens": 100}));
    }

    #[test]
    fn reconstruct_garbage_is_empty() {
        let (out, usage) = reconstruct_anthropic(b"not json, not sse");
        assert!(out.is_none());
        assert!(usage.is_none());
    }

    #[test]
    fn short_of_takes_leading_hex() {
        assert_eq!(short_of("0123abcd-ef00-0000-0000-000000000000"), "0123abcd");
    }

    #[test]
    fn aggregate_traces_sums_cost_tokens_and_count() {
        let data = [
            json!({
                "id": "t1",
                "totalCost": 0.012,
                "usageDetails": {"input": 10, "output": 5, "cache_read_input_tokens": 3},
            }),
            json!({
                "id": "t2",
                "totalCost": 0.008,
                "usageDetails": {"input": 20, "output": 7},
            }),
            json!({"id": "t3"}),
        ];
        let agg = aggregate_traces(&data);
        assert!((agg.cost_usd - 0.020).abs() < 1e-9);
        assert_eq!(agg.input_tokens, 30);
        assert_eq!(agg.output_tokens, 12);
        assert_eq!(agg.cache_read, 3);
        assert_eq!(agg.trace_count, 3);
    }

    #[test]
    fn aggregate_traces_empty_is_zero() {
        let agg = aggregate_traces(&[]);
        assert_eq!(agg.trace_count, 0);
        assert!(agg.cost_usd.abs() < 1e-9);
    }

    #[test]
    fn sample_rate_full_always_samples() {
        let c = LangfuseClient::new(
            LangfuseConfig {
                host: "h".into(),
                public_host: "h".into(),
                public_key: "p".into(),
                secret_key: "s".into(),
                sample_rate: 1.0,
            },
            reqwest::Client::new(),
        );
        assert!(c.should_sample());
        let z = LangfuseClient::new(
            LangfuseConfig {
                host: "h".into(),
                public_host: "h".into(),
                public_key: "p".into(),
                secret_key: "s".into(),
                sample_rate: 0.0,
            },
            reqwest::Client::new(),
        );
        assert!(!z.should_sample());
    }

    // Soft-limit evaluation must stay OUT of the Langfuse tracing path. The
    // gateway evaluates the soft limit BEFORE it ever constructs a tracer, and a
    // blocked request returns its 429 before any tracing/upstream work. Evaluating the
    // limit therefore must neither read nor mutate the tracer's usage cache —
    // assert that a full evaluate (both Block and Allow) leaves it untouched.
    #[test]
    fn soft_limit_eval_never_touches_langfuse_usage_cache() {
        use crate::soft_limit::{
            Decision, SoftLimits, evaluate_soft_limit, normalize_usage_windows,
        };
        let client = LangfuseClient::new(
            LangfuseConfig {
                host: "h".into(),
                public_host: "h".into(),
                public_key: "p".into(),
                secret_key: "s".into(),
                sample_rate: 1.0,
            },
            reqwest::Client::new(),
        );
        assert!(client.usage_cache.is_empty());

        let now = chrono::DateTime::parse_from_rfc3339("2026-06-19T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let usage = serde_json::json!({"limits":[
            {"kind":"session","percent":95,"resets_at":"2026-06-19T13:00:00Z"},
            {"kind":"weekly_all","percent":10,"resets_at":"2026-06-26T00:00:00Z"},
        ]});
        let windows = normalize_usage_windows(&usage);
        let caps = SoftLimits::from_json(Some(&serde_json::json!({
            "session": {"cap_pct": 80},
        })));
        // Blocked case: still no tracing-side effects.
        assert!(matches!(evaluate_soft_limit(&windows, &caps, None, now), Decision::Block { .. }));
        assert!(client.usage_cache.is_empty(), "blocked eval must not warm the trace cache");

        // Allowed case (under cap): likewise untouched.
        let allow_caps = SoftLimits::from_json(Some(&serde_json::json!({
            "session": {"cap_pct": 99},
        })));
        assert_eq!(evaluate_soft_limit(&windows, &allow_caps, None, now), Decision::Allow);
        assert!(client.usage_cache.is_empty(), "allowed eval must not warm the trace cache");
    }
}
