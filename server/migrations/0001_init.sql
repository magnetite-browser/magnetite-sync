-- magnetite-sync schema, protocol v2 (zero knowledge). Once released, a
-- migration never changes, comments included (sqlx checks each file's
-- checksum at start): changes go into a new file. What is stored and why
-- (README, "Data it stores"):
--   chains.id             hex Ed25519 public key; identifies the sync chain.
--   chains.created_day    days since 1970; chains that never stored a
--                         collection are deleted a week later, and it counts
--                         the chains started each day.
--   chains.last_seen_day  days since 1970; chains unused for the retention
--                         period are deleted.
--   chains.window_start,  collection writes in the current minute, for the
--   chains.window_writes  write limit.
--   collections           opaque id, version for optimistic writes, nonce and
--                         ciphertext (base64).
--   tombstones            chains deleted in the last ten minutes, so that
--                         replayed requests cannot bring them back.
--   chain_starts          how many chains were started today, for the daily
--                         budget (a deletion does not give a start back);
--                         earlier days are dropped.
-- No email, password, device, IP or content is stored.
CREATE TABLE chains (
    id             text PRIMARY KEY,
    created_day    bigint NOT NULL,
    last_seen_day  bigint NOT NULL,
    window_start   timestamptz,
    window_writes  integer NOT NULL DEFAULT 0
);

CREATE TABLE collections (
    chain_id    text NOT NULL REFERENCES chains (id) ON DELETE CASCADE,
    id          text NOT NULL,
    version     bigint NOT NULL,
    nonce       text NOT NULL,
    ciphertext  text NOT NULL,
    PRIMARY KEY (chain_id, id)
);
-- Base64 of a ciphertext does not compress: keep it out of line as it is.
ALTER TABLE collections ALTER COLUMN ciphertext SET STORAGE EXTERNAL;

CREATE TABLE tombstones (
    id          text PRIMARY KEY,
    deleted_at  timestamptz NOT NULL
);

CREATE TABLE chain_starts (
    day      bigint PRIMARY KEY,
    started  bigint NOT NULL
);
