//! Postgres storage. Several server processes may share one database, so
//! everything that must hold across requests is decided in the database: the
//! cap on chains and the daily budget of new ones, tombstones of deleted
//! chains, the write limit and the quotas, each inside one transaction.

use std::time::Duration;

use std::sync::Arc;

use anyhow::{Context, Result};
use magnetite_sync_proto::{CollectionResponse, MAX_CLOCK_SKEW_MS, Sealed};
use sqlx::Connection as _;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::Limits;

/// The schema, applied at start. The migrator takes an advisory lock, so
/// processes that start together apply it once.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Held by every transaction that adds or deletes chains, so that the cap,
/// the daily budget and the tombstones see one order of events.
pub(crate) const CHAINS_LOCK: i64 = 0x6d61_676e_6574_6974; // "magnetit"

/// A request signed just before a chain was deleted stays valid for up to
/// the clock skew past its time, which may itself be up to the skew ahead.
pub(crate) const TOMBSTONE_TTL: Duration =
    Duration::from_millis(2 * MAX_CLOCK_SKEW_MS.unsigned_abs());

/// The window `Limits::writes_per_minute` counts in.
const WRITE_WINDOW: Duration = Duration::from_secs(60);

/// How long getting a connection may take.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Settings of every session (Postgres's defaults wait for ever): a request
/// waiting on a lock gives up after 3 s (and is told the server is busy), a
/// statement is cut after 30 s, and a transaction left open (a process that
/// froze mid-request) ends after 30 s. The limits rely on read committed:
/// each statement after the advisory or row lock sees what the lock's last
/// holder committed.
const SESSION: [(&str, &str); 4] = [
    ("lock_timeout", "3s"),
    ("statement_timeout", "30s"),
    ("idle_in_transaction_session_timeout", "30s"),
    ("default_transaction_isolation", "read committed"),
];

/// Parameters `MAGNETITE_SYNC_DATABASE_URL` may carry. sqlx would log any
/// other one with its value, and ignore it.
const URL_PARAMETERS: [&str; 2] = ["sslmode", "application_name"];

/// Chains a purge deletes per statement, so requests get the database in
/// between.
const PURGE_BATCH: i64 = 8;

/// How long `Db::ready` may take and keeps its answer.
const READY_FOR: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct Db {
    pool: PgPool,
    /// When `ready` last asked the database, and its answer.
    ready: Arc<Mutex<Option<(Instant, bool)>>>,
}

/// Result of `Db::create_chain`.
#[derive(Debug, PartialEq, Eq)]
pub enum Create {
    Created,
    /// The chain exists already.
    Exists,
    /// The chain was deleted within `TOMBSTONE_TTL`.
    Deleted,
    /// The server holds `Limits::max_chains`.
    Full,
    /// `Limits::max_new_chains_per_day` chains were started today.
    DailyLimit,
}

/// Result of `Db::put_collection`.
#[derive(Debug, PartialEq, Eq)]
pub enum Put {
    /// Written; the new version.
    Stored(u64),
    /// `base_version` was stale; the current version.
    Conflict(u64),
    /// A new collection, but the chain has `Limits::max_collections` already.
    TooManyCollections,
    /// The chain would store more than `Limits::max_chain_bytes`.
    OverQuota,
    /// The chain wrote `Limits::writes_per_minute` times this minute.
    TooManyWrites,
    /// The chain was deleted while the write was on its way.
    UnknownChain,
}

/// Days since the Unix epoch: activity is kept at day precision only.
pub fn today() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs() / 86_400).unwrap_or(0))
}

impl Db {
    /// Connect to the database at `url` and bring its schema up to date. The
    /// pool has one connection for each signed request the gate lets run at
    /// once, and one for maintenance; getting one gives up after
    /// `ACQUIRE_TIMEOUT`, also at start when the database cannot be reached.
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_with(Self::options(url)?).await
    }

    /// The connection settings in `url` (`MAGNETITE_SYNC_DATABASE_URL`):
    /// `postgres://`, no parameters but `URL_PARAMETERS`. Nothing of the URL
    /// is quoted in errors; it holds the password.
    pub fn options(url: &str) -> Result<PgConnectOptions> {
        anyhow::ensure!(
            url.starts_with("postgres://") || url.starts_with("postgresql://"),
            "MAGNETITE_SYNC_DATABASE_URL must be a postgres:// URL"
        );
        if let Some((_, query)) = url.split_once('?') {
            let known = query
                .split('&')
                .map(|p| p.split_once('=').map_or(p, |(k, _)| k))
                .all(|key| URL_PARAMETERS.contains(&key));
            anyhow::ensure!(
                known,
                "MAGNETITE_SYNC_DATABASE_URL may carry no parameters but sslmode and \
                 application_name (in a password, write ? # @ / as %3F %23 %40 %2F)"
            );
        }
        let options: PgConnectOptions = url
            .parse()
            .map_err(|_| anyhow::anyhow!("MAGNETITE_SYNC_DATABASE_URL is not a valid URL"))?;
        // This build has no TLS: a mode that needs it could never connect.
        anyhow::ensure!(
            matches!(
                options.get_ssl_mode(),
                PgSslMode::Disable | PgSslMode::Allow | PgSslMode::Prefer
            ),
            "MAGNETITE_SYNC_DATABASE_URL: connections to Postgres are not encrypted yet, so sslmode may only be disable or prefer"
        );
        Ok(options)
    }

    /// `connect`, with the settings from `options`.
    pub async fn connect_with(options: PgConnectOptions) -> Result<Self> {
        // Migrations on a connection of their own, without the requests'
        // timeouts: a later one may take longer than a statement may, or
        // wait for another process's migration.
        let mut conn = sqlx::PgConnection::connect_with(&options)
            .await
            .context("connecting to Postgres")?;
        MIGRATOR
            .run(&mut conn)
            .await
            .context("migrating the database")?;
        conn.close().await.context("connecting to Postgres")?;
        let connections = u32::try_from(crate::gate::RUNNING + crate::gate::RUNNING_LARGE)? + 1;
        let pool = PgPoolOptions::new()
            .max_connections(connections)
            .acquire_timeout(ACQUIRE_TIMEOUT)
            // A connection keeps buffers as large as the largest message it
            // carried (a collection: ~21 MiB): back in the pool, it lets
            // them go, or the pool alone would hold close to a GiB.
            .after_release(|conn, _| {
                Box::pin(async move {
                    conn.shrink_buffers();
                    Ok(true)
                })
            })
            .connect_with(options.options(SESSION))
            .await
            .context("connecting to Postgres")?;
        Ok(Self::from_pool(pool))
    }

    /// A database whose schema is in place.
    pub(crate) fn from_pool(pool: PgPool) -> Self {
        Self {
            pool,
            ready: Arc::default(),
        }
    }

    /// Whether the database answers within a second. One query a second at
    /// most, whoever asks: the answer is kept that long (`/v2/ready` is not
    /// signed, so anyone may ask).
    pub async fn ready(&self) -> bool {
        let mut last = self.ready.lock().await;
        if let Some((at, ok)) = *last
            && at.elapsed() < READY_FOR
        {
            return ok;
        }
        let ok = tokio::time::timeout(READY_FOR, sqlx::query("SELECT 1").execute(&self.pool))
            .await
            .is_ok_and(|r| r.is_ok());
        *last = Some((tokio::time::Instant::now(), ok));
        ok
    }

    pub async fn chain_exists(&self, id: &str) -> Result<bool> {
        Ok(
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM chains WHERE id = $1)")
                .bind(id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Record activity (at most one write per chain and day). Returns whether
    /// the chain exists.
    pub async fn seen(&self, id: &str) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "WITH touched AS (
                 UPDATE chains SET last_seen_day = $2 WHERE id = $1 AND last_seen_day < $2)
             SELECT EXISTS (SELECT 1 FROM chains WHERE id = $1)",
        )
        .bind(id)
        .bind(today())
        .fetch_one(&self.pool)
        .await?)
    }

    /// Make sure chain `id` exists. A new one is added only if it was not
    /// deleted within `TOMBSTONE_TTL`, while the server holds fewer than
    /// `max_chains` and fewer than `max_new_chains_per_day` were started
    /// today (chains deleted since still count).
    pub async fn create_chain(&self, id: &str, limits: &Limits) -> Result<Create> {
        let day = today();
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(CHAINS_LOCK)
            .execute(&mut *tx)
            .await?;
        let (exists, deleted, chains, started_today): (bool, bool, i64, i64) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM chains WHERE id = $1),
                    EXISTS (SELECT 1 FROM tombstones
                            WHERE id = $1 AND deleted_at > now() - make_interval(secs => $3)),
                    count(*),
                    coalesce((SELECT started FROM chain_starts WHERE day = $2), 0)
             FROM chains",
        )
        .bind(id)
        .bind(day)
        .bind(TOMBSTONE_TTL.as_secs_f64())
        .fetch_one(&mut *tx)
        .await?;
        let per_day = i64::from(limits.max_new_chains_per_day);
        let outcome = if deleted {
            Create::Deleted
        } else if exists {
            Create::Exists
        } else if chains >= i64::from(limits.max_chains) {
            Create::Full
        } else if per_day > 0 && started_today >= per_day {
            Create::DailyLimit
        } else {
            sqlx::query("INSERT INTO chains (id, created_day, last_seen_day) VALUES ($1, $2, $2)")
                .bind(id)
                .bind(day)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO chain_starts (day, started) VALUES ($1, 1)
                 ON CONFLICT (day) DO UPDATE SET started = chain_starts.started + 1",
            )
            .bind(day)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM chain_starts WHERE day < $1")
                .bind(day)
                .execute(&mut *tx)
                .await?;
            Create::Created
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// Delete chain `id` and everything in it, and keep its tombstone for
    /// `TOMBSTONE_TTL`. Older tombstones go.
    pub async fn delete_chain(&self, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(CHAINS_LOCK)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM tombstones WHERE deleted_at <= now() - make_interval(secs => $1)")
            .bind(TOMBSTONE_TTL.as_secs_f64())
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO tombstones (id, deleted_at) VALUES ($1, now())
             ON CONFLICT (id) DO UPDATE SET deleted_at = excluded.deleted_at",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM chains WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn collection(&self, chain: &str, id: &str) -> Result<Option<CollectionResponse>> {
        let row: Option<(i64, String, String)> = sqlx::query_as(
            "SELECT version, nonce, ciphertext FROM collections WHERE chain_id = $1 AND id = $2",
        )
        .bind(chain)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(version, nonce, ciphertext)| {
            CollectionResponse::new(
                u64::try_from(version).unwrap_or(0),
                Sealed { nonce, ciphertext },
            )
        }))
    }

    /// Optimistic write: succeeds only if the stored version equals
    /// `base_version` (0 for a new collection) and the chain stays within
    /// `limits`. Every write that is not refused for the write limit counts
    /// against it, whatever comes of it.
    pub async fn put_collection(
        &self,
        chain: &str,
        id: &str,
        base_version: u64,
        sealed: &Sealed,
        limits: &Limits,
    ) -> Result<Put> {
        let mut tx = self.pool.begin().await?;
        // Locks the chain until the transaction ends, so writes of one chain
        // take turns and its quota holds.
        let writes: Option<i32> = sqlx::query_scalar(
            "UPDATE chains SET
                 window_writes = CASE WHEN window_start > now() - make_interval(secs => $2)
                                      THEN window_writes + 1 ELSE 1 END,
                 window_start = CASE WHEN window_start > now() - make_interval(secs => $2)
                                     THEN window_start ELSE now() END
             WHERE id = $1
             RETURNING window_writes",
        )
        .bind(chain)
        .bind(WRITE_WINDOW.as_secs_f64())
        .fetch_optional(&mut *tx)
        .await?;
        // Returning without a commit rolls back: a refused write does not count.
        let Some(writes) = writes else {
            return Ok(Put::UnknownChain);
        };
        let per_minute = i64::from(limits.writes_per_minute);
        if per_minute > 0 && i64::from(writes) > per_minute {
            return Ok(Put::TooManyWrites);
        }
        let current: Option<i64> =
            sqlx::query_scalar("SELECT version FROM collections WHERE chain_id = $1 AND id = $2")
                .bind(chain)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let is_new = current.is_none();
        let current = u64::try_from(current.unwrap_or(0)).unwrap_or(0);
        let outcome = if current == base_version {
            // Everything else the chain stores; this collection's old value
            // is replaced, so it does not count.
            let (others, other_bytes): (i64, i64) = sqlx::query_as(
                "SELECT count(*), coalesce(sum(octet_length(nonce) + octet_length(ciphertext)), 0)::bigint
                 FROM collections WHERE chain_id = $1 AND id <> $2",
            )
            .bind(chain)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            let size = i64::try_from(sealed.nonce.len() + sealed.ciphertext.len())?;
            if is_new && others >= i64::from(limits.max_collections) {
                Put::TooManyCollections
            } else if other_bytes.saturating_add(size) > i64::try_from(limits.max_chain_bytes)? {
                Put::OverQuota
            } else {
                let version = current + 1;
                sqlx::query(
                    "INSERT INTO collections (chain_id, id, version, nonce, ciphertext)
                     VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT (chain_id, id) DO UPDATE SET
                         version = excluded.version, nonce = excluded.nonce,
                         ciphertext = excluded.ciphertext",
                )
                .bind(chain)
                .bind(id)
                .bind(i64::try_from(version)?)
                .bind(&sealed.nonce)
                .bind(&sealed.ciphertext)
                .execute(&mut *tx)
                .await?;
                Put::Stored(version)
            }
        } else {
            Put::Conflict(current)
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// The most recent day any chain was used, if there are chains.
    pub async fn newest_activity(&self) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar("SELECT max(last_seen_day) FROM chains")
            .fetch_one(&self.pool)
            .await?)
    }

    /// Delete the chains created before `day` that never stored a
    /// collection. Returns how many.
    pub async fn purge_empty(&self, day: i64) -> Result<u64> {
        self.purge(
            "SELECT id FROM chains c WHERE created_day < $1
             AND NOT EXISTS (SELECT 1 FROM collections WHERE chain_id = c.id)
             LIMIT $2 FOR UPDATE SKIP LOCKED",
            "DELETE FROM chains c WHERE id = ANY($1) AND created_day < $2
             AND NOT EXISTS (SELECT 1 FROM collections WHERE chain_id = c.id)",
            day,
        )
        .await
    }

    /// Delete the chains last used before `day`. Returns how many.
    pub async fn purge_inactive(&self, day: i64) -> Result<u64> {
        self.purge(
            "SELECT id FROM chains WHERE last_seen_day < $1 LIMIT $2 FOR UPDATE SKIP LOCKED",
            "DELETE FROM chains WHERE id = ANY($1) AND last_seen_day < $2",
            day,
        )
        .await
    }

    /// Delete chains in batches, one transaction each: `pick` (`$1` the day,
    /// `$2` the batch size) locks candidates, passing over those a request
    /// holds; `delete` (`$1` their ids, `$2` the day) deletes those that
    /// still qualify, read anew after the lock, so a chain written to or
    /// used meanwhile stays. Ends when a batch comes back short.
    async fn purge(&self, pick: &'static str, delete: &'static str, day: i64) -> Result<u64> {
        let batch = usize::try_from(PURGE_BATCH)?;
        let mut total = 0;
        loop {
            let mut tx = self.pool.begin().await?;
            let ids: Vec<String> = sqlx::query_scalar(pick)
                .bind(day)
                .bind(PURGE_BATCH)
                .fetch_all(&mut *tx)
                .await?;
            total += sqlx::query(delete)
                .bind(&ids)
                .bind(day)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            tx.commit().await?;
            if ids.len() < batch {
                return Ok(total);
            }
        }
    }

    /// Forget what only the last minutes need: write windows that ended,
    /// tombstones that expired. The server runs it every minute
    /// (`SWEEP_EVERY`), so neither lingers, nor ends up in backups.
    pub async fn sweep(&self) -> Result<()> {
        sqlx::query(
            "UPDATE chains SET window_start = NULL, window_writes = 0
             WHERE window_start <= now() - make_interval(secs => $1)",
        )
        .bind(WRITE_WINDOW.as_secs_f64())
        .execute(&self.pool)
        .await?;
        sqlx::query("DELETE FROM tombstones WHERE deleted_at <= now() - make_interval(secs => $1)")
            .bind(TOMBSTONE_TTL.as_secs_f64())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }
}
