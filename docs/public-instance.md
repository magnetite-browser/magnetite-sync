# The public instance

StackForge runs magnetite-sync at `https://sync.magnetitebrowser.org`, free to
use, hosted by Hetzner in Germany. Magnetite preselects it in Settings → Sync.
This page is how it is set up; it is also a template for anyone running an
open server.

## Who may start a chain

Anyone, without an account (`MAGNETITE_SYNC_REGISTRATION=open`). Which data
belongs to whom is decided by the chain key, which only devices that know the
24 words have. When growth or abuse needs it, starting a chain will require an
email address confirmed by double opt-in; that brings personal data with it
and will be documented here and in the privacy policy first.

## Limits

| Setting | Value | Why |
|---|---|---|
| `MAGNETITE_SYNC_REGISTRATION` | `open` | no account needed |
| `MAGNETITE_SYNC_MAX_NEW_CHAINS_PER_DAY` | `100` | sets how fast the cap can fill |
| `MAGNETITE_SYNC_MAX_CHAINS` | sized to the disk: chains × 32 MiB × 3 must fit, with free space to spare | a full server refuses new chains instead of filling up |
| `MAGNETITE_SYNC_RETENTION_DAYS` | `400` | chains nobody uses are deleted |
| per chain (built in) | 8 collections, 32 MiB, 10 writes a minute; deleted after 7 days if it never stored anything | one chain cannot fill the disk or keep it busy |
| reverse proxy, per IPv4 address or IPv6 /64 | `POST /v2/chain`: 3 at once, then 1 a minute; all requests: 20 at once, then 5 a second; 8 connections | slows down mass sign-ups and floods |

The chain cap limits the damage, it does not decide who gets in: someone with
many addresses can fill it over time (a chain that stores a little is kept
for 400 days), at the pace of the daily budget. When that happens, the server
refuses new chains (403) and starting one will need an email address (above).
Clients with chains of their own can also keep the server busy (503): each
process takes 4 collection transfers and 16 other requests at once, only
requests with a valid signature take a slot, a chain writes one collection
at a time, and nginx lets a client have two transfers at a time. Two
addresses with two chains each, dripping their uploads, hold one process's
transfer slots for up to two minutes at a time; small requests go on. They
cannot make it use more memory or disk than its limits allow.

## Reverse proxy

TLS, per-client limits and connection limits live in nginx; the server never
sees client addresses. Checked with `nginx -t` on nginx 1.31.8
(`docker.io/library/nginx`) with a throwaway certificate, and against a
running server: IPv6 clients share limits per /64, a request signed for this
host passes, larger bodies get 413 from nginx, and a collection write for an
unknown chain is refused after its first 64 KiB.

```nginx
# http context, e.g. /etc/nginx/conf.d/magnetite-sync.conf

# No access log. The error log only takes crit and worse; rate limiting is
# logged at info and never reaches it.
access_log off;
error_log /var/log/nginx/error.log crit;

# One key per client: an IPv4 address, or the /64 of an IPv6 address (the
# first 8 of its 16 bytes), since one host usually has a whole /64.
map $binary_remote_addr $sync_client {
    "~(?s)^(?<net>.{8}).{8}" $net;
    default                  $binary_remote_addr;
}
# Only creating a chain counts against the sign-up limit: other methods get
# an empty key, which nginx does not limit.
map $request_method $sync_signup {
    POST    $sync_client;
    default "";
}
limit_req_zone  $sync_signup zone=sync_new:10m rate=1r/m;
limit_req_zone  $sync_client zone=sync_all:10m rate=5r/s;
limit_conn_zone $sync_client zone=sync_conn:10m;
limit_conn_zone $sync_client zone=sync_transfers:10m;

# Any other name is refused: signatures are checked against the Host
# header, so only the server's own name may reach it.
server {
    listen 443 ssl default_server;
    listen [::]:443 ssl default_server;
    ssl_reject_handshake on;
    return 444;
}

server {
    listen 443 ssl;
    listen [::]:443 ssl;
    http2 on;
    server_name sync.magnetitebrowser.org;
    ssl_certificate     /etc/letsencrypt/live/sync.magnetitebrowser.org/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/sync.magnetitebrowser.org/privkey.pem;

    limit_req_status 429;
    limit_conn_status 429;
    limit_req_log_level info;
    limit_conn_log_level info;
    limit_conn sync_conn 8;
    limit_req zone=sync_all burst=20 nodelay;

    client_max_body_size 4k;
    client_header_timeout 10s;
    client_body_timeout 10s;
    send_timeout 30s;
    keepalive_timeout 30s;

    proxy_http_version 1.1;
    proxy_set_header Host $http_host;
    proxy_connect_timeout 5s;
    proxy_send_timeout 30s;
    proxy_read_timeout 30s;
    # Answers are buffered in memory only, never in temporary files.
    proxy_max_temp_file_size 0;

    location = /v2/chain {
        limit_req zone=sync_all burst=20 nodelay;
        limit_req zone=sync_new burst=2 nodelay;
        proxy_pass http://127.0.0.1:8787;
    }
    location /v2/collections/ {
        limit_conn sync_conn 8;
        limit_conn sync_transfers 2;
        client_max_body_size 22m;
        proxy_request_buffering off;
        proxy_pass http://127.0.0.1:8787;
    }
    location / {
        proxy_pass http://127.0.0.1:8787;
    }
}
```

- Clients are keyed by IPv4 address or IPv6 /64, which is usually one
  household or one server. nginx listens on IPv4 and IPv6 separately (its
  default), so IPv4 clients do not arrive as IPv4-mapped IPv6 addresses,
  which would all share one key.
- `rate=1r/m` with `burst=2 nodelay`: a client may start three chains at
  once, then one a minute. Failed invite codes count the same.
- `Host $http_host` passes the host as the client sent it, port included:
  signatures cover it. Other names never reach the server.
- Collection writes are streamed to the server (`proxy_request_buffering
  off`), which refuses a bad signature, then an unknown chain, before the
  body arrives; buffered, nginx would first take up to 22 MiB of every
  request.
- A client has at most two collection transfers at a time (`sync_transfers`;
  a device reads and writes one collection after the other), so one client
  cannot take a server process's four transfer slots.
  The largest legitimate write is about 21.4 MiB; everything else stays under
  4 KiB.
- Answers are buffered in memory only (`proxy_max_temp_file_size 0`): no
  ciphertext in temporary files. A client that reads slowly holds its answer
  on the server, which closes the connection after 120 seconds.
- No access log. The error log takes only `crit` and worse; rate limiting is
  logged at `info`, so it never reaches it.

## Running it

The container image (`ghcr.io/magnetite-browser/magnetite-sync:<version>`)
and Postgres as `compose.yml` runs them: the database on a volume, reachable
only from the server; the server bound to `127.0.0.1:8787`, restarted by the
system, with the limits of `compose.yml` (read-only, no capabilities, 1 GiB
of memory). Back up the database daily with `pg_dump`; backups are kept 14
days.

Watch: disk use of the database, the number of chains against the cap, new
chains per day against the budget, and the health endpoint (`/v2/health`).

## Data protection

- The server stores pseudonymous data per chain: a public key, two dates,
  its writes in the current minute, opaque collection ids, versions and
  ciphertext, and for ten minutes after a deletion the deleted chain's id. It cannot tell who a chain
  belongs to; whoever holds the 24 words can.
- Client IP addresses reach only the proxy. It processes them, with the chain
  id each signed request carries, in memory only, for TLS and rate limiting,
  on the basis of Art. 6(1)(f) GDPR (running and protecting the service).
  There is no access log, and neither rate limiting nor client errors reach
  the error log. Only a critical fault, such as a rare unexpected TLS error,
  can put a client address there; the error log is kept 7 days.
- Backups are kept 14 days: a deleted chain is gone from them 14 days later.
- Hetzner Online GmbH hosts the server as a processor under a data
  processing agreement (Art. 28 GDPR); the server is in Germany.
- The privacy policy at magnetitebrowser.org/privacy/ names StackForge as
  the controller, Hetzner as hosting provider and the above.

## Running an open instance yourself

The same setup works for anyone who wants to offer a server to others:

- the container and the reverse proxy above, under your own name;
- `MAGNETITE_SYNC_MAX_CHAINS` sized to the disk;
- daily backups kept no longer than you promise, and error log rotation;
- a data processing agreement with your hosting provider, and a privacy
  policy and imprint that describe the service;
- monitoring of disk use, chain count, new chains per day and health.
