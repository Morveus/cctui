//! Integration tests — require a running cctui-server and `DATABASE_URL`.
//!
//! Run: `TEST_CCTUI_URL=http://localhost:8700 cargo test -p cctui-server --test integration -- --ignored`

use reqwest::Client;
use serde_json::json;

fn server_url() -> String {
    std::env::var("TEST_CCTUI_URL").unwrap_or_else(|_| "http://localhost:8700".into())
}

fn admin_token() -> String {
    std::env::var("TEST_ADMIN_TOKEN").unwrap_or_else(|_| "test-admin".into())
}

#[tokio::test]
#[ignore = "requires running server"]
async fn health_check() {
    let client = Client::new();
    let resp = client.get(format!("{}/health", server_url())).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
#[ignore = "requires running server"]
async fn register_and_list_session() {
    let client = Client::new();
    let base = server_url();

    let u: serde_json::Value = client
        .post(format!("{base}/api/v1/admin/users"))
        .bearer_auth(admin_token())
        .json(&json!({"name": format!("reg-{}", uuid_like())}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let user_key = u["key"].as_str().unwrap().to_string();

    let m: serde_json::Value = client
        .post(format!("{base}/api/v1/enroll"))
        .bearer_auth(&user_key)
        .json(&json!({"hostname": "reg-host"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let machine_key = m["machine_key"].as_str().unwrap().to_string();

    // Register
    let resp = client
        .post(format!("{base}/api/v1/sessions/register"))
        .bearer_auth(&machine_key)
        .json(&json!({
            "machine_id": "reg-host",
            "working_dir": "/tmp/test",
            "metadata": {"project_name": "test-project"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let session_id = body["session_id"].as_str().unwrap();

    // List
    let resp = client
        .get(format!("{base}/api/v1/sessions"))
        .bearer_auth(admin_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let sessions = body["sessions"].as_array().unwrap();
    assert!(sessions.iter().any(|s| s["id"].as_str() == Some(session_id)));

    // Deregister
    let resp = client
        .post(format!("{base}/api/v1/sessions/{session_id}/deregister"))
        .bearer_auth(&machine_key)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
}

/// claudo/inbox#210: a Task subagent has no worker, so a message can never
/// reach it. The route used to answer 202 anyway and the janitor counted
/// phantom revives; it must refuse instead.
#[tokio::test]
#[ignore = "requires running server"]
async fn a_message_to_a_subagent_is_refused_not_accepted() {
    let client = Client::new();
    let base = server_url();

    let u: serde_json::Value = client
        .post(format!("{base}/api/v1/admin/users"))
        .bearer_auth(admin_token())
        .json(&json!({"name": format!("sub-{}", uuid_like())}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let user_key = u["key"].as_str().unwrap().to_string();

    let m: serde_json::Value = client
        .post(format!("{base}/api/v1/enroll"))
        .bearer_auth(&user_key)
        .json(&json!({"hostname": format!("sub-host-{}", uuid_like())}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let machine_key = m["machine_key"].as_str().unwrap().to_string();

    let body: serde_json::Value = client
        .post(format!("{base}/api/v1/sessions/register"))
        .bearer_auth(&machine_key)
        .json(&json!({
            "machine_id": "sub-host",
            "working_dir": "/tmp/test",
            "metadata": {"subagent": true, "agent_type": "general-purpose"}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session_id = body["session_id"].as_str().unwrap().to_string();

    let resp = client
        .post(format!("{base}/api/v1/sessions/{session_id}/message"))
        .bearer_auth(admin_token())
        .json(&json!({"content": "continue"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409, "a subagent must refuse a message, never accept it");
    let err: serde_json::Value = resp.json().await.unwrap();
    assert!(
        err["error"].as_str().unwrap_or_default().contains("subagent"),
        "the refusal must say why: {err}"
    );

    let _ = client
        .post(format!("{base}/api/v1/sessions/{session_id}/deregister"))
        .bearer_auth(&machine_key)
        .send()
        .await;
}

#[tokio::test]
#[ignore = "requires running server"]
async fn auth_rejects_bad_token() {
    let client = Client::new();
    let resp = client
        .get(format!("{}/api/v1/sessions", server_url()))
        .bearer_auth("wrong-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
#[ignore = "requires running server"]
async fn user_enroll_revoke_flow() {
    let client = Client::new();
    let base = server_url();

    // 1. Admin creates a user — receives key once.
    let resp = client
        .post(format!("{base}/api/v1/admin/users"))
        .bearer_auth(admin_token())
        .json(&json!({"name": format!("itest-{}", uuid_like())}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let user_id = body["id"].as_str().unwrap().to_string();
    let user_key = body["key"].as_str().unwrap().to_string();
    assert!(user_key.starts_with("cctui_u_"));

    // 2. User enrols a machine with their key.
    let resp = client
        .post(format!("{base}/api/v1/enroll"))
        .bearer_auth(&user_key)
        .json(&json!({"hostname": "itest-host"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let machine_key = body["machine_key"].as_str().unwrap().to_string();
    assert!(machine_key.starts_with("cctui_m_"));

    // 3. Machine key can register a session.
    let resp = client
        .post(format!("{base}/api/v1/sessions/register"))
        .bearer_auth(&machine_key)
        .json(&json!({
            "machine_id": "itest-host",
            "working_dir": "/tmp/itest",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // 4. Admin revokes the user; both keys stop working (after TTL or cache purge).
    let resp = client
        .delete(format!("{base}/api/v1/admin/users/{user_id}"))
        .bearer_auth(admin_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    let resp = client
        .post(format!("{base}/api/v1/enroll"))
        .bearer_auth(&user_key)
        .json(&json!({"hostname": "itest-host-2"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let resp = client
        .post(format!("{base}/api/v1/sessions/register"))
        .bearer_auth(&machine_key)
        .json(&json!({"machine_id": "itest-host", "working_dir": "/tmp/itest"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
#[ignore = "requires running server"]
async fn machine_rotate_invalidates_old_key() {
    let client = Client::new();
    let base = server_url();

    let u: serde_json::Value = client
        .post(format!("{base}/api/v1/admin/users"))
        .bearer_auth(admin_token())
        .json(&json!({"name": format!("rot-{}", uuid_like())}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let user_key = u["key"].as_str().unwrap().to_string();

    let m: serde_json::Value = client
        .post(format!("{base}/api/v1/enroll"))
        .bearer_auth(&user_key)
        .json(&json!({"hostname": "rot-host"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let machine_id = m["machine_id"].as_str().unwrap().to_string();
    let old_key = m["machine_key"].as_str().unwrap().to_string();

    let r: serde_json::Value = client
        .post(format!("{base}/api/v1/admin/machines/{machine_id}/rotate"))
        .bearer_auth(admin_token())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let new_key = r["key"].as_str().unwrap().to_string();
    assert_ne!(old_key, new_key);

    // Old machine key rejected.
    let resp = client
        .post(format!("{base}/api/v1/sessions/register"))
        .bearer_auth(&old_key)
        .json(&json!({"machine_id": "rot-host", "working_dir": "/tmp/rot"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // New machine key works.
    let resp = client
        .post(format!("{base}/api/v1/sessions/register"))
        .bearer_auth(&new_key)
        .json(&json!({"machine_id": "rot-host", "working_dir": "/tmp/rot"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos().to_string()
}

/// The full redirect lifecycle: validation ordering, upsert idempotence,
/// model-flip rules needing no target provider, and delete.
#[tokio::test]
#[ignore = "requires running server"]
#[allow(clippy::too_many_lines)]
async fn account_redirect_flow() {
    let client = Client::new();
    let base = server_url();

    let u: serde_json::Value = client
        .post(format!("{base}/api/v1/admin/users"))
        .bearer_auth(admin_token())
        .json(&json!({"name": format!("redir-{}", uuid_like())}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let user_key = u["key"].as_str().unwrap().to_string();

    let mk_account = |name: String| {
        let client = client.clone();
        let base = base.clone();
        let user_key = user_key.clone();
        async move {
            let a: serde_json::Value = client
                .post(format!("{base}/api/v1/accounts"))
                .bearer_auth(&user_key)
                .json(&json!({"name": name}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            a["id"].as_str().unwrap().to_string()
        }
    };
    let hirobot = mk_account(format!("hirobot-{}", uuid_like())).await;
    let pafin = mk_account(format!("pafin-{}", uuid_like())).await;

    // The target has no anthropic provider yet: the rule must be refused.
    let resp = client
        .put(format!("{base}/api/v1/accounts/{hirobot}/redirect"))
        .bearer_auth(&user_key)
        .json(&json!({"to_account": pafin, "family": "anthropic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    let resp = client
        .post(format!("{base}/api/v1/accounts/{pafin}/providers"))
        .bearer_auth(&user_key)
        .json(&json!({
            "provider": "anthropic-compatible",
            "base_url": "http://localhost:9",
            "access_token": "test-cred"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "{}", resp.text().await.unwrap());

    let resp = client
        .put(format!("{base}/api/v1/accounts/{hirobot}/redirect"))
        .bearer_auth(&user_key)
        .json(&json!({"to_account": pafin, "family": "anthropic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    for bad in [
        json!({"to_account": hirobot, "family": "anthropic"}),
        json!({"to_account": pafin, "to_model": "opus", "family": "anthropic"}),
        json!({"family": "anthropic"}),
        json!({"to_account": pafin, "family": "carrier-pigeon"}),
        json!({"match_model": "fable", "to_account": pafin, "family": "anthropic"}),
    ] {
        let resp = client
            .put(format!("{base}/api/v1/accounts/{hirobot}/redirect"))
            .bearer_auth(&user_key)
            .json(&bad)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{bad}");
    }

    // A model flip stays on the account: no target provider involved.
    let resp = client
        .put(format!("{base}/api/v1/accounts/{hirobot}/redirect"))
        .bearer_auth(&user_key)
        .json(&json!({"to_model": "opus", "match_model": "fable", "family": "anthropic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    // Re-arming the account rule overwrites (unique per source+family+match).
    let resp = client
        .put(format!("{base}/api/v1/accounts/{hirobot}/redirect"))
        .bearer_auth(&user_key)
        .json(&json!({"to_account": pafin, "family": "anthropic", "reason": "re-armed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let rules: serde_json::Value = client
        .get(format!("{base}/api/v1/redirects"))
        .bearer_auth(&user_key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mine: Vec<&serde_json::Value> =
        rules.as_array().unwrap().iter().filter(|r| r["from_account"] == json!(hirobot)).collect();
    assert_eq!(mine.len(), 2, "one account rule + one model rule: {rules}");

    for r in mine {
        let resp = client
            .delete(format!("{base}/api/v1/redirects/{}", r["id"].as_str().unwrap()))
            .bearer_auth(&user_key)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
    }
    let rules: serde_json::Value = client
        .get(format!("{base}/api/v1/redirects"))
        .bearer_auth(&user_key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        rules.as_array().unwrap().iter().all(|r| r["from_account"] != json!(hirobot)),
        "{rules}"
    );
}

/// Pools through the admin token, which has no user identity of its own: create
/// must name the owner (and be accepted when it does), and the listing must be
/// the cross-user god-view rather than "the pools of nobody".
#[tokio::test]
#[ignore = "requires running server"]
async fn admin_creates_and_lists_a_pool_for_a_user() {
    let client = Client::new();
    let base = server_url();

    let owner: serde_json::Value = client
        .post(format!("{base}/api/v1/admin/users"))
        .bearer_auth(admin_token())
        .json(&json!({"name": format!("pool-owner-{}", uuid_like())}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let owner_id = owner["id"].as_str().unwrap().to_string();

    // No owner named: still a 400, so the admin token cannot create ownerless pools.
    let resp = client
        .post(format!("{base}/api/v1/account-pools"))
        .bearer_auth(admin_token())
        .json(&json!({"name": format!("pool-{}", uuid_like())}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    let name = format!("pool-{}", uuid_like());
    let resp = client
        .post(format!("{base}/api/v1/account-pools"))
        .bearer_auth(admin_token())
        .json(&json!({"name": name, "user_id": owner_id, "accounts": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let pool: serde_json::Value = resp.json().await.unwrap();
    let pool_id = pool["id"].as_str().unwrap().to_string();
    assert_eq!(pool["user_id"].as_str(), Some(owner_id.as_str()));

    // The admin listing sees another user's pool.
    let pools: serde_json::Value = client
        .get(format!("{base}/api/v1/account-pools"))
        .bearer_auth(admin_token())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        pools.as_array().unwrap().iter().any(|p| p["id"].as_str() == Some(pool_id.as_str())),
        "{pools}"
    );

    let resp = client
        .delete(format!("{base}/api/v1/account-pools/{pool_id}"))
        .bearer_auth(admin_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
}

/// The pool usage aggregate: a pool with no credentialed members still lists
/// (with no families), and the route is not shadowed by `/account-pools/{id}`.
#[tokio::test]
#[ignore = "requires running server"]
async fn pool_usage_lists_every_pool() {
    let client = Client::new();
    let base = server_url();

    let u: serde_json::Value = client
        .post(format!("{base}/api/v1/admin/users"))
        .bearer_auth(admin_token())
        .json(&json!({"name": format!("pool-usage-{}", uuid_like())}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let user_key = u["key"].as_str().unwrap().to_string();

    let account: serde_json::Value = client
        .post(format!("{base}/api/v1/accounts"))
        .bearer_auth(&user_key)
        .json(&json!({"name": format!("member-{}", uuid_like())}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let account_id = account["id"].as_str().unwrap().to_string();
    assert_eq!(account["pool_weight"].as_f64(), Some(1.0), "{account}");

    let name = format!("pool-{}", uuid_like());
    let pool: serde_json::Value = client
        .post(format!("{base}/api/v1/account-pools"))
        .bearer_auth(&user_key)
        .json(&json!({"name": name, "accounts": [account_id]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pool_id = pool["id"].as_str().unwrap().to_string();

    let resp = client
        .get(format!("{base}/api/v1/account-pools/usage"))
        .bearer_auth(&user_key)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let usage: serde_json::Value = resp.json().await.unwrap();
    let mine = usage
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pool_id"].as_str() == Some(pool_id.as_str()))
        .unwrap_or_else(|| panic!("pool missing from usage: {usage}"));
    assert_eq!(mine["name"].as_str(), Some(name.as_str()));
    assert_eq!(mine["failover"].as_bool(), Some(false));
    // An account with no provider credential contributes to no family.
    assert_eq!(mine["families"].as_array().map(Vec::len), Some(0), "{mine}");

    let resp = client
        .delete(format!("{base}/api/v1/account-pools/{pool_id}"))
        .bearer_auth(&user_key)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
}

/// `pool_weight` is owner-editable on its own, and refused when it is not a
/// positive number.
#[tokio::test]
#[ignore = "requires running server"]
async fn pool_weight_is_validated_and_persisted() {
    let client = Client::new();
    let base = server_url();

    let u: serde_json::Value = client
        .post(format!("{base}/api/v1/admin/users"))
        .bearer_auth(admin_token())
        .json(&json!({"name": format!("pool-weight-{}", uuid_like())}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let user_key = u["key"].as_str().unwrap().to_string();

    let account: serde_json::Value = client
        .post(format!("{base}/api/v1/accounts"))
        .bearer_auth(&user_key)
        .json(&json!({"name": format!("weighted-{}", uuid_like())}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let account_id = account["id"].as_str().unwrap().to_string();

    // A non-positive number is refused by the handler (400); a non-number never
    // reaches it (axum's JSON rejection, 422).
    for (bad, status) in [(json!(0), 400), (json!(-2.5), 400), (json!("four"), 422)] {
        let resp = client
            .patch(format!("{base}/api/v1/accounts/{account_id}"))
            .bearer_auth(&user_key)
            .json(&json!({"pool_weight": bad}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), status, "pool_weight {bad} should be refused");
    }

    let resp = client
        .patch(format!("{base}/api/v1/accounts/{account_id}"))
        .bearer_auth(&user_key)
        .json(&json!({"pool_weight": 4}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let updated: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(updated["pool_weight"].as_f64(), Some(4.0), "{updated}");

    let fetched: serde_json::Value = client
        .get(format!("{base}/api/v1/accounts/{account_id}"))
        .bearer_auth(&user_key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(fetched["pool_weight"].as_f64(), Some(4.0), "{fetched}");
}
