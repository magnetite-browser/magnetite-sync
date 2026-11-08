use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;
use magnetite_sync::{AppState, Db, Limits, Registration};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // RUST_LOG's directives, info when it is unset or empty (invalid ones
    // are reported and skipped).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();
    let bind: SocketAddr = std::env::var("MAGNETITE_SYNC_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8787".into())
        .parse()
        .context("MAGNETITE_SYNC_BIND")?;
    // Never logged: it carries the database password.
    let database_url = std::env::var("MAGNETITE_SYNC_DATABASE_URL").context(
        "MAGNETITE_SYNC_DATABASE_URL must name the Postgres database (postgres://user:password@host/database)",
    )?;
    // Checked once: a wrong setting does not get better by retrying.
    let database = Db::options(&database_url)?;
    // Unset means closed: nobody starts chains on a server by accident.
    let registration =
        Registration::from_env(&std::env::var("MAGNETITE_SYNC_REGISTRATION").unwrap_or_default());
    // Delete chains nobody used for this many days (0 keeps them forever).
    let retention_days = std::env::var("MAGNETITE_SYNC_RETENTION_DAYS")
        .ok()
        .map_or(Ok(400), |v| v.parse::<u32>())
        .context("MAGNETITE_SYNC_RETENTION_DAYS")?;
    let retention_days = (retention_days > 0).then_some(retention_days);
    let mut limits = Limits::default();
    if let Ok(v) = std::env::var("MAGNETITE_SYNC_MAX_CHAINS") {
        limits.max_chains = v.parse().context("MAGNETITE_SYNC_MAX_CHAINS")?;
    }
    if let Ok(v) = std::env::var("MAGNETITE_SYNC_MAX_NEW_CHAINS_PER_DAY") {
        limits.max_new_chains_per_day =
            v.parse().context("MAGNETITE_SYNC_MAX_NEW_CHAINS_PER_DAY")?;
    }
    let shutdown = shutdown_signal()?;
    let db = connect(&database).await?;
    // `Registration`'s Debug never prints the invite code.
    tracing::info!(%bind, ?registration, ?retention_days, ?limits, "magnetite-sync starting");
    magnetite_sync::serve(
        bind,
        AppState {
            db,
            registration,
            retention_days,
            limits,
        },
        shutdown,
    )
    .await
}

/// Connect to the database. It may come up after the server (Kubernetes starts
/// both at once), so this keeps trying for about a minute.
async fn connect(options: &sqlx::postgres::PgConnectOptions) -> anyhow::Result<Db> {
    const ATTEMPTS: u32 = 10;
    let mut attempt = 1;
    loop {
        match Db::connect_with(options.clone()).await {
            Ok(db) => return Ok(db),
            Err(e) if attempt < ATTEMPTS => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    attempt,
                    "database not reachable yet"
                );
                tokio::time::sleep(Duration::from_secs(2)).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Resolves on Ctrl-C or SIGTERM. In a container the server is PID 1, which
/// the kernel does not stop for signals it has no handler for.
fn shutdown_signal() -> anyhow::Result<impl Future<Output = ()> + Send + 'static> {
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("SIGTERM handler")?;
    Ok(async move {
        #[cfg(unix)]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down");
    })
}
