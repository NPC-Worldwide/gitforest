#!/bin/bash
set -u
export no_proxy='*' NO_PROXY='*'
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/gitforest"
API_PORT=17878
P2P_PORT=19418
TMP="$(mktemp -d /tmp/gf-demo-XXXXXX)"
export HOME="$TMP/home" XDG_CONFIG_HOME="$TMP/home/.config" XDG_DATA_HOME="$TMP/home/.local/share" XDG_RUNTIME_DIR="$TMP" GITFOREST_SKIP_AUTH=1
mkdir -p "$HOME"

cleanup() {
  if [ -f "$TMP/gitforest.pid" ]; then kill "$(cat "$TMP/gitforest.pid")" 2>/dev/null; fi
  pkill -f "gitforest --config $TMP/config.toml" 2>/dev/null
  sleep 1
  rm -rf "$TMP"
}
trap cleanup EXIT

fail() {
  echo "DEMO FAIL: $1"
  echo "--- daemon log tail ---"
  tail -n 30 "$TMP/daemon.log" 2>/dev/null
  exit 1
}

echo "== gitforest end-to-end demo =="
if [ ! -x "$BIN" ]; then
  echo "building release binary"
  (cd "$ROOT" && cargo build --release -p gt-daemon) || fail "build failed"
fi

MODE="local"
if docker info >/dev/null 2>&1; then MODE="docker"; fi
echo "execution mode: $MODE"

"$BIN" init --name demo >/dev/null 2>&1 || fail "init failed"
cat > "$TMP/config.toml" <<EOF
data_dir = "$TMP/data"
p2p_port = $P2P_PORT
api_port = $API_PORT
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
bootstrap_peers = []

[compute]
worker_enabled = true
max_cpu_cores = 4
max_ram_mb = 8192
max_disk_mb = 102400
docker_socket = "/var/run/docker.sock"
hourly_rate_sats = 1000
accepted_chains = ["lightning"]
max_concurrent_jobs = 2
allowed_images = []
max_job_timeout_secs = 600
allow_network_jobs = false
execution_mode = "$MODE"
wallets = {}
EOF

setsid -f "$BIN" --config "$TMP/config.toml" daemon </dev/null >>"$TMP/daemon.log" 2>&1
READY=""
for _ in $(seq 1 30); do
  CODE="$(curl -s --noproxy '*' -o /dev/null -w "%{http_code}" "http://127.0.0.1:$API_PORT/api/status" || true)"
  if [ "$CODE" = "200" ]; then READY="yes"; break; fi
  sleep 1
done
[ "$READY" = "yes" ] || fail "daemon did not start"

mkdir -p "$TMP/repo" && cd "$TMP/repo"
git init -q && git config user.email demo@gf && git config user.name demo
echo "demo-data" > input.txt && git add . && git commit -qm init
SHA="$(git rev-parse HEAD)"
cd "$ROOT"

export GF_API="http://127.0.0.1:$API_PORT" GF_REPO="$TMP/repo" GF_SHA="$SHA" GF_TMP="$TMP"
JOB_ID="$(python3 - <<'PYEOF'
import json, urllib.request, os
api = os.environ["GF_API"]
def post(path, body):
    req = urllib.request.Request(api + path, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"}, method="POST")
    return json.load(urllib.request.urlopen(req, timeout=30))
post("/api/repos", {"path": os.environ["GF_REPO"], "name": "demo"})
yaml_text = "name: demo\ndocker_image: alpine:3\ntimeout_secs: 120\nsteps:\n  - name: check\n    run: cat input.txt && echo done > result.txt\nartifacts:\n  - result.txt\n"
job = post("/api/compute/jobs", {"repo_name": "demo", "commit_sha": os.environ["GF_SHA"],
                                 "job_config_yaml": yaml_text, "max_payment_sats": 500})
print(job["id"])
PYEOF
)" || fail "submit failed"
[ -n "$JOB_ID" ] || fail "empty job id"
echo "job submitted: $JOB_ID"

export GF_JOB="$JOB_ID"
STATUS=""
for _ in $(seq 1 60); do
  STATUS="$(python3 -c "import json,urllib.request,os; print(json.load(urllib.request.urlopen(os.environ['GF_API'] + '/api/compute/jobs/' + os.environ['GF_JOB'], timeout=10))['status'])" || true)"
  if [ "$STATUS" = "completed" ] || [ "$STATUS" = "failed" ]; then break; fi
  sleep 2
done
[ "$STATUS" = "completed" ] || fail "job ended with status: $STATUS"
echo "job status: $STATUS"
python3 -c "import json,urllib.request,os; r = json.load(urllib.request.urlopen(os.environ['GF_API'] + '/api/compute/jobs/' + os.environ['GF_JOB'], timeout=10))['result']; print('exit_code:', r['exit_code'], 'time_ms:', r['execution_time_ms'], 'artifacts:', r['artifacts'])"

echo "hi-blob" > "$TMP/blob.bin"
BLOB="$(python3 -c "import json,urllib.request,os; print(json.load(urllib.request.urlopen(urllib.request.Request(os.environ['GF_API'] + '/api/blobs', data=json.dumps({'path': os.environ['GF_TMP'] + '/blob.bin'}).encode(), headers={'Content-Type': 'application/json'}, method='POST'), timeout=30))['blob_id'])" || true)"
[ -n "$BLOB" ] || fail "blob upload failed"
curl -s --noproxy '*' "http://127.0.0.1:$API_PORT/api/blobs/$BLOB/download" -o "$TMP/blob.out" || fail "blob download failed"
cmp -s "$TMP/blob.bin" "$TMP/blob.out" || fail "blob bytes differ"
echo "blob roundtrip ok: $BLOB"

echo "DEMO PASS"
