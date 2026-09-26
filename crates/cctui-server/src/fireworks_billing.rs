//! Reconciliation of cctui's locally metered Fireworks spend against Fireworks'
//! own billing API.
//!
//! Local metering can under-count: it only sees requests the gateway both proxied
//! and could attribute to a session. The billing API is the provider's own record,
//! so a window takes whichever of the two is higher.
//!
//! Two properties of the upstream API shape everything here:
//!
//! 1. Every row comes back `costNanoUsd: 0` — Fireworks does not rate this usage,
//!    which is why its own dashboard also reads $0.00. So reconciliation prices
//!    upstream *token counts* through the account's catalog, exactly as the local
//!    path does, and never trusts an upstream dollar figure.
//! 2. Buckets are daily. A 7d window reconciles directly; a 5h window cannot be
//!    read from daily buckets at all (see [`reconcile_5h`]).
//!
//! The account is frequently shared — a Fireworks account holds many API keys, and
//! reconciling account-wide would import other tenants' inference into cctui's
//! budget. Every query is therefore narrowed to cctui's own API key, and a missing
//! key name disables reconciliation rather than widening it.

use crate::cost::TokenUsage;

/// Fireworks' control-plane base (the billing API), distinct from the inference
/// base the gateway proxies to. Overridable to track upstream changes.
pub fn billing_base() -> String {
    std::env::var("CCTUI_FIREWORKS_BILLING_BASE")
        .unwrap_or_else(|_| "https://api.fireworks.ai/v1".into())
}

/// Per-model token tallies, already narrowed to one API key.
pub type ModelUsage = (String, TokenUsage);

fn count(v: Option<&serde_json::Value>) -> i64 {
    match v {
        Some(serde_json::Value::String(s)) => s.trim().parse().unwrap_or(0),
        Some(n) => n.as_i64().unwrap_or(0),
        None => 0,
    }
}

/// Sum a `billingUsage` response into per-model tallies, keeping only rows billed
/// to `key_name`.
///
/// Int64 fields arrive JSON-encoded as strings, and the response carries the
/// cached/uncached prompt split directly — so no split has to be inferred.
/// Unknown or unmatched rows contribute nothing.
pub fn parse_billing_usage(body: &[u8], key_name: &str) -> Vec<ModelUsage> {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(rows) = json.get("serverlessCosts").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let mut by_model: std::collections::BTreeMap<String, TokenUsage> =
        std::collections::BTreeMap::new();
    for row in rows {
        let group = row.get("group");
        let matches = group
            .and_then(|g| g.get("api_key_name"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|n| n == key_name);
        if !matches {
            continue;
        }
        let Some(model) = row
            .get("modelName")
            .or_else(|| group.and_then(|g| g.get("model_name")))
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let entry = by_model.entry(model.to_owned()).or_default();
        entry.input += count(row.get("uncachedPromptTokens"));
        entry.cached_input += count(row.get("cachedPromptTokens"));
        entry.output += count(row.get("completionTokens"));
    }
    by_model.into_iter().filter(|(_, u)| !u.is_empty()).collect()
}

/// A 7d window reconciles directly against the upstream total.
pub fn reconcile_7d(local: f64, upstream: Option<f64>) -> f64 {
    upstream.filter(|u| u.is_finite() && *u > local).unwrap_or(local)
}

/// A 5h window has no upstream counterpart: buckets are daily, so the last 5
/// hours cannot be isolated. Instead of inventing precision, carry over whatever
/// systematic under-count the 7d comparison exposes and scale the local figure by
/// it.
///
/// When local metering recorded nothing at all over 7d there is no ratio to take,
/// and no way to know what share of the upstream total is recent — the window
/// falls back to the full upstream total. That over-states a 5h window, which is
/// the safe direction for a spend cap.
pub fn reconcile_5h(recent: f64, week: f64, upstream_week: Option<f64>) -> f64 {
    let Some(upstream) = upstream_week.filter(|u| u.is_finite() && *u > week) else {
        return recent;
    };
    if week <= 0.0 {
        return upstream.max(recent);
    }
    (recent * (upstream / week)).max(recent)
}

#[cfg(test)]
mod tests {
    use super::{parse_billing_usage, reconcile_5h, reconcile_7d};

    /// Verbatim shape of a real `billingUsage` response (accounts/beta-inc,
    /// 2026-07-28, grouped by `api_key_id` + `api_key_name` + `model_name`): two keys on
    /// one shared account, only one of which is cctui's.
    const SHARED_ACCOUNT: &[u8] = br#"{
      "dedicatedCosts": [],
      "serverlessCosts": [
        {
          "apiKeyId": "key_BCeQCxy72",
          "cachedPromptTokens": "762880",
          "completionTokens": "8272",
          "costNanoUsd": 0,
          "group": {
            "api_key_id": "key_BCeQCxy72",
            "api_key_name": "grid-alpha",
            "model_name": "accounts/fireworks/models/kimi-k3"
          },
          "modelName": "accounts/fireworks/models/kimi-k3",
          "promptTokens": "886211",
          "uncachedPromptTokens": "123331",
          "usageType": "TEXT_COMPLETION_INFERENCE_USAGE"
        },
        {
          "apiKeyId": "key_xJ5igaFeW",
          "cachedPromptTokens": "45200384",
          "completionTokens": "407395",
          "costNanoUsd": 0,
          "group": {
            "api_key_id": "key_xJ5igaFeW",
            "api_key_name": "ai-hedge-research",
            "model_name": "accounts/fireworks/models/kimi-k3"
          },
          "modelName": "accounts/fireworks/models/kimi-k3",
          "promptTokens": "47823199",
          "uncachedPromptTokens": "2622815",
          "usageType": "TEXT_COMPLETION_INFERENCE_USAGE"
        }
      ],
      "trainingCosts": []
    }"#;

    #[test]
    fn only_our_own_api_key_is_counted_on_a_shared_account() {
        let got = parse_billing_usage(SHARED_ACCOUNT, "grid-alpha");
        assert_eq!(got.len(), 1);
        let (model, usage) = &got[0];
        assert_eq!(model, "accounts/fireworks/models/kimi-k3");
        assert_eq!(usage.input, 123_331, "uncached prompt tokens");
        assert_eq!(usage.cached_input, 762_880);
        assert_eq!(usage.output, 8_272);
    }

    #[test]
    fn an_unknown_key_name_yields_nothing_rather_than_the_whole_account() {
        assert!(parse_billing_usage(SHARED_ACCOUNT, "").is_empty());
        assert!(parse_billing_usage(SHARED_ACCOUNT, "not-ours").is_empty());
    }

    #[test]
    fn daily_buckets_for_one_key_are_summed_per_model() {
        let body = br#"{"serverlessCosts":[
          {"group":{"api_key_name":"k","model_name":"m"},"modelName":"m",
           "uncachedPromptTokens":"10","cachedPromptTokens":"100","completionTokens":"1"},
          {"group":{"api_key_name":"k","model_name":"m"},"modelName":"m",
           "uncachedPromptTokens":"20","cachedPromptTokens":"200","completionTokens":"2"}
        ]}"#;
        let got = parse_billing_usage(body, "k");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1.input, 30);
        assert_eq!(got[0].1.cached_input, 300);
        assert_eq!(got[0].1.output, 3);
    }

    #[test]
    fn malformed_or_empty_bodies_are_inert() {
        assert!(parse_billing_usage(b"not json", "k").is_empty());
        assert!(parse_billing_usage(b"{}", "k").is_empty());
        assert!(parse_billing_usage(br#"{"serverlessCosts":[]}"#, "k").is_empty());
    }

    #[test]
    fn a_window_takes_whichever_figure_is_higher() {
        assert!((reconcile_7d(1.0, Some(4.0)) - 4.0).abs() < f64::EPSILON);
        assert!((reconcile_7d(9.0, Some(4.0)) - 9.0).abs() < f64::EPSILON);
        assert!((reconcile_7d(9.0, None) - 9.0).abs() < f64::EPSILON);
        assert!((reconcile_7d(9.0, Some(f64::NAN)) - 9.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_5h_window_scales_by_the_under_count_the_7d_comparison_reveals() {
        // Local saw half of what the provider billed ⇒ the 5h figure doubles.
        assert!((reconcile_5h(2.0, 10.0, Some(20.0)) - 4.0).abs() < 1e-9);
        // Local already matches or exceeds upstream ⇒ untouched.
        assert!((reconcile_5h(2.0, 10.0, Some(10.0)) - 2.0).abs() < 1e-9);
        assert!((reconcile_5h(2.0, 10.0, None) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_totally_broken_local_meter_falls_back_to_the_upstream_total() {
        assert!((reconcile_5h(0.0, 0.0, Some(7.5)) - 7.5).abs() < 1e-9);
    }
}
