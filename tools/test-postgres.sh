#!/bin/sh
# Start a throwaway Postgres (compose.yml's image, data in memory) on
# 127.0.0.1:5432 for the tests, wait until it takes connections and print
# the DATABASE_URL they need:
#
#   export "$(tools/test-postgres.sh)" && cargo test --workspace
#
# Stop it with: docker rm --force magnetite-sync-test-postgres
set -eu
cd "$(dirname "$0")/.."

name=magnetite-sync-test-postgres
image=$(sed -n 's/^ *image: *\(docker\.io\/library\/postgres:.*\)$/\1/p' compose.yml)
docker run --detach --rm --name "$name" --publish 127.0.0.1:5432:5432 \
  --env POSTGRES_HOST_AUTH_METHOD=trust --tmpfs /var/lib/postgresql "$image" >/dev/null
for _ in $(seq 1 60); do
  if docker exec "$name" pg_isready --host=127.0.0.1 --username=postgres >/dev/null 2>&1; then
    echo "DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres"
    exit 0
  fi
  sleep 0.5
done
echo "Postgres did not start within 30 seconds" >&2
docker logs "$name" >&2
exit 1
