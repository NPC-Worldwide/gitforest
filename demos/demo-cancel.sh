#!/bin/bash
set -u
export no_proxy='*' NO_PROXY='*'
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/gitforest"
TMP="$(mktemp -d /tmp/gf-cancel-XXXXXX)"
CLUSTER="cancel-live-7"

SUB_API=17884; SUB_P2P=19426
WA_API=17885; WA_P2P=19428

cleanup() {
  if [ -n "${KEEP:-}" ]; then
    echo "KEEP set: daemons left running, workdir $TMP" >&2
    return
  fi
  for cfg in "$TMP/sub.toml" "$TMP/wa.toml"; do
    for pid in $(pgrep -f -- "gitforest --config $cfg" 2>/dev/null | grep -v "^$$\$" || true); do
      kill "$pid" 2>/dev/null || true
    done
  done
  sleep 1
  rm -rf "$TMP"
}
trap cleanup EXIT

fail() {
  echo "CANCEL FAIL: $1"
  for n in sub wa; do
    echo "--- $n log tail ---"
    tail -n 15 "$TMP/$n.log" 2>/dev/null
  done
  exit 1
}

echo "== gitforest live payer-protect cancel proof =="
(cd "$ROOT" && cargo build --release --offline -p gt-daemon) || fail "build failed"

SUB_ADDR="/ip4/127.0.0.1/tcp/$SUB_P2P/api/$SUB_API"
WA_ADDR="/ip4/127.0.0.1/tcp/$WA_P2P/api/$WA_API"

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
execution_mode = "local"
wallets = {}
EOF
  fi
}

mkconfig sub "$SUB_API" "$SUB_P2P" "\"$WA_ADDR\"" no
mkconfig wa "$WA_API" "$WA_P2P" "\"$SUB_ADDR\"" yes

for n in sub wa; do
  port_var="$(echo "$n" | tr 'a-z' 'A-Z')_API"
  env HOME="$TMP/home-$n" setsid -f "$BIN" --config "$TMP/$n.toml" daemon </dev/null >>"$TMP/$n.log" 2>&1
done

for spec in "sub:$SUB_API" "wa:$WA_API"; do
  n="${spec%%:*}"; port="${spec##*:}"
  READY=""
  for _ in $(seq 1 40); do
    CODE="$(curl -s --noproxy '*' -o /dev/null -w "%{http_code}" "http://127.0.0.1:$port/api/status" || true)"
    if [ "$CODE" = "200" ]; then READY="yes"; break; fi
    sleep 1
  done
  [ "$READY" = "yes" ] || fail "daemon $n did not start"
done
echo "two daemons up (submitter + worker), real cluster auth"

SUB_TOKEN="$(cat "$TMP/home-sub/.local/share/gitforest/api.token")"
WA_TOKEN="$(cat "$TMP/home-wa/.local/share/gitforest/api.token")"
[ -n "$SUB_TOKEN" ] && [ -n "$WA_TOKEN" ] || fail "missing api tokens"

mkdir -p "$TMP/probe/src"
cat > "$TMP/probe/Cargo.toml" <<EOF
[package]
name = "cancelprobe"
version = "0.0.0"
edition = "2021"

[dependencies]
gt-core = { path = "$ROOT/crates/gt-core" }
serde_json = "1"

[workspace]
EOF
cat > "$TMP/probe/src/main.rs" <<'EOF'
use gt_core::identity::Identity;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args[1].as_str() {
        "genkey" => {
            let id = Identity::generate();
            id.save(Path::new(&args[2])).unwrap();
            println!("{}", id.peer_id());
        }
        "submit" => {
            let id = Identity::load(Path::new(&args[2])).unwrap();
            let mut msg = gt_core::protocol::JobSubmissionMessage {
                job_id: args[3].clone(),
                submitter: id.peer_id(),
                repo_owner: args[4].parse().unwrap(),
                repo_name: args[5].clone(),
                commit_sha: args[6].clone(),
                job_config_json: args[7].clone(),
                min_cpu_cores: 1,
                min_ram_mb: 128,
                min_disk_mb: 512,
                gpu_required: false,
                network_access: false,
                max_payment_sats: 500,
                preferred_chain: "lightning".into(),
                signature: vec![],
            };
            let payload = msg.signable_payload();
            msg.signature = id.sign(&payload);
            println!("{}", serde_json::to_string(&msg).unwrap());
        }
        "cancel" => {
            let id = Identity::load(Path::new(&args[2])).unwrap();
            let mut msg = gt_core::protocol::JobCancelMessage {
                job_id: args[3].clone(),
                submitter: id.peer_id(),
                reason: args[4].clone(),
                signature: vec![],
            };
            let payload = msg.signable_payload();
            msg.signature = id.sign(&payload);
            println!("{}", serde_json::to_string(&msg).unwrap());
        }
        _ => std::process::exit(2),
    }
}
EOF
(cd "$TMP/probe" && cargo build --release --offline -q) || fail "probe build failed"
PROBE="$TMP/probe/target/release/cancelprobe"

mkdir -p "$TMP/repo" && cd "$TMP/repo"
git init -q && git config user.email cancel@gf && git config user.name cancel
echo "cancel-data" > input.txt && git add . && git commit -qm init
SHA="$(git rev-parse HEAD)"
cd "$ROOT"

curl -s --noproxy '*' -H "Authorization: Bearer $SUB_TOKEN" -H "Content-Type: application/json" \
  -d "{\"path\": \"$TMP/repo\", \"name\": \"cancelrepo\"}" \
  "http://127.0.0.1:$SUB_API/api/repos" >/dev/null || fail "repo register failed"

SUB_PEER="$(python3 -c "import json,urllib.request; q=urllib.request.Request('http://127.0.0.1:$SUB_API/api/id'); print(json.load(urllib.request.urlopen(q,timeout=10))['peer_id'])")"
[ -n "$SUB_PEER" ] || fail "no submitter peer id"
echo "submitter peer: ${SUB_PEER:0:16}..."

JOB_ID="cancel-job-1"
CONFIG_JSON='{"name":"ci","steps":[{"name":"t","run":"echo hi"}],"docker_image":"alpine:3","timeout_secs":60}'
SUBMIT_MSG="$("$PROBE" submit "$TMP/home-sub/.config/gitforest/identity.key" "$JOB_ID" "$SUB_PEER" "cancelrepo" "$SHA" "$CONFIG_JSON")"

echo "waiting for worker to accept remote submit..."
ACCEPTED=""
for _ in $(seq 1 60); do
  RESP="$(curl -s --noproxy '*' -H "Authorization: Bearer $CLUSTER" -H "Content-Type: application/json" \
    -d "$SUBMIT_MSG" "http://127.0.0.1:$WA_API/api/compute/remote/submit" || true)"
  ACCEPTED="$(echo "$RESP" | python3 -c "import json,sys; print(json.load(sys.stdin).get('accepted', False))" 2>/dev/null || echo False)"
  if [ "$ACCEPTED" = "True" ]; then break; fi
  sleep 2
done
[ "$ACCEPTED" = "True" ] || fail "worker never accepted: $RESP"
echo "worker accepted job $JOB_ID"

HAVE="$(curl -s --noproxy '*' -H "Authorization: Bearer $WA_TOKEN" "http://127.0.0.1:$WA_API/api/compute/jobs" | grep -c "$JOB_ID" || true)"
[ "$HAVE" -ge 1 ] || fail "job missing on worker before cancel"

"$PROBE" genkey "$TMP/evil.key" >/dev/null || fail "evil keygen failed"
EVIL_MSG="$("$PROBE" cancel "$TMP/evil.key" "$JOB_ID" "stale match")"
EVIL_RESP="$(curl -s --noproxy '*' -H "Authorization: Bearer $CLUSTER" -H "Content-Type: application/json" \
  -d "$EVIL_MSG" "http://127.0.0.1:$WA_API/api/compute/remote/cancel")"
echo "$EVIL_RESP" | grep -q '"cancelled":false' || fail "impostor cancel not rejected: $EVIL_RESP"
echo "impostor cancel rejected"

REAL_MSG="$("$PROBE" cancel "$TMP/home-sub/.config/gitforest/identity.key" "$JOB_ID" "stale match")"
CANCEL_RESP="$(curl -s --noproxy '*' -H "Authorization: Bearer $CLUSTER" -H "Content-Type: application/json" \
  -d "$REAL_MSG" "http://127.0.0.1:$WA_API/api/compute/remote/cancel")"
echo "$CANCEL_RESP" | grep -q '"cancelled":true' || fail "real cancel failed: $CANCEL_RESP"
echo "submitter cancel accepted: $CANCEL_RESP"

HAVE="$(curl -s --noproxy '*' -H "Authorization: Bearer $WA_TOKEN" "http://127.0.0.1:$WA_API/api/compute/jobs" | grep -c "$JOB_ID" || true)"
[ "$HAVE" -eq 0 ] || fail "job still on worker after cancel"

AGAIN="$(curl -s --noproxy '*' -o /dev/null -w "%{http_code}" -H "Authorization: Bearer $CLUSTER" -H "Content-Type: application/json" \
  -d "$REAL_MSG" "http://127.0.0.1:$WA_API/api/compute/remote/cancel")"
[ "$AGAIN" = "404" ] || fail "repeat cancel gave $AGAIN, want 404"

echo "proving local submitter-initiated cancel..."
export SUB_API SUB_TOKEN_FILE="$TMP/home-sub/.local/share/gitforest/api.token" SHA
LOCAL_ID="$(python3 - <<'PYEOF'
import json, urllib.request, os
api = "http://127.0.0.1:%s" % os.environ["SUB_API"]
token = open(os.environ["SUB_TOKEN_FILE"]).read().strip()
def post(path, body):
    req = urllib.request.Request(api + path, data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {token}"},
        method="POST")
    try:
        return json.load(urllib.request.urlopen(req, timeout=30))
    except urllib.error.HTTPError as e:
        print("POST", path, "->", e.code, e.read().decode()[:300])
        raise
yaml_text = "name: local-cancel\ndocker_image: alpine:3\ntimeout_secs: 60\nsteps:\n  - name: t\n    run: echo hi\n"
job = post("/api/compute/jobs", {"repo_name": "cancelrepo", "commit_sha": os.environ["SHA"],
                                 "job_config_yaml": yaml_text, "max_payment_sats": 100,
                                 "preferred_chain": "lightning"})
print(job["id"])
PYEOF
)" || fail "local submit failed"
[ -n "$LOCAL_ID" ] || fail "empty local job id"
LOCAL_CANCEL="$(curl -s --noproxy '*' -X POST -H "Authorization: Bearer $SUB_TOKEN" \
  "http://127.0.0.1:$SUB_API/api/compute/jobs/$LOCAL_ID/cancel")"
echo "$LOCAL_CANCEL" | grep -q '"cancelled":true' || fail "local cancel failed: $LOCAL_CANCEL"
LOCAL_STATUS="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request('http://127.0.0.1:$SUB_API/api/compute/jobs/$LOCAL_ID'); print(json.load(urllib.request.urlopen(q,timeout=10))['status'])")"
[ "$LOCAL_STATUS" = "cancelled" ] || fail "local status is $LOCAL_STATUS, want cancelled"
echo "local cancel accepted, status: $LOCAL_STATUS"

echo "CANCEL PASS"
