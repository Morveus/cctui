//! Replica-aware WS presence + pod discovery.
//!
//! The daemon/dispatcher connection registries in [`crate::state::AppState`]
//! are per-pod in-memory maps, so with multiple server replicas an HTTP request
//! that needs a live WS can land on a pod that doesn't hold it. Each replica
//! records the WS connections it terminates in the `ws_presence` table; a pod
//! that misses locally consults the table and, when a live peer owns the
//! connection, the [`crate::bus::peer::PeerHttpTransport`] forwards the frame
//! to that peer over the internal bus endpoints.
//!
//! `pods` is the pod-level twin of `ws_presence`: each replica registers its
//! own (name, IP) row and heartbeats it, so event publish can fan out to every
//! live peer replica.
//!
//! Registration only happens when the pod knows its own routable IP
//! (`CCTUI_POD_IP`, injected via the k8s downward API). Without it — local dev,
//! single-replica deployments — nothing is written and behavior is exactly the
//! single-pod model; lookups still work so such a pod can forward
//! *to* registered peers.

use dashmap::DashMap;
use sqlx::PgPool;
use uuid::Uuid;

use crate::state::AppState;

/// What kind of WS a presence row describes. Stored as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Daemon,
    Dispatcher,
    /// One announced session, so session-scoped frames reach the pod holding
    /// the WS that announced it rather than whichever pod owns the (possibly
    /// shared) machine identity.
    Session,
}

impl Kind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Daemon => "daemon",
            Self::Dispatcher => "dispatcher",
            Self::Session => "session",
        }
    }
}

/// A row's heartbeat must be at most this old to be trusted. Heartbeats are
/// written every [`HEARTBEAT_SECS`], so 3× distinguishes a crashed pod from a
/// slow tick (mirrors the WS read-timeout discipline).
const LIVE_WITHIN_SECS: i32 = 45;
/// Cadence of the per-pod heartbeat task.
const HEARTBEAT_SECS: u64 = 15;
/// Rows a crashed peer never deleted are reaped past this age. Far beyond
/// [`LIVE_WITHIN_SECS`] so a reap never races a slow-but-alive heartbeat.
const REAP_AFTER_SECS: i32 = 600;

/// This pod's identity for presence rows. Built once at boot.
pub struct PodIdentity {
    /// Pod (host) name — unique per replica; scopes our own rows.
    pub pod: String,
    /// Routable IP peers can reach this pod's HTTP port on. `None` disables
    /// registration (this pod never OWNS forwardable rows).
    pub ip: Option<String>,
    /// The `ws_presence` rows this pod currently owns. The heartbeat re-upserts
    /// them, so a row reaped while the DB was unreachable comes back on the
    /// next tick instead of staying gone until the WS reconnects.
    owned: DashMap<(Kind, Uuid), ()>,
}

impl PodIdentity {
    /// `CCTUI_POD_IP` (downward API) + `HOSTNAME`. No IP ⇒ registration off.
    pub fn from_env() -> Self {
        let pod = std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into());
        let ip = std::env::var("CCTUI_POD_IP").ok().filter(|s| !s.trim().is_empty());
        match &ip {
            Some(ip) => tracing::info!(pod, ip, "WS presence registration enabled"),
            None => {
                tracing::info!(pod, "CCTUI_POD_IP unset — WS presence registration disabled");
            }
        }
        Self { pod, ip, owned: DashMap::new() }
    }

    #[cfg(test)]
    fn for_test(pod: &str, ip: &str) -> Self {
        Self { pod: pod.into(), ip: Some(ip.into()), owned: DashMap::new() }
    }
}

/// Record this pod as the owner of `entity_id`'s live WS. Upsert: on a
/// cross-pod reconnect the newest connection wins, exactly like the in-memory
/// registries. No-op without a pod IP. Best-effort — a presence write failure
/// must never break the WS itself.
pub async fn register(state: &AppState, kind: Kind, entity_id: Uuid) {
    let Some(ip) = state.presence.ip.as_deref() else { return };
    state.presence.owned.insert((kind, entity_id), ());
    if let Err(err) = sqlx::query(
        "INSERT INTO ws_presence (kind, entity_id, pod, pod_ip, connected_at, heartbeat_at) \
         VALUES ($1, $2, $3, $4, now(), now()) \
         ON CONFLICT (kind, entity_id) DO UPDATE SET \
           pod = EXCLUDED.pod, pod_ip = EXCLUDED.pod_ip, \
           connected_at = now(), heartbeat_at = now()",
    )
    .bind(kind.as_str())
    .bind(entity_id)
    .bind(&state.presence.pod)
    .bind(ip)
    .execute(&state.pool)
    .await
    {
        tracing::warn!(%err, %entity_id, kind = kind.as_str(), "ws_presence register failed");
    }
}

/// Drop this pod's presence row for `entity_id`. Guarded by `pod = self`: if
/// the entity already reconnected to a peer (which upserted the row over to
/// itself), our late disconnect cleanup must not delete the new owner's row —
/// the cross-pod twin of the `remove_if(same_channel)` guard.
pub async fn unregister(state: &AppState, kind: Kind, entity_id: Uuid) {
    if state.presence.ip.is_none() {
        return;
    }
    state.presence.owned.remove(&(kind, entity_id));
    if let Err(err) =
        sqlx::query("DELETE FROM ws_presence WHERE kind = $1 AND entity_id = $2 AND pod = $3")
            .bind(kind.as_str())
            .bind(entity_id)
            .bind(&state.presence.pod)
            .execute(&state.pool)
            .await
    {
        tracing::warn!(%err, %entity_id, kind = kind.as_str(), "ws_presence unregister failed");
    }
}

/// The IP of a live PEER pod owning `entity_id`'s WS, if any. `None` means
/// "no live peer owns it" — either truly offline, or a stale row (crashed pod),
/// or we own it ourselves (callers check the in-memory registry first, so a
/// self-row here still means the connection is gone locally → offline).
/// Pool-level (the bus transport holds a pool + pod name, not the whole
/// `AppState` — the bus is built before it).
pub async fn peer_owner_ip(
    pool: &PgPool,
    self_pod: &str,
    kind: Kind,
    entity_id: Uuid,
) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT pod_ip FROM ws_presence \
         WHERE kind = $1 AND entity_id = $2 AND pod <> $3 \
           AND heartbeat_at > now() - make_interval(secs => $4)",
    )
    .bind(kind.as_str())
    .bind(entity_id)
    .bind(self_pod)
    .bind(f64::from(LIVE_WITHIN_SECS))
    .fetch_optional(pool)
    .await
    .map_err(|err| tracing::warn!(%err, %entity_id, "ws_presence lookup failed"))
    .ok()
    .flatten()
}

/// IPs of every live PEER pod (excluding this one), for event fan-out.
/// Best-effort: a lookup failure logs and returns empty — DB
/// persistence remains the source of truth for refetch, so a missed relay
/// degrades to today's single-pod visibility rather than an error.
pub async fn live_peer_pods(pool: &PgPool, self_pod: &str) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT pod_ip FROM pods \
         WHERE pod <> $1 AND heartbeat_at > now() - make_interval(secs => $2)",
    )
    .bind(self_pod)
    .bind(f64::from(LIVE_WITHIN_SECS))
    .fetch_all(pool)
    .await
    .map_err(|err| tracing::warn!(%err, "pods lookup failed"))
    .unwrap_or_default()
}

/// Boot cleanup + heartbeat loop. On start, drop any rows a previous
/// incarnation of THIS pod name left behind (a crashed process can't
/// unregister) and register this pod in `pods`; then re-upsert our rows every
/// [`HEARTBEAT_SECS`] and opportunistically reap long-dead rows from crashed
/// peers so the tables stay small. Spawned from `main` only when registration
/// is enabled (pod IP known).
pub async fn heartbeat_task(state: AppState) {
    boot_register(&state).await;
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(HEARTBEAT_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        heartbeat_tick(&state.pool, &state.presence).await;
    }
}

/// Boot-time presence bookkeeping: drop stale `ws_presence` rows from a prior
/// incarnation of this pod name and self-register in `pods`.
/// Upsert: a restarted pod with the same name simply takes its row over.
async fn boot_register(state: &AppState) {
    if let Err(err) = sqlx::query("DELETE FROM ws_presence WHERE pod = $1")
        .bind(&state.presence.pod)
        .execute(&state.pool)
        .await
    {
        tracing::warn!(%err, "ws_presence boot cleanup failed");
    }
    if let Some(ip) = state.presence.ip.as_deref()
        && let Err(err) = sqlx::query(
            "INSERT INTO pods (pod, pod_ip, started_at, heartbeat_at) \
             VALUES ($1, $2, now(), now()) \
             ON CONFLICT (pod) DO UPDATE SET \
               pod_ip = EXCLUDED.pod_ip, started_at = now(), heartbeat_at = now()",
        )
        .bind(&state.presence.pod)
        .bind(ip)
        .execute(&state.pool)
        .await
    {
        tracing::warn!(%err, "pods register failed");
    }
}

/// One heartbeat: re-upsert this pod's `pods` row and every `ws_presence` row
/// it owns, then reap rows crashed PEERS never deleted. Upserts rather than
/// updates because a DB outage longer than [`REAP_AFTER_SECS`] lets a peer reap
/// our rows; an UPDATE would then match nothing forever and this replica would
/// silently vanish from fan-out until restarted. The reaps exclude our own rows
/// so a replica can never delete itself.
async fn heartbeat_tick(pool: &PgPool, me: &PodIdentity) {
    let Some(ip) = me.ip.as_deref() else { return };
    match sqlx::query_scalar::<_, bool>(
        "INSERT INTO pods (pod, pod_ip, started_at, heartbeat_at) \
         VALUES ($1, $2, now(), now()) \
         ON CONFLICT (pod) DO UPDATE SET pod_ip = EXCLUDED.pod_ip, heartbeat_at = now() \
         RETURNING (xmax = 0)",
    )
    .bind(&me.pod)
    .bind(ip)
    .fetch_one(pool)
    .await
    {
        Ok(true) => tracing::warn!(pod = me.pod, "pods row was missing; re-registered"),
        Ok(false) => {}
        Err(err) => tracing::warn!(%err, "pods heartbeat failed"),
    }

    let owned: Vec<(Kind, Uuid)> = me.owned.iter().map(|r| *r.key()).collect();
    for (kind, entity_id) in owned {
        if let Err(err) = sqlx::query(
            "INSERT INTO ws_presence (kind, entity_id, pod, pod_ip, connected_at, heartbeat_at) \
             VALUES ($1, $2, $3, $4, now(), now()) \
             ON CONFLICT (kind, entity_id) DO UPDATE SET heartbeat_at = now() \
             WHERE ws_presence.pod = EXCLUDED.pod",
        )
        .bind(kind.as_str())
        .bind(entity_id)
        .bind(&me.pod)
        .bind(ip)
        .execute(pool)
        .await
        {
            tracing::warn!(%err, %entity_id, kind = kind.as_str(), "ws_presence heartbeat failed");
        }
    }

    for sql in [
        "DELETE FROM ws_presence \
         WHERE pod <> $1 AND heartbeat_at < now() - make_interval(secs => $2)",
        "DELETE FROM pods WHERE pod <> $1 AND heartbeat_at < now() - make_interval(secs => $2)",
    ] {
        let _ = sqlx::query(sql).bind(&me.pod).bind(f64::from(REAP_AFTER_SECS)).execute(pool).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> Option<PgPool> {
        let url = crate::routes::gateway::test_db_url("presence")?;
        Some(
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(2)
                .connect(&url)
                .await
                .expect("connect test db"),
        )
    }

    fn prefix(tag: &str) -> String {
        format!("cct1070-{tag}-{}-", Uuid::new_v4().simple())
    }

    /// `live_peer_pods` is unscoped by design — in production every `pods` row
    /// is a real peer — so on a shared test database it also returns other
    /// tests' pods. Assert against this test's own IPs only.
    async fn live_peer_ips(pool: &PgPool, self_pod: &str, prefix: &str) -> Vec<String> {
        let mut ips = live_peer_pods(pool, self_pod).await;
        ips.retain(|ip| ip.starts_with(prefix));
        ips
    }

    async fn wipe(pool: &PgPool, prefix: &str) {
        let like = format!("{prefix}%");
        sqlx::query("DELETE FROM pods WHERE pod LIKE $1").bind(&like).execute(pool).await.unwrap();
        sqlx::query("DELETE FROM ws_presence WHERE pod LIKE $1")
            .bind(&like)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn age_pod(pool: &PgPool, pod: &str, secs: i32) {
        sqlx::query(
            "UPDATE pods SET heartbeat_at = now() - make_interval(secs => $2) WHERE pod = $1",
        )
        .bind(pod)
        .bind(f64::from(secs))
        .execute(pool)
        .await
        .unwrap();
    }

    async fn pod_exists(pool: &PgPool, pod: &str) -> bool {
        sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM pods WHERE pod = $1)")
            .bind(pod)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn ws_owner(pool: &PgPool, kind: Kind, id: Uuid) -> Option<String> {
        sqlx::query_scalar::<_, String>(
            "SELECT pod FROM ws_presence WHERE kind = $1 AND entity_id = $2",
        )
        .bind(kind.as_str())
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    /// A DB outage past the reap window lets a peer delete our `pods` row. The
    /// next heartbeat must bring it back so fan-out resumes without a restart.
    #[tokio::test]
    async fn heartbeat_re_registers_a_reaped_pods_row() {
        let Some(pool) = pool().await else { return };
        let prefix = prefix("rereg");
        let ip_a = format!("{prefix}10.0.0.1");
        let a = PodIdentity::for_test(&format!("{prefix}a"), &ip_a);
        let b = PodIdentity::for_test(&format!("{prefix}b"), &format!("{prefix}10.0.0.2"));

        heartbeat_tick(&pool, &a).await;
        assert!(pod_exists(&pool, &a.pod).await);
        sqlx::query("DELETE FROM pods WHERE pod = $1").bind(&a.pod).execute(&pool).await.unwrap();
        assert!(live_peer_ips(&pool, &b.pod, &prefix).await.is_empty());

        heartbeat_tick(&pool, &a).await;
        assert_eq!(live_peer_ips(&pool, &b.pod, &prefix).await, vec![ip_a]);
        wipe(&pool, &prefix).await;
    }

    /// The reap deletes stale peers but never this pod's own row, even when
    /// our own heartbeat is older than the reap window.
    #[tokio::test]
    async fn reap_skips_self_and_removes_stale_peers() {
        let Some(pool) = pool().await else { return };
        let prefix = prefix("reap");
        let ip_a = format!("{prefix}10.0.1.1");
        let a = PodIdentity::for_test(&format!("{prefix}a"), &ip_a);
        let b = PodIdentity::for_test(&format!("{prefix}b"), &format!("{prefix}10.0.1.2"));
        let c = PodIdentity::for_test(&format!("{prefix}c"), &format!("{prefix}10.0.1.3"));
        for p in [&a, &b, &c] {
            heartbeat_tick(&pool, p).await;
        }
        age_pod(&pool, &a.pod, REAP_AFTER_SECS + 60).await;
        age_pod(&pool, &b.pod, REAP_AFTER_SECS + 60).await;

        heartbeat_tick(&pool, &a).await;
        assert!(pod_exists(&pool, &a.pod).await, "a must never reap itself");
        assert!(!pod_exists(&pool, &b.pod).await, "stale peer b is reaped");
        assert!(pod_exists(&pool, &c.pod).await, "live peer c is kept");
        assert_eq!(live_peer_ips(&pool, &c.pod, &prefix).await, vec![ip_a]);
        wipe(&pool, &prefix).await;
    }

    /// Owned `ws_presence` rows are re-upserted on every tick, so a reaped row
    /// returns; a row a peer has since taken over is left alone.
    #[tokio::test]
    async fn heartbeat_restores_owned_ws_rows_without_stealing_peer_rows() {
        let Some(pool) = pool().await else { return };
        let prefix = prefix("ws");
        let ip_a = format!("{prefix}10.0.2.1");
        let a = PodIdentity::for_test(&format!("{prefix}a"), &ip_a);
        let reaped = Uuid::new_v4();
        let moved = Uuid::new_v4();
        a.owned.insert((Kind::Daemon, reaped), ());
        a.owned.insert((Kind::Session, moved), ());
        sqlx::query(
            "INSERT INTO ws_presence (kind, entity_id, pod, pod_ip) VALUES ('session', $1, $2, $3)",
        )
        .bind(moved)
        .bind(format!("{prefix}b"))
        .bind(format!("{prefix}10.0.2.2"))
        .execute(&pool)
        .await
        .unwrap();

        heartbeat_tick(&pool, &a).await;
        assert_eq!(ws_owner(&pool, Kind::Daemon, reaped).await.as_deref(), Some(a.pod.as_str()));
        assert_eq!(
            ws_owner(&pool, Kind::Session, moved).await.as_deref(),
            Some(format!("{prefix}b").as_str())
        );
        assert_eq!(
            peer_owner_ip(&pool, &format!("{prefix}b"), Kind::Daemon, reaped).await.as_deref(),
            Some(ip_a.as_str())
        );
        wipe(&pool, &prefix).await;
    }
}
