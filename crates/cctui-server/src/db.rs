use std::str::FromStr;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool};

/// The embedded migrations. A `static` keeps the list off the stack: built
/// inline in `connect`, it outgrew clippy's `large_stack_arrays` limit.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Read a `u32` env var, falling back to `default` when unset or unparseable.
fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    // Pool sizing is env-tunable so prod can scale connections without a rebuild.
    // Defaults are generous enough to absorb gateway proxying + heartbeats +
    // dispatcher bumps + webui loads without starving the pool (CCT slow-pool fix);
    // acquire_timeout fails fast instead of hanging requests (and the UI) ~30s.
    let max_connections = env_u32("CCTUI_DB_MAX_CONNECTIONS", 40);
    let min_connections = env_u32("CCTUI_DB_MIN_CONNECTIONS", 5);
    let acquire_timeout = env_u32("CCTUI_DB_ACQUIRE_TIMEOUT_SECS", 5);
    let slow_query_ms = env_u32("CCTUI_DB_SLOW_QUERY_MS", 500);

    let connect_options = PgConnectOptions::from_str(database_url)?.log_slow_statements(
        log::LevelFilter::Warn,
        Duration::from_millis(u64::from(slow_query_ms)),
    );

    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .min_connections(min_connections)
        .acquire_timeout(Duration::from_secs(u64::from(acquire_timeout)))
        .connect_with(connect_options)
        .await?;

    reconcile_migration_checksums(&pool).await?;

    MIGRATOR.run(&pool).await?;

    tracing::info!(
        max_connections,
        min_connections,
        acquire_timeout_secs = acquire_timeout,
        slow_query_ms,
        "database connected and migrations applied"
    );
    Ok(pool)
}

async fn reconcile_migration_checksums(pool: &PgPool) -> Result<(), sqlx::Error> {
    const RECONCILED: &[(i64, &str)] = &[
        (
            51,
            "41e5695b0fb3238b1d8ecc979de99c2daff7289615cbf8b08c68f9c9d5b9b4b8bfb3bc2d2183f339fa4f2a6c0aa508ad",
        ),
        (
            55,
            "c23c56dd3c04b56e6874526fd3a43f783e9ff844ccff98e8531d984628725bbf2e1014a19df3204234b3079345a6d177",
        ),
        (
            60,
            "8d0caf988684f0c4894fe6ea43ec74c44453e79c23a68518604e77e78e9a57466d792b8e6253090b80eea4c278d400dd",
        ),
    ];
    let table_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
            .fetch_one(pool)
            .await?;
    if !table_exists {
        return Ok(());
    }

    for (version, checksum_hex) in RECONCILED {
        let checksum = hex::decode(checksum_hex).expect("static checksum hex is valid");
        sqlx::query("UPDATE _sqlx_migrations SET checksum = $1 WHERE version = $2")
            .bind(checksum)
            .bind(version)
            .execute(pool)
            .await?;
    }

    Ok(())
}
