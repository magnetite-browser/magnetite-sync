#!/bin/sh
# Install the Helm chart with a built image in a throwaway kind cluster, on a
# throwaway Postgres (compose.yml's image): lint it, check other value
# combinations against the API server, run two server pods (with the network
# policy), ask /v2/health through the service, and uninstall. It uses a
# kubeconfig of its own, so it never touches another cluster.
#
# Usage: tools/chart-test.sh <image>   (needs docker, kind, kubectl, helm, curl)
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <image>" >&2
  exit 2
fi
cd "$(dirname "$0")/.."

image=$1
chart=charts/magnetite-sync
cluster=magnetite-sync-chart-test
postgres=$(sed -n 's/^ *image: *\(docker\.io\/library\/postgres:.*\)$/\1/p' compose.yml)
random() { od -An -N16 -tx1 /dev/urandom | tr -d ' \n'; }
password=$(random)
work=$(mktemp -d)
KUBECONFIG="$work/kubeconfig"
export KUBECONFIG
forward=""
# shellcheck disable=SC2317 # run by the trap
cleanup() {
  if [ -n "$forward" ]; then kill "$forward" 2>/dev/null || true; fi
  kind delete cluster --name "$cluster" --kubeconfig "$KUBECONFIG" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

helm lint --strict "$chart" --set registration.value=closed --set database.url=postgres://db

kind create cluster --name "$cluster" --kubeconfig "$KUBECONFIG" --wait 120s
kind load docker-image "$image" --name "$cluster"

# Other value combinations, checked by the API server without creating anything.
helm template sync "$chart" \
  --set registration.existingSecret.name=registration \
  --set database.existingSecret.name=database --set database.existingSecret.key=uri \
  --set ingress.enabled=true --set ingress.host=sync.example.org \
  --set ingress.tlsSecretName=sync-tls --set networkPolicy.enabled=false \
  --set config.maxNewChainsPerDay=0 --set config.retentionDays=30 |
  kubectl apply --dry-run=server -f - >/dev/null

kubectl create secret generic postgres --from-literal=password="$password" >/dev/null
kubectl apply -f - >/dev/null <<EOF
apiVersion: apps/v1
kind: Deployment
metadata: {name: postgres}
spec:
  selector: {matchLabels: {app: postgres}}
  template:
    metadata: {labels: {app: postgres}}
    spec:
      containers:
        - name: postgres
          image: $postgres
          env:
            - {name: POSTGRES_USER, value: magnetite_sync}
            - {name: POSTGRES_DB, value: magnetite_sync}
            - name: POSTGRES_PASSWORD
              valueFrom: {secretKeyRef: {name: postgres, key: password}}
          ports: [{containerPort: 5432}]
          readinessProbe:
            exec: {command: [pg_isready, --host=127.0.0.1, --username=magnetite_sync]}
            periodSeconds: 2
---
apiVersion: v1
kind: Service
metadata: {name: postgres}
spec:
  selector: {app: postgres}
  ports: [{port: 5432}]
EOF
kubectl rollout status deployment/postgres --timeout=180s

if ! helm install sync "$chart" --wait --timeout 180s \
  --set image.repository="${image%:*}" --set image.tag="${image##*:}" \
  --set image.pullPolicy=Never --set registration.value="invite:$(random)" \
  --set database.url="postgres://magnetite_sync:$password@postgres:5432/magnetite_sync"; then
  kubectl get pods -o wide >&2
  kubectl describe pods -l app.kubernetes.io/instance=sync >&2
  kubectl logs -l app.kubernetes.io/instance=sync --tail=50 >&2
  exit 1
fi
ready=$(kubectl get deployment sync-magnetite-sync -o jsonpath='{.status.readyReplicas}')
[ "$ready" = 2 ] || { echo "$ready of 2 server pods are ready" >&2; exit 1; }

kubectl port-forward service/sync-magnetite-sync 18787:8787 >/dev/null &
forward=$!
for _ in $(seq 1 30); do
  if [ "$(curl --silent --max-time 2 http://127.0.0.1:18787/v2/health)" = "ok" ]; then
    echo "chart: two pods ready, health: ok"
    helm uninstall sync --wait >/dev/null
    exit 0
  fi
  sleep 1
done
echo "the service did not answer /v2/health within 30 seconds" >&2
exit 1
