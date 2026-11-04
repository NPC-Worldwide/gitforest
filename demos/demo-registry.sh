#!/bin/bash
set -u
export no_proxy='*' NO_PROXY='*'
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/gitforest"
TMP="$(mktemp -d /tmp/gf-registry-XXXXXX)"
CLUSTER="registry-proof"
IMAGE="127.0.0.1:5000/gf-test:1"
REG_USER="demo"
REG_PASS="demopass"

REG_API=17906; REG_P2P=19476

cleanup() {
  for pid in $(pgrep -f -- "gitforest --config $TMP/" 2>/dev/null | grep -v "^$$\$" || true); do
    kill "$pid" 2>/dev/null || true
  done
  docker rm -f gf-reg-demo >/dev/null 2>&1 || true
  sleep 1
  rm -rf "$TMP"
}
trap cleanup EXIT

fail() {
  echo "REGISTRY FAIL: $1"
  echo "--- daemon log tail ---"
  tail -n 15 "$TMP/node.log" 2>/dev/null
  echo "--- registry log tail ---"
  docker logs gf-reg-demo 2>&1 | tail -n 8 || true
  exit 1
}

echo "== gitforest private-registry credentials proof =="
(cd "$ROOT" && cargo build --release --offline -p gt-daemon) || fail "build failed"
command -v docker >/dev/null || fail "docker missing"

docker pull registry:2 >/dev/null 2>&1 || fail "registry image pull failed"
docker run --rm httpd:2 htpasswd -Bbn "$REG_USER" "$REG_PASS" > "$TMP/htpasswd" 2>/dev/null || fail "htpasswd failed"
docker rm -f gf-reg-demo >/dev/null 2>&1 || true
docker run -d --name gf-reg-demo -p 127.0.0.1:5000:5000 -v "$TMP/htpasswd:/auth/htpasswd:ro" \
  -e REGISTRY_AUTH=htpasswd -e REGISTRY_AUTH_HTPASSWD_REALM=gf-demo \
  -e REGISTRY_AUTH_HTPASSWD_PATH=/auth/htpasswd registry:2 >/dev/null || fail "registry start failed"
for _ in $(seq 1 20); do
  curl -s --noproxy '*' --max-time 2 "http://127.0.0.1:5000/v2/" >/dev/null 2>&1 && break
  sleep 1
done
echo "$REG_PASS" | docker login 127.0.0.1:5000 -u "$REG_USER" --password-stdin >/dev/null 2>&1 || fail "docker login failed"
docker tag alpine:3 "$IMAGE" || fail "tag failed"
docker push "$IMAGE" >/dev/null 2>&1 || fail "push failed"
docker rmi -f "$IMAGE" >/dev/null 2>&1 || true
echo "private registry up with $IMAGE (local copy removed)"

mkdir -p "$TMP/home" "$TMP/data" "$TMP/repo/.gitforest/workflows"
cat > "$TMP/repo/.gitforest/workflows/registry.yml" <<EOF
name: RegistryCreds
on:
  workflow_dispatch:
jobs:
  pull:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 512
    disk-mb: 1024
    container:
      image: $IMAGE
      credentials:
        username: $REG_USER
        password: $REG_PASS
    steps:
      - run: echo registry-creds-ok > reg.txt
    artifacts:
      - reg.txt
EOF
cat > "$TMP/repo/.gitforest/workflows/registry-bad.yml" <<EOF
name: RegistryBadCreds
on:
  workflow_dispatch:
jobs:
  pull:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 512
    disk-mb: 1024
    container:
      image: $IMAGE
      credentials:
        username: $REG_USER
        password: wrongpassword
    steps:
      - run: echo unreachable > reg.txt
    artifacts:
      - reg.txt
EOF
git -C "$TMP/repo" init -q --initial-branch=main 2>/dev/null
git -C "$TMP/repo" config user.email t@t; git -C "$TMP/repo" config user.name t
git -C "$TMP/repo" add .; git -C "$TMP/repo" commit -qm init
SHA="$(git -C "$TMP/repo" rev-parse HEAD)"

cat > "$TMP/node.toml" <<EOF
data_dir = "$TMP/data"
p2p_port = $REG_P2P
p2p_listen = "127.0.0.1"
api_port = $REG_API
advertise_addr = "/ip4/127.0.0.1/tcp/$REG_P2P/api/$REG_API"
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
bootstrap_peers = []
enable_mdns = false
cluster_token = "$CLUSTER"

[compute]
worker_enabled = true
max_cpu_cores = 2
max_ram_mb = 4096
max_disk_mb = 8192
docker_socket = "/var/run/docker.sock"
hourly_rate_sats = 0
accepted_chains = []
wallets = {}
max_concurrent_jobs = 2
allowed_images = []
execution_mode = "docker"
gpu_enabled = false
EOF

HOME="$TMP/home" "$BIN" init --name regproof >/dev/null 2>&1 || fail "init failed"
HOME="$TMP/home" setsid -f "$BIN" --config "$TMP/node.toml" daemon </dev/null >>"$TMP/node.log" 2>&1
ready=no
for _ in $(seq 1 30); do
  CODE="$(curl -s --noproxy '*' -o /dev/null -w "%{http_code}" "http://127.0.0.1:$REG_API/api/status" || true)"
  [ "$CODE" = "200" ] && { ready=yes; break; }
  sleep 1
done
[ "$ready" = "yes" ] || fail "daemon did not start"
TOK="$(cat "$TMP/home/.local/share/gitforest/api.token")"
[ -n "$TOK" ] || fail "missing api token"
BASE="http://127.0.0.1:$REG_API"
AUTH="Authorization: Bearer $TOK"
echo "docker daemon up"

curl -s --noproxy '*' -H "$AUTH" -H 'Content-Type: application/json' -X POST "$BASE/api/repos" -d '{"path":"'"$TMP/repo"'","name":"repro"}' | grep -q '"repro"' || fail "repo add failed"

GOOD_RUN="$(curl -s --noproxy '*' -H "$AUTH" -H 'Content-Type: application/json' -X POST "$BASE/api/repos/repro/workflows/dispatch" -d '{"workflow":"registry.yml","commit_sha":"'"$SHA"'"}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["runs"][0]["id"])')"
[ -n "$GOOD_RUN" ] || fail "good dispatch failed"
DONE=""
for _ in $(seq 1 45); do
  SUM="$(curl -s --noproxy '*' -H "$AUTH" "$BASE/api/workflows/runs/$GOOD_RUN" | python3 -c "import json,sys; s=json.load(sys.stdin)['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  [ "$SUM" = "1/1" ] && { DONE="yes"; break; }
  sleep 2
done
[ "$DONE" = "yes" ] || fail "credentialed run never completed (last: $SUM)"
ART="$(curl -s --noproxy '*' -H "$AUTH" "$BASE/api/compute/jobs/$(curl -s --noproxy '*' -H "$AUTH" "$BASE/api/workflows/runs/$GOOD_RUN" | python3 -c 'import json,sys; print(json.load(sys.stdin)["jobs"][0]["id"])')/artifacts/reg.txt")"
echo "$ART" | grep -q "registry-creds-ok" || fail "artifact mismatch: $ART"
echo "credentialed pull ran the private image: $ART"

BAD_RUN="$(curl -s --noproxy '*' -H "$AUTH" -H 'Content-Type: application/json' -X POST "$BASE/api/repos/repro/workflows/dispatch" -d '{"workflow":"registry-bad.yml","commit_sha":"'"$SHA"'"}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["runs"][0]["id"])')"
[ -n "$BAD_RUN" ] || fail "bad dispatch failed"
FAILED=""
for _ in $(seq 1 45); do
  SUM="$(curl -s --noproxy '*' -H "$AUTH" "$BASE/api/workflows/runs/$BAD_RUN" | python3 -c "import json,sys; s=json.load(sys.stdin)['summary']; print(str(s['failed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  [ "$SUM" = "1/1" ] && { FAILED="yes"; break; }
  sleep 2
done
[ "$FAILED" = "yes" ] || fail "bad-credentials run did not fail (last: $SUM)"
echo "wrong password failed the pull as expected"

echo "REGISTRY PASS"
