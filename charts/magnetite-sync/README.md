# magnetite-sync Helm chart

Runs the magnetite-sync server (the repository's README) on Kubernetes: server
pods, a Service, a NetworkPolicy and, if you want, an Ingress. The data lives
in a Postgres database that this chart does not run: CloudNativePG, a managed
database or any other (tested with Postgres 18). The connection to it is not
encrypted, so keep the database in the cluster or on a private network.

## Install

```sh
kubectl create secret generic magnetite-sync \
  --from-literal=registration="invite:$(openssl rand -hex 16)" \
  --from-literal=url='postgres://magnetite_sync:<password>@<host>:5432/magnetite_sync'
helm install sync oci://ghcr.io/magnetite-browser/charts/magnetite-sync --version 0.1.0 \
  --set registration.existingSecret.name=magnetite-sync \
  --set database.existingSecret.name=magnetite-sync
```

With the settings in a Secret of your own, the invite code and the database
password stay out of Helm's release history (`helm get values`). Settings are
read when a pod starts: after changing one, restart the pods
(`kubectl rollout restart deployment/<release>-magnetite-sync`). With
CloudNativePG, the cluster's application Secret holds the URL:
`--set database.existingSecret.name=<cluster>-app --set database.existingSecret.key=uri`.

The server creates its tables when it starts; pods that start together apply
them once.

## In front of it

Magnetite reaches any server but localhost over `https://` only. The ingress
or proxy in front of the service must:

- terminate TLS and pass the `Host` header on: requests are signed for it;
- take request bodies of up to 22 MiB and stream them to the server instead
  of buffering them (collection writes);
- limit requests and connections per client: the server never sees client
  addresses.

`docs/public-instance.md` in the repository has the nginx setup of the public
instance.

## Values

| Value | Default | Meaning |
|---|---|---|
| `image.repository` | `ghcr.io/magnetite-browser/magnetite-sync` | |
| `image.tag` | the chart's `appVersion` | |
| `image.pullPolicy` | `IfNotPresent` | |
| `replicaCount` | `2` | server pods; they share the database and every limit kept in it |
| `registration.value` | | `invite:<code>` with a code of 16 characters or more, `open` or `closed` |
| `registration.existingSecret.name`, `.key` | `registration` (key) | a Secret that holds it instead |
| `database.url` | | `postgres://user:password@host:5432/database` |
| `database.existingSecret.name`, `.key` | `url` (key) | a Secret that holds it instead |
| `config.retentionDays` | the server's, 400 | delete chains unused this long; `0` keeps them |
| `config.maxChains` | the server's, 1000 | chains the server holds at most; the database needs room for chains × 32 MiB × 3 |
| `config.maxNewChainsPerDay` | the server's, 100 | chains started per day; `0` = unlimited |
| `config.logLevel` | `info` | `RUST_LOG` |
| `resources` | 1 GiB memory limit; requests 50m CPU, 64 MiB | |
| `service.type`, `.port` | `ClusterIP`, `8787` | |
| `ingress.enabled`, `.className`, `.annotations`, `.host`, `.tlsSecretName` | off | `tlsSecretName` empty when TLS ends in front of the ingress |
| `networkPolicy.enabled` | `true` | in: the server's port; out: DNS and `networkPolicy.databasePort` (5432) only |
| `networkPolicy.from`, `.databaseTo` | any | NetworkPolicy peers allowed to reach the server, and where the database is |
| `preStopSleepSeconds` | `5` | pause before a stopping pod stops taking requests (Kubernetes 1.30+); `0` off |
| `podSecurityContext`, `securityContext` | user 10001, read-only root, no capabilities, `RuntimeDefault` seccomp | |
| `nodeSelector`, `tolerations`, `affinity` | none | |

## Running it

- Upgrades replace the pods one after another; a new version migrates the
  database when it starts.
- A pod is ready while the database answers (`/v2/ready`) and alive while
  the server process answers (`/v2/health`): a lost database takes pods out
  of the service without restarting them.
- Each pod opens up to 21 database connections (Postgres allows 100 by
  default).
- A stopping pod finishes its transfers (up to 120 s) and pauses
  `preStopSleepSeconds` first, so rollouts do not cut uploads.
- Back up the database (`pg_dump`, or your operator's backups); the pods keep
  nothing.
- `helm uninstall` leaves the database alone.
