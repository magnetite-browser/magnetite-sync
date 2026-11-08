//! magnetite-sync: a small, self-hostable, zero-knowledge sync server for Magnetite.
//! A sync chain is identified only by an Ed25519 public key; every request is
//! signed; collections are opaque, padded ciphertexts. See docs/design.md.
//! Library for tests and embedding; `main.rs` is the binary.

pub mod api;
pub mod db;
mod gate;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::middleware;
pub use db::Db;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub registration: Registration,
    /// Chains unused for this many days are deleted; `None` keeps them forever.
    pub retention_days: Option<u32>,
    pub limits: Limits,
}

/// Who may start new chains (`MAGNETITE_SYNC_REGISTRATION`).
#[derive(Clone, PartialEq, Eq)]
pub enum Registration {
    Open,
    Closed,
    Invite(String),
}

impl Registration {
    /// `open`, `closed` or `invite:<code>`. Anything else, including nothing
    /// and an empty invite code, is `Closed`.
    pub fn from_env(value: &str) -> Self {
        match value.trim() {
            "open" => Self::Open,
            v => v
                .strip_prefix("invite:")
                .filter(|code| !code.is_empty())
                .map_or(Self::Closed, |code| Self::Invite(code.to_string())),
        }
    }
}

/// The server logs its configuration at start: never the invite code.
impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Open => "Open",
            Self::Closed => "Closed",
            Self::Invite(_) => "Invite(redacted)",
        })
    }
}

/// What the server stores and accepts at most, so one chain (or many new
/// ones) cannot fill its disk. The defaults suit a personal or family server.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Chains on this server (`MAGNETITE_SYNC_MAX_CHAINS`); new ones are
    /// refused beyond. A ceiling for the damage, not admission control:
    /// anyone allowed to start chains can fill it over time.
    pub max_chains: u32,
    /// Chains started per day (`MAGNETITE_SYNC_MAX_NEW_CHAINS_PER_DAY`, UTC
    /// days); 0 = unlimited.
    pub max_new_chains_per_day: u32,
    /// Collections per chain. The app uses two (sidebar, settings).
    pub max_collections: u32,
    /// Bytes stored per chain, nonces and ciphertexts of all collections.
    pub max_chain_bytes: u64,
    /// Collection writes per chain and minute; 0 = unlimited.
    pub writes_per_minute: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_chains: 1000,
            max_new_chains_per_day: 100,
            max_collections: 8,
            // One full-size collection (16 MiB padded, ~21.4 MiB as base64)
            // plus small ones.
            max_chain_bytes: 32 * 1024 * 1024,
            // A device syncs every few minutes, two collections at a time.
            writes_per_minute: 10,
        }
    }
}

/// All routes; every response must reach the client within the send deadline.
pub fn router(state: AppState) -> Router {
    Router::new()
        .nest("/v2", api::routes(state))
        .layer(middleware::from_fn(gate::send_deadline))
}

/// Chains that never stored a collection are deleted this many days after
/// they were started.
const EMPTY_CHAIN_DAYS: i64 = 7;
/// Purges are skipped while "today" is further than this past the newest
/// activity: the clock has probably jumped, and would delete every chain.
const CLOCK_GUARD_DAYS: i64 = 7;

/// What one maintenance run deleted.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Purged {
    pub(crate) empty: u64,
    pub(crate) inactive: u64,
}

/// Delete chains that never stored anything after `EMPTY_CHAIN_DAYS` and
/// chains unused for `retention_days`. `None` when skipped because `today`
/// lies far beyond the newest activity. Every server process runs it; they
/// may run it at the same time.
pub(crate) async fn maintain(
    db: &Db,
    retention_days: Option<u32>,
    today: i64,
) -> anyhow::Result<Option<Purged>> {
    let newest = db.newest_activity().await?;
    if newest.is_some_and(|day| today - day > CLOCK_GUARD_DAYS) {
        return Ok(None);
    }
    let empty = db.purge_empty(today - EMPTY_CHAIN_DAYS).await?;
    let inactive = match retention_days {
        Some(days) => db.purge_inactive(today - i64::from(days)).await?,
        None => 0,
    };
    Ok(Some(Purged { empty, inactive }))
}

/// Run `maintain` at start and then once a day.
fn spawn_maintenance(state: &AppState) {
    let (db, retention_days) = (state.db.clone(), state.retention_days);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_hours(24));
        loop {
            tick.tick().await;
            let today = db::today();
            match maintain(&db, retention_days, today).await {
                Ok(Some(Purged {
                    empty: 0,
                    inactive: 0,
                })) => {}
                Ok(Some(Purged { empty, inactive })) => {
                    tracing::info!(empty, inactive, "deleted sync chains");
                }
                Ok(None) => tracing::warn!(
                    today,
                    "no chain was used in the last {CLOCK_GUARD_DAYS} days: deleting nothing until one is (if chains are in use, check the clock)"
                ),
                Err(e) => tracing::error!(error = %e, "maintenance failed"),
            }
        }
    });
}

/// How often `Db::sweep` runs.
const SWEEP_EVERY: Duration = Duration::from_mins(1);

/// Run `Db::sweep` every `SWEEP_EVERY`.
fn spawn_sweep(state: &AppState) {
    let db = state.db.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_EVERY);
        loop {
            tick.tick().await;
            if let Err(e) = db.sweep().await {
                tracing::error!(error = %e, "sweep failed");
            }
        }
    });
}

/// Serve on `bind` until `shutdown` resolves.
pub async fn serve(
    bind: SocketAddr,
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    spawn_maintenance(&state);
    spawn_sweep(&state);
    let listener = gate::Accept::bind(bind).await?;
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<gate::SendDeadline>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;
    Ok(())
}

/// Bind to an ephemeral port (tests) and return the address.
pub async fn serve_ephemeral(
    state: AppState,
) -> anyhow::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = gate::Accept::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    let addr = axum::serve::Listener::local_addr(&listener)?;
    let handle = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<gate::SendDeadline>(),
        )
        .await;
    });
    Ok((addr, handle))
}
