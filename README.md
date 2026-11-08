# magnetite-sync

The zero-knowledge sync server of the [Magnetite](https://magnetitebrowser.org)
browser. One binary and a Postgres database, no accounts, no third-party
services. A
sync chain is known to the server only by an anonymous public key; everything
Magnetite uploads is encrypted and padded on the device. The server cannot read
your spaces, tabs or settings, and never sees the 24-word sync code they are
encrypted with (`docs/design.md`).

Magnetite offers a free public instance at `https://sync.magnetitebrowser.org`,
run by StackForge on servers in Germany (`docs/public-instance.md`). This
repository is for running your own.

| Part | What |
|---|---|
| `server/` | the server (`magnetite-sync`, axum + Postgres) |
| `proto/` | `magnetite-sync-proto`: sync codes, key derivation, request signing, encryption, API types; shared with the browser |
| `charts/` | the Helm chart (`charts/magnetite-sync/README.md`) |
| `docs/` | design, public instance, releases |

## Run it

```sh
export "$(tools/test-postgres.sh)"   # a throwaway Postgres in Docker: DATABASE_URL
MAGNETITE_SYNC_DATABASE_URL="$DATABASE_URL" MAGNETITE_SYNC_REGISTRATION=invite:dev \
  cargo run -p magnetite-sync   # dev: http://127.0.0.1:8787
cargo test --workspace          # each test gets a database of its own on DATABASE_URL
```

Magnetite talks plain `http://` only to `localhost`, `127.0.0.1` and `::1`;
any other server needs `https://`. Production, behind a reverse proxy that
terminates TLS (Caddy example below; it passes the `Host` header on, which
signatures cover), with an invite code so only people you give it to can
start chains. Without `MAGNETITE_SYNC_REGISTRATION`, nobody can.

```sh
MAGNETITE_SYNC_BIND=127.0.0.1:8787 \
MAGNETITE_SYNC_DATABASE_URL=postgres://magnetite_sync:password@127.0.0.1/magnetite_sync \
MAGNETITE_SYNC_REGISTRATION=invite:choose-a-long-random-code \
RUST_LOG=info magnetite-sync
```

```
# Caddyfile
sync.example.org {
    reverse_proxy 127.0.0.1:8787
}
```

Containers (builds the image from this repository): `compose.yml` runs the
server and Postgres 18, which only the server can reach. It refuses to start
without `MAGNETITE_SYNC_REGISTRATION` and `POSTGRES_PASSWORD` (put them in a
`.env` file next to it) and runs the server read-only, without capabilities,
with 1 GiB of memory and 128 processes at most:

```sh
printf 'MAGNETITE_SYNC_REGISTRATION=invite:%s\nPOSTGRES_PASSWORD=%s\n' \
  "$(openssl rand -hex 16)" "$(openssl rand -hex 32)" > .env
docker compose up -d
```

Kubernetes: the Helm chart in `charts/magnetite-sync`, released as
`oci://ghcr.io/magnetite-browser/charts/magnetite-sync`, runs the server on a
Postgres of yours (its README).

Released images: `ghcr.io/magnetite-browser/magnetite-sync:<version>` (amd64
and arm64). The image is the static server binary on an empty base: no
shell, no package manager. It runs as user 10001, writes no files and carries
its licence files in `/licenses`. Each GitHub release also carries the same
binary for Linux (amd64, arm64) as a tar.gz with its licence files, and
`SHA256SUMS`.

All data is in Postgres (tested with 18): back up the database
(`pg_dump`). Several server processes may share one database; every limit
below holds across them. The connection to the database is not encrypted:
keep Postgres on the same host or a private network.

| Variable | Default | Meaning |
|---|---|---|
| `MAGNETITE_SYNC_BIND` | `127.0.0.1:8787` | Listen address |
| `MAGNETITE_SYNC_DATABASE_URL` | (required) | `postgres://user:password@host:5432/database`, with no parameters but `sslmode` (`disable` or `prefer`: no TLS yet) and `application_name`; the server creates its tables there. Never logged |
| `MAGNETITE_SYNC_REGISTRATION` | `closed` | `open`, `closed`, or `invite:<code>` (who may start new chains); use `invite:` with a long random code (`openssl rand -hex 16`) on any server others can reach, unless it is meant to be public (then rate-limit `POST /v2/chain` in the proxy, as the public instance does: `docs/public-instance.md`). Guessing a code is slowed only by the proxy's limits, so make it long |
| `MAGNETITE_SYNC_RETENTION_DAYS` | `400` | delete chains unused this long; `0` keeps them |
| `MAGNETITE_SYNC_MAX_CHAINS` | `1000` | chains the server holds at most; new ones are refused beyond. A ceiling for the damage, sized to the disk (Limits), not admission control |
| `MAGNETITE_SYNC_MAX_NEW_CHAINS_PER_DAY` | `100` | chains started per day (UTC), whoever starts them; `0` = unlimited |
| `RUST_LOG` | `info` | Log filter |

The server keeps the clock that signed requests are checked against
(±5 minutes); run it with NTP. The invite code is never logged.

## Limits

- A chain holds at most 8 collections (Magnetite uses two) and 32 MiB in
  total; one collection at most 16 MiB of padded data. It writes at most 10
  times a minute.
- Chains that never stored a collection are deleted after 7 days. At most
  `MAGNETITE_SYNC_MAX_NEW_CHAINS_PER_DAY` chains start per day, and the
  server holds at most `MAGNETITE_SYNC_MAX_CHAINS`. The cap protects the
  disk, not the service: with open registration it can be filled over time,
  and then nobody new gets in.
- Disk: plan with chains × 32 MiB × 3 for the database, plus Postgres's WAL
  (up to `max_wal_size`, 1 GB by default) and free space. A rewritten
  collection is stored anew, and the old version's space is reused only
  after vacuum (one 21 MiB collection, rewritten, took 44 MB of table and
  22 MB of WAL per write).
- Every signature is checked before the server looks the chain up, so only
  the key holder learns whether a chain exists. A collection write carries
  its body's hash ahead of the body (`Content-Digest`): the server checks
  the signature against it, then the chain, and only then reads the body,
  which must match the hash. Any other request carries at most 4 KiB, read
  before its signature is checked.
- Deletions of unused chains pause while no chain has been used for 7 days:
  a clock that jumped ahead would otherwise delete everything.
- A request takes a slot only once its signature is verified. Each server
  process runs 16 requests at once, and apart from them 4 collection reads
  or writes; a chain writes one collection at a time (another write gets
  429). Up to 64 more requests and 16 more transfers wait at most 2 seconds
  for a slot, the rest get 503. A collection write must arrive and be
  answered within 120 seconds; any other request must arrive within 10
  seconds and is answered within 10 more. GET and DELETE carry no body. A
  response must reach the client within 120 seconds, and a keep-alive
  connection idle that long is closed. `/v2/health` and `/v2/ready` are
  outside these limits. At most about 1 GiB of memory per
  process, and 21 database connections. A request that waits for a lock in
  the database longer than 3 seconds gets 503; a statement is cut after 30.
- For 10 minutes after a chain is deleted, requests for it are refused, so
  replaying requests captured before cannot bring it back.
- Per-IP limits, connection counts and slow request headers are the reverse
  proxy's job (nginx `limit_req` and `limit_conn`, or a rate-limit module for
  Caddy); the server never sees client addresses. A proxy that buffers
  request bodies (nginx by default) takes whole uploads before the server can
  refuse them: let it stream collection writes (`docs/public-instance.md`).

## Data it stores

| Field | Why |
|---|---|
| chain id (Ed25519 public key) | verify signatures, separate chains |
| day created | delete chains that never stored anything, count new chains per day |
| day last used | delete inactive chains |
| start and count of the current minute's writes, cleared a minute later | the write limit |
| collection id (derived, opaque), version | optimistic writes |
| nonce + ciphertext (padded) | the encrypted data |
| ids of chains deleted in the last 10 minutes, then dropped | refuse replayed requests |
| number of chains started today | the daily budget of new chains |

No email, password, device name, IP address or content. The server does not
log requests. `DELETE /v2/chain` removes a chain and everything in it.
Postgres reuses the space of deleted rows later; until then, and in its WAL
and in backups until they rotate, deleted ciphertext can stay on disk,
encrypted on the device as it always is. Back up the database; there is
nothing else.

## API

Base path `/v2`, JSON bodies. Every call except `/health` and `/ready` carries
`Authorization: Magnetite-Sig <pubkey-hex>.<unix-ms>.<signature>`, an Ed25519
signature over `magnetite-sync/v2\n<METHOD>\n<host>\n<path below /v2>\n<unix-ms>\n<sha256(body) hex>`;
`<host>` is the `Host` header, lowercase, without the ports 443 and 80.
Collection writes also send that hash as `Content-Digest: sha-256=:<base64>:`
(RFC 9530), so the server can check the signature before it reads the body.

| Method | Path | Purpose |
|---|---|---|
| GET | `/health` | liveness (never touches the database) |
| GET | `/ready` | 200 while the database answers, else 503 (asked at most once a second) |
| POST | `/chain` | create the chain for this key, `{invite?}`; 201, or 204 if it exists; 403 if registration refuses it or the server is full, 503 if today's new chains are used up, 410 if it was deleted a moment ago |
| GET | `/chain` | 204 if the chain exists (joining), 404 if not |
| DELETE | `/chain` | erase the chain and all data; 204, or 404 if it does not exist |
| GET | `/collections/{id}` | `{version, nonce, ciphertext}`; 404 if the collection or the chain does not exist |
| PUT | `/collections/{id}` | `{base_version, nonce, ciphertext}` → `{version}`; 409 with `current_version` on conflict; 403 if the chain has too many collections, 413 if over its quota, 429 if it writes too often or another of its writes is under way; 404 if the chain does not exist; 400 without a `Content-Digest` or when the body does not match it |

Any signed request: 401 for a missing, stale or bad signature, 503 when the
server is busy, 408 when the request took too long.

Clients accept only an answer to a write whose `version` is the one they
encrypted for (`base_version + 1`), and never go back to an older version.

## Security

Report vulnerabilities to security@magnetitebrowser.org (see `SECURITY.md` in
the organisation profile), not in public issues.

## Licence

magnetite-sync is free software under the European Union Public Licence v1.2
(`LICENSE`). If you run a modified version for others over a network, you must
offer them its source under the EUPL-1.2, as a copy or a link to where it is
freely available, for as long as you run it. Third-party crates keep their own
licences: the container image and the release archives carry their notices
(`THIRD_PARTY_NOTICES.md`, written by `tools/third-party-notices.py` during the
build), musl's licence and the Rust standard library's in `licenses/`.
