//! Per-turn prompt-cache loss.
//!
//! `cache_read_input_tokens` is the exact length of the prefix Anthropic
//! matched, so a turn that should have matched its predecessor's whole context
//! and did not paid to re-write it at the cache-write rate. Parent and subagent
//! turns share a session id, so the predecessor is the previous turn of the same
//! *stream*, found by context size rather than by timestamp.

use chrono::{DateTime, Duration, Utc};

/// A turn's cache read must reach this fraction of the previous turn's context
/// to count as a hit. Well below 1.0: a turn legitimately adds its own tokens
/// above the cached prefix, and Anthropic rounds to block boundaries.
const HIT_FRACTION: f64 = 0.5;

/// How far a stream's context may exceed the incoming turn's and still be
/// considered its predecessor. Contexts grow, but compaction and a dropped
/// system-reminder can shrink one slightly.
const LINEAGE_SLACK: f64 = 1.25;

/// Anthropic bills a cache write at 1.25x the base input rate.
const CACHE_WRITE_MULTIPLIER: f64 = 1.25;

/// Beyond this gap the 1h cache TTL has expired and a miss is expected.
pub const fn ttl_window() -> Duration {
    Duration::hours(1)
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub message_id: String,
    pub model: Option<String>,
    pub input: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    pub created_at: DateTime<Utc>,
    /// The gateway re-serialized this request before forwarding it.
    pub gateway_rewrote_body: bool,
}

impl Turn {
    /// Full context this turn carried: what its successor should be able to read
    /// back out of the cache.
    fn context(&self) -> i64 {
        self.input.max(0) + self.cache_read.max(0) + self.cache_creation.max(0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bust {
    pub lost_tokens: u64,
    pub lost_usd: f64,
    pub reason: Reason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    TtlExpired,
    GatewayRewroteBody,
    Unknown,
}

impl Reason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TtlExpired => "ttl_expired",
            Self::GatewayRewroteBody => "gateway_rewrote_body",
            Self::Unknown => "unknown",
        }
    }
}

/// USD wasted re-writing `lost` tokens that should have been read from cache.
pub fn lost_usd(catalog: Option<&serde_json::Value>, model: Option<&str>, lost: u64) -> f64 {
    let Some(price) = model.and_then(|m| crate::cost::price_for_model(catalog, m)) else {
        return 0.0;
    };
    let delta = price.input.mul_add(CACHE_WRITE_MULTIPLIER, -price.cached_input);
    (lost as f64 * delta.max(0.0)) / 1_000_000.0
}

/// Whether `turn` lost a cache it should have hit, given its stream predecessor.
fn judge(prev: &Turn, turn: &Turn) -> Option<(u64, Reason)> {
    let expected = prev.context();
    if expected <= 0 {
        return None;
    }
    let threshold = expected as f64 * HIT_FRACTION;
    if turn.cache_read.max(0) as f64 >= threshold {
        return None;
    }
    let lost = u64::try_from(expected - turn.cache_read.max(0)).unwrap_or(0);
    let reason = if turn.gateway_rewrote_body {
        Reason::GatewayRewroteBody
    } else if turn.created_at - prev.created_at > ttl_window() {
        Reason::TtlExpired
    } else {
        Reason::Unknown
    };
    Some((lost, reason))
}

/// The stream `turn` continues: the stream whose head carries the closest
/// context at or below this turn's own, within [`LINEAGE_SLACK`].
///
/// A bust keeps the context roughly constant (the tokens move from `cache_read`
/// into `cache_creation`), so context size separates an interleaved parent and
/// subagent even when one of them just lost its cache.
fn pick_stream(heads: &[usize], turns: &[Turn], i: usize) -> Option<usize> {
    let ceiling = turns[i].context() as f64 * LINEAGE_SLACK;
    heads
        .iter()
        .copied()
        .enumerate()
        .filter(|&(_, h)| turns[h].context() as f64 <= ceiling)
        .max_by(|&(_, a), &(_, b)| turns[a].context().cmp(&turns[b].context()).then(a.cmp(&b)))
        .map(|(slot, _)| slot)
}

/// Bust verdicts keyed by `message_id`. `turns` need not be sorted.
pub fn compute(
    turns: &[Turn],
    catalog: Option<&serde_json::Value>,
) -> std::collections::HashMap<String, Bust> {
    let mut order: Vec<usize> = (0..turns.len()).collect();
    order.sort_by(|&a, &b| {
        turns[a]
            .created_at
            .cmp(&turns[b].created_at)
            .then_with(|| turns[a].message_id.cmp(&turns[b].message_id))
    });

    let mut heads: Vec<usize> = Vec::new();
    let mut out = std::collections::HashMap::new();
    for i in order {
        let Some(slot) = pick_stream(&heads, turns, i) else {
            heads.push(i);
            continue;
        };
        if let Some((lost, reason)) = judge(&turns[heads[slot]], &turns[i]) {
            out.insert(
                turns[i].message_id.clone(),
                Bust {
                    lost_tokens: lost,
                    lost_usd: lost_usd(catalog, turns[i].model.as_deref(), lost),
                    reason,
                },
            );
        }
        heads[slot] = i;
    }
    out
}

/// How many recent rows to reconstruct the session's streams from when judging
/// a turn that has not been persisted yet.
const LIVE_LOOKBACK: i64 = 40;

/// Judge a just-completed turn against the session's recent history. `turn` is
/// not yet in `session_token_usage`, so it is appended to what is.
pub async fn judge_latest(pool: &sqlx::PgPool, session_id: &str, mut turn: Turn) -> Option<Bust> {
    type Row = (String, Option<String>, i64, i64, i64, bool, DateTime<Utc>);
    turn.message_id = format!("live-{}", uuid::Uuid::new_v4().simple());
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT message_id, model, input_tokens, cache_read_tokens, cache_creation_tokens, \
                gateway_rewrote_body, created_at \
         FROM session_token_usage WHERE session_id = $1 \
         ORDER BY created_at DESC LIMIT $2",
    )
    .bind(session_id)
    .bind(LIVE_LOOKBACK)
    .fetch_all(pool)
    .await
    .ok()?;
    if rows.is_empty() {
        return None;
    }
    let key = turn.message_id.clone();
    let mut turns: Vec<Turn> = rows
        .into_iter()
        .map(|(message_id, model, input, cache_read, cache_creation, rewrote, at)| Turn {
            message_id,
            model,
            input,
            cache_read,
            cache_creation,
            created_at: at,
            gateway_rewrote_body: rewrote,
        })
        .collect();
    turns.push(turn);
    compute(&turns, None).remove(&key)
}

/// `cache bust: read X of expected Y (reason)` for a trace's status message.
pub fn status_message(cache_read: i64, bust: &Bust) -> String {
    let read = cache_read.max(0);
    let expected = u64::try_from(read).unwrap_or(0) + bust.lost_tokens;
    let reason = bust.reason.as_str();
    format!("cache bust: read {read} of expected {expected} ({reason})")
}

/// Langfuse `(level, statusMessage)` for a just-completed gateway call, so a
/// bust reads red in the trace view. `(None, None)` when nothing was lost.
pub async fn trace_annotation(
    pool: &sqlx::PgPool,
    session_id: Option<&str>,
    model: Option<&str>,
    usage: Option<&serde_json::Value>,
    gateway_rewrote_body: bool,
) -> (Option<&'static str>, Option<String>) {
    let (Some(session_id), Some(usage)) = (session_id, usage) else { return (None, None) };
    let field = |key: &str| {
        i64::try_from(usage.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0))
            .unwrap_or(i64::MAX)
    };
    let cache_read = field("cache_read_input_tokens");
    let turn = Turn {
        message_id: String::new(),
        model: model.map(str::to_owned),
        input: field("input"),
        cache_read,
        cache_creation: field("cache_creation_input_tokens"),
        created_at: Utc::now(),
        gateway_rewrote_body,
    };
    judge_latest(pool, session_id, turn)
        .await
        .map_or((None, None), |bust| (Some("WARNING"), Some(status_message(cache_read, &bust))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(mins: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 20, 6, 0, 0).unwrap() + Duration::minutes(mins)
    }

    fn turn(id: &str, input: i64, cr: i64, cc: i64, mins: i64) -> Turn {
        Turn {
            message_id: id.into(),
            model: Some("claude-opus-5".into()),
            input,
            cache_read: cr,
            cache_creation: cc,
            created_at: at(mins),
            gateway_rewrote_body: false,
        }
    }

    fn catalog() -> serde_json::Value {
        serde_json::json!([{
            "model": "claude-opus-5",
            "price_input_per_mtok": 15.0,
            "price_cached_input_per_mtok": 1.5,
            "price_output_per_mtok": 75.0,
        }])
    }

    #[test]
    fn a_clean_stream_busts_nothing() {
        let turns = [turn("a", 266, 200_968, 266, 0), turn("b", 300, 201_234, 1154, 2)];
        assert!(compute(&turns, None).is_empty());
    }

    /// The production signature: `cache_read` pinned at tools+system while the
    /// rest of the context is re-written.
    #[test]
    fn a_collapsed_cache_read_is_a_bust() {
        let turns = [turn("a", 266, 200_968, 266, 0), turn("bust", 100, 28_977, 172_992, 2)];
        let busts = compute(&turns, Some(&catalog()));
        let b = busts.get("bust").expect("bust detected");
        assert_eq!(b.reason, Reason::Unknown);
        assert_eq!(b.lost_tokens, 201_500 - 28_977);
        // 15 * 1.25 - 1.5 = 17.25 per mtok.
        assert!((b.lost_usd - (172_523.0 * 17.25 / 1_000_000.0)).abs() < 1e-9);
        assert!(!busts.contains_key("a"));
    }

    #[test]
    fn a_long_gap_is_ttl_expiry_not_a_gateway_fault() {
        let turns = [turn("a", 266, 200_968, 266, 0), turn("b", 100, 0, 201_000, 75)];
        assert_eq!(compute(&turns, None)["b"].reason, Reason::TtlExpired);
    }

    #[test]
    fn a_recorded_rewrite_names_the_gateway() {
        let mut bust = turn("bust", 100, 28_977, 172_992, 2);
        bust.gateway_rewrote_body = true;
        let turns = [turn("a", 266, 200_968, 266, 0), bust];
        assert_eq!(compute(&turns, None)["bust"].reason, Reason::GatewayRewroteBody);
    }

    /// A rewrite outranks the clock: a >1h gap that ALSO went through a
    /// re-serialized body is the gateway's doing, not the TTL's.
    #[test]
    fn a_rewrite_outranks_a_long_gap() {
        let mut bust = turn("bust", 100, 0, 201_000, 75);
        bust.gateway_rewrote_body = true;
        let turns = [turn("a", 266, 200_968, 266, 0), bust];
        assert_eq!(compute(&turns, None)["bust"].reason, Reason::GatewayRewroteBody);
    }

    /// A subagent's small turns interleave with the parent's large ones under
    /// one session id. Judging each against the row before it by time would
    /// call every switch a bust; each must be judged against its own stream.
    #[test]
    fn interleaved_parent_and_subagent_are_judged_separately() {
        let turns = [
            turn("p1", 266, 150_000, 500, 0),
            turn("s1", 200, 8_000, 300, 1),
            turn("p2", 300, 150_766, 400, 2),
            turn("s2", 150, 8_500, 200, 3),
            turn("p3", 300, 151_466, 400, 4),
        ];
        assert!(compute(&turns, None).is_empty(), "no stream lost its cache");
    }

    #[test]
    fn a_subagent_bust_is_attributed_to_the_subagent_turn() {
        let turns = [
            turn("p1", 266, 150_000, 500, 0),
            turn("s1", 200, 8_000, 300, 1),
            turn("p2", 300, 150_766, 400, 2),
            turn("s2", 150, 100, 8_400, 3),
        ];
        let busts = compute(&turns, None);
        assert_eq!(busts.len(), 1);
        assert!(busts.contains_key("s2"));
    }

    #[test]
    fn a_stream_opening_turn_has_no_predecessor_to_lose() {
        let turns = [turn("first", 20_000, 0, 20_000, 0)];
        assert!(compute(&turns, None).is_empty());
    }

    #[test]
    fn an_unpriced_model_still_reports_lost_tokens() {
        let turns = [turn("a", 266, 200_968, 266, 0), turn("bust", 100, 28_977, 172_992, 2)];
        let b = &compute(&turns, None)["bust"];
        assert!(b.lost_tokens > 0);
        assert!(b.lost_usd.abs() < f64::EPSILON);
    }

    #[test]
    fn unsorted_input_is_ordered_before_judging() {
        let forward = [turn("a", 266, 200_968, 266, 0), turn("bust", 100, 28_977, 172_992, 2)];
        let reversed = [forward[1].clone(), forward[0].clone()];
        assert_eq!(compute(&forward, None), compute(&reversed, None));
    }

    #[test]
    fn reason_labels_match_the_api_contract() {
        assert_eq!(Reason::TtlExpired.as_str(), "ttl_expired");
        assert_eq!(Reason::GatewayRewroteBody.as_str(), "gateway_rewrote_body");
        assert_eq!(Reason::Unknown.as_str(), "unknown");
    }
}
