#!/bin/bash
set -u
export no_proxy='*' NO_PROXY='*'
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/gitforest"
TMP="$(mktemp -d /tmp/gf-verify-XXXXXX)"
CLUSTER="verify-live-9"

SUB_API=17881; SUB_P2P=19421
WA_API=17882; WA_P2P=19423
WB_API=17883; WB_P2P=19425

cleanup() {
  pkill -f "gitforest --config $TMP/" 2>/dev/null
  sleep 1
  rm -rf "$TMP"
}
trap cleanup EXIT

fail() {
  echo "VERIFY FAIL: $1"
  for n in sub wa wb; do
    echo "--- $n log tail ---"
    tail -n 15 "$TMP/$n.log" 2>/dev/null
  done
  exit 1
}

echo "== gitforest live two-worker replay proof =="
(cd "$ROOT" && cargo build --release -p gt-daemon) || fail "build failed"

MODE="local"
if docker info >/dev/null 2>&1; then MODE="docker"; fi
echo "execution mode: $MODE"

WA_ADDR="/ip4/127.0.0.1/tcp/$WA_P2P/api/$WA_API"
WB_ADDR="/ip4/127.0.0.1/tcp/$WB_P2P/api/$WB_API"
SUB_ADDR="/ip4/127.0.0.1/tcp/$SUB_P2P/api/$SUB_API"

mkconfig() {
  local name="$1" api="$2" p2p="$3" boots="$4" worker="$5"
  mkdir -p "$TMP/home-$name" "$TMP/$name"
  HOME="$TMP/home-$name" "$BIN" init --name "$name" >/dev/null 2>&1 || fail "init $name failed"
  cat > "$TMP/$name.toml" <<EOF
data_dir = "$TMP/$name/data"
p2p_port = $p2p
api_port = $api
cluster_token = "$CLUSTER"
advertise_addr = "/ip4/127.0.0.1/tcp/$p2p/api/$api"
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
bootstrap_peers = [$boots]
EOF
  if [ "$worker" = "yes" ]; then
    cat >> "$TMP/$name.toml" <<EOF

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
  fi
}

mkconfig sub "$SUB_API" "$SUB_P2P" "\"$WA_ADDR\", \"$WB_ADDR\"" no
mkconfig wa "$WA_API" "$WA_P2P" "\"$SUB_ADDR\"" yes
mkconfig wb "$WB_API" "$WB_P2P" "\"$SUB_ADDR\"" yes

for n in sub wa wb; do
  env HOME="$TMP/home-$n" setsid -f "$BIN" --config "$TMP/$n.toml" daemon </dev/null >>"$TMP/$n.log" 2>&1
done

for spec in "sub:$SUB_API" "wa:$WA_API" "wb:$WB_API"; do
  n="${spec%%:*}"; port="${spec##*:}"
  READY=""
  for _ in $(seq 1 40); do
    CODE="$(curl -s --noproxy '*' -o /dev/null -w "%{http_code}" "http://127.0.0.1:$port/api/status" || true)"
    if [ "$CODE" = "200" ]; then READY="yes"; break; fi
    sleep 1
  done
  [ "$READY" = "yes" ] || fail "daemon $n did not start"
done
echo "three daemons up (submitter + 2 workers), real cluster auth"

SUB_TOKEN="$(cat "$TMP/home-sub/.local/share/gitforest/api.token")"
[ -n "$SUB_TOKEN" ] || fail "no submitter api token"

echo "waiting for DHT offer discovery (2 offers)..."
OFFERS=""
for _ in $(seq 1 60); do
  OFFERS="$(curl -s --noproxy '*' -H "Authorization: Bearer $SUB_TOKEN" "http://127.0.0.1:$SUB_API/api/compute/offers" || true)"
  N="$(echo "$OFFERS" | python3 -c "import json,sys; print(len(json.load(sys.stdin)))" 2>/dev/null || echo 0)"
  if [ "$N" -ge 2 ] 2>/dev/null; then break; fi
  sleep 2
done
[ "$N" -ge 2 ] 2>/dev/null || fail "only $N offers discovered: $OFFERS"
echo "discovered $N worker offers via DHT"

mkdir -p "$TMP/repo" && cd "$TMP/repo"
git init -q && git config user.email verify@gf && git config user.name verify
echo "verify-data" > input.txt && git add . && git commit -qm init
SHA="$(git rev-parse HEAD)"
cd "$ROOT"

export GF_API="http://127.0.0.1:$SUB_API" GF_REPO="$TMP/repo" GF_SHA="$SHA" GF_TOKEN="$SUB_TOKEN"
JOB_ID="$(python3 - <<'PYEOF'
import json, urllib.request, os
api, token = os.environ["GF_API"], os.environ["GF_TOKEN"]
def post(path, body):
    req = urllib.request.Request(api + path, data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {token}"},
        method="POST")
    return json.load(urllib.request.urlopen(req, timeout=30))
post("/api/repos", {"path": os.environ["GF_REPO"], "name": "verify"})
yaml_text = "name: verify\ndocker_image: alpine:3\ntimeout_secs: 120\nsteps:\n  - name: check\n    run: cat input.txt && echo done > result.txt\nartifacts:\n  - result.txt\n"
job = post("/api/compute/jobs", {"repo_name": "verify", "commit_sha": os.environ["GF_SHA"],
                                 "job_config_yaml": yaml_text, "max_payment_sats": 500,
                                 "verify": True})
print(job["id"])
PYEOF
)" || fail "submit failed"
[ -n "$JOB_ID" ] || fail "empty job id"
echo "verify job submitted: $JOB_ID"

export GF_JOB="$JOB_ID"
STATUS=""
for _ in $(seq 1 120); do
  STATUS="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_API']+'/api/compute/jobs/'+os.environ['GF_JOB'],headers={'Authorization':'Bearer '+os.environ['GF_TOKEN']}); print(json.load(urllib.request.urlopen(q,timeout=10))['status'])" || true)"
  if [ "$STATUS" = "completed" ] || [ "$STATUS" = "failed" ]; then break; fi
  sleep 2
done
[ "$STATUS" = "completed" ] || fail "job ended with status: $STATUS"
echo "replay job status: $STATUS"
python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_API']+'/api/compute/jobs/'+os.environ['GF_JOB'],headers={'Authorization':'Bearer '+os.environ['GF_TOKEN']}); r=json.load(urllib.request.urlopen(q,timeout=10))['result']; print('exit_code:',r['exit_code'],'artifacts:',r['artifacts']); raise SystemExit(0 if r['exit_code']==0 else 1)" || fail "bad result"

PAID="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_API']+'/api/compute/trust',headers={'Authorization':'Bearer '+os.environ['GF_TOKEN']}); w=json.load(urllib.request.urlopen(q,timeout=10))['workers']; print(sum(1 for x in w if x['successful_jobs']>=1))")"
echo "workers with a recorded success: $PAID"
[ "$PAID" -ge 2 ] || fail "expected 2 paid workers, saw $PAID"

echo "VERIFY PASS"
