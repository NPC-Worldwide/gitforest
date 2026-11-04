#!/bin/bash
set -u
export no_proxy='*' NO_PROXY='*'
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/gitforest"
TMP="$(mktemp -d /tmp/gf-workflows-XXXXXX)"

DEV_API=17895; DEV_P2P=19439
PEER_API=17896; PEER_P2P=19441
SVC_API=17897; SVC_P2P=19443

cleanup() {
  pkill -f "gitforest --config $TMP/" 2>/dev/null
  sleep 1
  rm -rf "$TMP"
}
trap cleanup EXIT

fail() {
  echo "WORKFLOWS-MODELS FAIL: $1"
  for n in dev peer svc; do
    echo "--- $n log tail ---"
    tail -n 12 "$TMP/$n.log" 2>/dev/null
  done
  exit 1
}

echo "== gitforest live workflows + model sharing proof =="
(cd "$ROOT" && cargo build --release -p gt-daemon) || fail "build failed"

DEV_ADDR="/ip4/127.0.0.1/tcp/$DEV_P2P/api/$DEV_API"
PEER_ADDR="/ip4/127.0.0.1/tcp/$PEER_P2P/api/$PEER_API"

mkconfig() {
  local name="$1" api="$2" p2p="$3" boots="$4" llm="$5"
  mkdir -p "$TMP/home-$name" "$TMP/$name"
  HOME="$TMP/home-$name" "$BIN" init --name "$name" >/dev/null 2>&1 || fail "init $name failed"
  cat > "$TMP/$name.toml" <<EOF
data_dir = "$TMP/$name/data"
p2p_port = $p2p
p2p_listen = "127.0.0.1"
api_port = $api
advertise_addr = "/ip4/127.0.0.1/tcp/$p2p/api/$api"
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
bootstrap_peers = [$boots]
enable_mdns = false
cluster_token = "demo-cluster-token"
$(if [ -n "$llm" ]; then echo "llm_backend = \"$llm\""; fi)

[compute]
worker_enabled = true
max_cpu_cores = 1
max_ram_mb = 2048
max_disk_mb = 2048
docker_socket = "/var/run/docker.sock"
hourly_rate_sats = 0
accepted_chains = []
wallets = {}
max_concurrent_jobs = 2
allowed_images = []
execution_mode = "local"
gpu_enabled = true
EOF
}

cat > "$TMP/stub-llama.sh" <<'EOF'
#!/bin/sh
echo "demo completion from stub backend"
EOF
chmod +x "$TMP/stub-llama.sh"

mkdir -p "$TMP/fakebin"
cat > "$TMP/fakebin/nvidia-smi" <<'EOF'
#!/bin/sh
echo "GPU 0: Fake Tesla T4 (UUID: GPU-fake-000)"
EOF
chmod +x "$TMP/fakebin/nvidia-smi"

mkconfig dev "$DEV_API" "$DEV_P2P" "\"$PEER_ADDR\"" "$TMP/stub-llama.sh"
mkconfig peer "$PEER_API" "$PEER_P2P" "\"$DEV_ADDR\"" "$TMP/stub-llama.sh"
SVC_ADDR="/ip4/127.0.0.1/tcp/$SVC_P2P/api/$SVC_API"
mkconfig svc "$SVC_API" "$SVC_P2P" "\"$DEV_ADDR\"" "$TMP/stub-llama.sh"
printf 'protected_environments = ["prod"]\n' >> "$TMP/dev.toml"
sed -i 's/^worker_enabled = true$/worker_enabled = false/' "$TMP/peer.toml"
sed -i 's/^gpu_enabled = true$/gpu_enabled = false/' "$TMP/peer.toml"
sed -i 's/^execution_mode = "local"$/execution_mode = "docker"/' "$TMP/svc.toml"
printf 'allow_network_jobs = true\n' >> "$TMP/svc.toml"

for n in dev peer svc; do
  extra=""
  [ "$n" = "peer" ] && extra="GF_SECRETS_PASSPHRASE=peer-demo-passphrase PATH=$TMP/fakebin:$PATH"
  env HOME="$TMP/home-$n" XDG_RUNTIME_DIR="$TMP" $extra setsid -f "$BIN" --config "$TMP/$n.toml" daemon </dev/null >>"$TMP/$n.log" 2>&1
done

for spec in "dev:$DEV_API" "peer:$PEER_API" "svc:$SVC_API"; do
  n="${spec%%:*}"; port="${spec##*:}"
  READY=""
  for _ in $(seq 1 40); do
    CODE="$(curl -s --noproxy '*' -o /dev/null -w "%{http_code}" "http://127.0.0.1:$port/api/status" || true)"
    if [ "$CODE" = "200" ]; then READY="yes"; break; fi
    sleep 1
  done
  [ "$READY" = "yes" ] || fail "daemon $n did not start"
done
echo "three daemons up with real local auth (dev+peer local, svc docker)"

DEV_TOKEN="$(cat "$TMP/home-dev/.local/share/gitforest/api.token")"
PEER_TOKEN="$(cat "$TMP/home-peer/.local/share/gitforest/api.token")"
SVC_TOKEN="$(cat "$TMP/home-svc/.local/share/gitforest/api.token")"
[ -n "$DEV_TOKEN" ] && [ -n "$PEER_TOKEN" ] && [ -n "$SVC_TOKEN" ] || fail "missing api tokens"

export GF_DEV="http://127.0.0.1:$DEV_API" GF_PEER="http://127.0.0.1:$PEER_API" GF_SVC="http://127.0.0.1:$SVC_API"
export GF_DEV_TOKEN="$DEV_TOKEN" GF_PEER_TOKEN="$PEER_TOKEN" GF_SVC_TOKEN="$SVC_TOKEN"

GPU_ADVERTISED="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/compute/worker',headers={'Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']}); print(json.load(urllib.request.urlopen(q,timeout=10))['has_gpu'])")" || fail "worker status failed"
[ "$GPU_ADVERTISED" = "True" ] || fail "dev worker does not advertise a GPU"
echo "dev worker advertises has_gpu"
PEER_GPU="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_PEER']+'/api/compute/worker',headers={'Authorization':'Bearer '+os.environ['GF_PEER_TOKEN']}); print(json.load(urllib.request.urlopen(q,timeout=10))['has_gpu'])")" || fail "peer worker status failed"
[ "$PEER_GPU" = "True" ] || fail "peer worker did not auto-detect its GPU"
grep -q "Detected 1 GPU" "$TMP/peer.log" || fail "peer log lacks the GPU probe line"
echo "peer worker auto-detected its GPU via nvidia-smi"

git init -q --initial-branch=main "$TMP/repo"
git -C "$TMP/repo" config user.email "demo@test.com"
git -C "$TMP/repo" config user.name "Demo"
mkdir -p "$TMP/repo/.github/workflows"
cat > "$TMP/repo/.github/workflows/ci.yml" <<'EOF'
name: CI
on:
  push:
    branches: [main]
  pull_request:
  workflow_dispatch:
jobs:
  build:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 512
    disk-mb: 1024
    steps:
      - name: Build
        run: echo building && echo build-output-123 > build.txt
    artifacts:
      - build.txt
  test:
    needs: build
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 512
    disk-mb: 1024
    gpu: true
    steps:
      - name: Test
        run: echo testing
EOF
cat > "$TMP/repo/.github/workflows/release.yml" <<'EOF'
name: Release
on:
  push:
    tags: ["v*"]
jobs:
  package:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - run: echo release packaging ok
EOF
cat > "$TMP/repo/.github/workflows/inputs.yml" <<'EOF'
name: Inputs
on:
  workflow_dispatch:
    inputs:
      name:
        required: true
      greeting:
        default: hello
jobs:
  greet:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - run: echo "${{ inputs.greeting }}-${{ inputs.name }}" > greet.txt
    artifacts:
      - greet.txt
EOF
echo "# demo" > "$TMP/repo/README.md"
git -C "$TMP/repo" add . && git -C "$TMP/repo" commit -qm "initial"
git -C "$TMP/repo" tag v0.0.1-demo
SHA="$(git -C "$TMP/repo" rev-parse HEAD)"
export GF_REPO="$TMP/repo" GF_SHA="$SHA"

python3 - <<'PYEOF' || fail "repo add failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/repos",
    data=json.dumps({"path": os.environ["GF_REPO"], "name": "demo"}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
print(json.load(urllib.request.urlopen(req, timeout=30)))
PYEOF

WF="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/repos/demo/workflows',headers={'Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']}); print(len(json.load(urllib.request.urlopen(q,timeout=10))['workflows']))")" || fail "workflow list failed"
[ "$WF" = "3" ] || fail "expected 3 workflows, saw $WF"
echo "workflows discovered: CI + Release + Inputs"

python3 - <<'PYEOF' || fail "secret set failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/repos/demo/secrets",
    data=json.dumps({"key": "DEPLOY_TOKEN", "value": "live-secret-plaintext-probe"}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
d = json.load(urllib.request.urlopen(req, timeout=30))
assert d["status"] == "stored", d
q = urllib.request.Request(os.environ["GF_DEV"] + "/api/repos/demo/secrets",
    headers={"Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]})
names = json.load(urllib.request.urlopen(q, timeout=30))["names"]
assert names == ["DEPLOY_TOKEN"], names
print("secret stored, names:", names)
PYEOF
SECRETS_FILE="$TMP/dev/data/secrets/demo.json"
[ -f "$SECRETS_FILE" ] || fail "missing secrets file"
grep -q "live-secret-plaintext-probe" "$SECRETS_FILE" && fail "secret value present in plaintext at rest"
grep -q "xchacha20poly1305" "$SECRETS_FILE" || fail "secrets file is not an encrypted envelope"
grep -q '"v": 1' "$SECRETS_FILE" || fail "dev secrets file is not a v1 envelope"
[ "$(stat -c %a "$SECRETS_FILE")" = "600" ] || fail "secrets file is not owner-only"
echo "repo secret encrypted at rest (no plaintext, 0600 v1 envelope)"

python3 - <<'PYEOF' || fail "peer secret set failed"
import json, urllib.request, os
reg = urllib.request.Request(os.environ["GF_PEER"] + "/api/repos",
    data=json.dumps({"path": os.environ["GF_REPO"], "name": "demo-peer"}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_PEER_TOKEN"]},
    method="POST")
json.load(urllib.request.urlopen(reg, timeout=30))
req = urllib.request.Request(os.environ["GF_PEER"] + "/api/repos/demo-peer/secrets",
    data=json.dumps({"key": "PEER_TOKEN", "value": "peer-secret-plaintext-probe"}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_PEER_TOKEN"]},
    method="POST")
d = json.load(urllib.request.urlopen(req, timeout=30))
assert d["status"] == "stored", d
q = urllib.request.Request(os.environ["GF_PEER"] + "/api/repos/demo-peer/secrets",
    headers={"Authorization": "Bearer " + os.environ["GF_PEER_TOKEN"]})
names = json.load(urllib.request.urlopen(q, timeout=30))["names"]
assert names == ["PEER_TOKEN"], names
print("peer secret stored under passphrase, names:", names)
PYEOF
PEER_SECRETS_FILE="$TMP/peer/data/secrets/demo-peer.json"
[ -f "$PEER_SECRETS_FILE" ] || fail "missing peer secrets file"
grep -q "peer-secret-plaintext-probe" "$PEER_SECRETS_FILE" && fail "peer secret value present in plaintext at rest"
grep -q '"v": 2' "$PEER_SECRETS_FILE" || fail "peer secrets file is not a v2 envelope"
grep -q "pbkdf2-sha256" "$PEER_SECRETS_FILE" || fail "peer secrets file lacks KDF marker"
echo "passphrase-sealed peer secret at rest (v2 envelope)"

RUN_ID="$(python3 - <<'PYEOF' || fail "dispatch failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/repos/demo/workflows/dispatch",
    data=json.dumps({"branch": "main", "commit_sha": os.environ["GF_SHA"]}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1, runs
assert runs[0]["workflow"] == "CI", runs
assert len(runs[0]["job_ids"]) == 2, runs
print(runs[0]["id"])
PYEOF
)"
[ -n "$RUN_ID" ] || fail "empty run id"
echo "branch push triggered CI only (tags-only Release stayed quiet)"

export GF_RUN="$RUN_ID"
DONE=""
for _ in $(seq 1 60); do
  SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/workflows/runs/'+os.environ['GF_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  if [ "$SUM" = "2/2" ]; then DONE="yes"; break; fi
  sleep 2
done
[ "$DONE" = "yes" ] || fail "run never completed (last summary: $SUM)"
echo "DAG executed in order: build then gpu-required test, both completed"
python3 - <<'PYEOF' || fail "artifact download failed"
import urllib.request, os
base = os.environ["GF_DEV"]
tok = {"Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]}
import json
q = urllib.request.Request(base + "/api/workflows/runs/" + os.environ["GF_RUN"], headers=tok)
jobs = json.load(urllib.request.urlopen(q, timeout=10))["jobs"]
build = [j for j in jobs if j["name"].endswith("build")]
assert len(build) == 1, jobs
dl = urllib.request.Request(base + "/api/compute/jobs/" + build[0]["id"] + "/artifacts/build.txt")
body = urllib.request.urlopen(dl, timeout=30).read()
assert body == b"build-output-123\n", body
print("artifact bytes round-tripped:", body.decode().strip())
PYEOF
echo "build artifact downloaded byte-identical"

python3 - <<'PYEOF' || fail "merge request failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/repos/demo/merge-requests",
    data=json.dumps({"source_repo": "fork", "source_branch": "feature",
                     "target_branch": "main", "title": "t", "description": "d"}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
print(json.load(urllib.request.urlopen(req, timeout=30)))
PYEOF
PR_RUNS="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/workflows/runs',headers={'Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']}); print(sum(1 for r in json.load(urllib.request.urlopen(q,timeout=10)) if r['event']=='pull_request'))")" || fail "runs list failed"
[ "$PR_RUNS" = "1" ] || fail "expected 1 pull_request run, saw $PR_RUNS"
echo "merge request auto-triggered a pull_request run"

TAG_RUN="$(python3 - <<'PYEOF' || fail "tag dispatch failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/repos/demo/workflows/dispatch",
    data=json.dumps({"tag": "v0.0.1-demo", "commit_sha": os.environ["GF_SHA"]}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1, runs
assert runs[0]["event"] == "tag", runs
assert runs[0]["workflow"] == "Release", runs
assert len(runs[0]["job_ids"]) == 1, runs
print(runs[0]["id"])
PYEOF
)"
[ -n "$TAG_RUN" ] || fail "empty tag run id"
export GF_TAG_RUN="$TAG_RUN"
TAG_DONE=""
for _ in $(seq 1 45); do
  TAG_SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/workflows/runs/'+os.environ['GF_TAG_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  if [ "$TAG_SUM" = "1/1" ]; then TAG_DONE="yes"; break; fi
  sleep 2
done
[ "$TAG_DONE" = "yes" ] || fail "tag run never completed (last summary: $TAG_SUM)"
echo "tag push triggered the Release workflow to completion"

IN_RUN="$(python3 - <<'PYEOF' || fail "inputs dispatch failed"
import json, urllib.request, os
base = os.environ["GF_DEV"]
tok = {"Content-Type": "application/json",
       "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]}
def dispatch(payload):
    req = urllib.request.Request(base + "/api/repos/demo/workflows/dispatch",
        data=json.dumps(payload).encode(), headers=tok, method="POST")
    return json.load(urllib.request.urlopen(req, timeout=30))["runs"]
runs = dispatch({"workflow": "inputs.yml", "commit_sha": os.environ["GF_SHA"], "inputs": {}})
assert runs == [], runs
runs = dispatch({"workflow": "inputs.yml", "commit_sha": os.environ["GF_SHA"],
                 "inputs": {"name": "world"}})
assert len(runs) == 1, runs
assert runs[0]["event"] == "workflow_dispatch", runs
assert len(runs[0]["job_ids"]) == 1, runs
print(runs[0]["id"])
PYEOF
)"
[ -n "$IN_RUN" ] || fail "empty inputs run id"
echo "missing required input produced no runs"
export GF_IN_RUN="$IN_RUN"
IN_DONE=""
for _ in $(seq 1 45); do
  IN_SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/workflows/runs/'+os.environ['GF_IN_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  if [ "$IN_SUM" = "1/1" ]; then IN_DONE="yes"; break; fi
  sleep 2
done
[ "$IN_DONE" = "yes" ] || fail "inputs run never completed (last summary: $IN_SUM)"
python3 - <<'PYEOF' || fail "inputs artifact mismatch"
import urllib.request, os, json
base = os.environ["GF_DEV"]
tok = {"Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]}
q = urllib.request.Request(base + "/api/workflows/runs/" + os.environ["GF_IN_RUN"], headers=tok)
jobs = json.load(urllib.request.urlopen(q, timeout=10))["jobs"]
assert len(jobs) == 1, jobs
dl = urllib.request.Request(base + "/api/compute/jobs/" + jobs[0]["id"] + "/artifacts/greet.txt")
body = urllib.request.urlopen(dl, timeout=30).read()
assert body == b"hello-world\n", body
print("inputs artifact bytes:", body.decode().strip())
PYEOF
echo "dispatch inputs substituted with defaults into the run"
HOME="$TMP/home-dev" "$BIN" --config "$TMP/dev.toml" workflow dispatch demo --workflow inputs.yml --input name=cli 2>&1 | grep -q "Inputs" || fail "cli dispatch --input failed"
echo "cli dispatch --input works"
HOME="$TMP/home-dev" "$BIN" --config "$TMP/dev.toml" workflow dispatch demo --workflow release.yml --tag v9.9.9-cli 2>&1 | grep -q "Release" || fail "cli dispatch --tag failed"
echo "cli dispatch --tag works"

cat > "$TMP/repo/.github/workflows/concurrency.yml" <<'EOF'
name: Deploy
on:
  workflow_dispatch:
jobs:
  deploy:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    concurrency:
      group: deploy-${{ github.ref_name }}
      cancel-in-progress: true
    steps:
      - run: echo deploy ok
EOF
git -C "$TMP/repo" add . && git -C "$TMP/repo" commit -qm "concurrency workflow"
SHA3="$(git -C "$TMP/repo" rev-parse HEAD)"
export GF_SHA3="$SHA3"
CC_RUN2="$(python3 - <<'PYEOF' || fail "concurrency dispatch failed"
import json, urllib.request, os
base = os.environ["GF_DEV"]
tok = {"Content-Type": "application/json",
       "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]}
def dispatch(payload):
    req = urllib.request.Request(base + "/api/repos/demo/workflows/dispatch",
        data=json.dumps(payload).encode(), headers=tok, method="POST")
    return json.load(urllib.request.urlopen(req, timeout=30))["runs"]
first = dispatch({"workflow": "concurrency.yml", "commit_sha": os.environ["GF_SHA3"]})
second = dispatch({"workflow": "concurrency.yml", "commit_sha": os.environ["GF_SHA3"]})
assert len(first) == 1 and len(second) == 1, (first, second)
assert len(first[0]["job_ids"]) == 1 and len(second[0]["job_ids"]) == 1
q = urllib.request.Request(base + "/api/workflows/runs/" + first[0]["id"],
    headers={"Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]})
jobs = json.load(urllib.request.urlopen(q, timeout=10))["jobs"]
assert jobs[0]["status"] == "cancelled", jobs
job = json.load(urllib.request.urlopen(urllib.request.Request(
    base + "/api/compute/jobs/" + second[0]["job_ids"][0],
    headers={"Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]}), timeout=10))
assert job["concurrency_group"] == "deploy-main", job
assert job["cancel_in_progress"] is True, job
assert job["status"] == "pending", job
print(second[0]["id"])
PYEOF
)"
[ -n "$CC_RUN2" ] || fail "empty concurrency run id"
echo "cancel-in-progress retired the first run and kept the second queued in deploy-main"
export GF_CC_RUN="$CC_RUN2"
CC_DONE=""
for _ in $(seq 1 45); do
  CC_SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/workflows/runs/'+os.environ['GF_CC_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  if [ "$CC_SUM" = "1/1" ]; then CC_DONE="yes"; break; fi
  sleep 2
done
[ "$CC_DONE" = "yes" ] || fail "concurrency run never completed (last summary: $CC_SUM)"
echo "surviving concurrency run completed"

cat > "$TMP/repo/.github/workflows/prod.yml" <<'EOF'
name: ProdDeploy
on:
  workflow_dispatch:
jobs:
  ship:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    environment:
      name: prod
      url: https://example.com/prod
    steps:
      - run: echo shipping to prod
EOF
git -C "$TMP/repo" add . && git -C "$TMP/repo" commit -qm "protected env workflow"
SHA4="$(git -C "$TMP/repo" rev-parse HEAD)"
export GF_SHA4="$SHA4"
ENV_RUN="$(python3 - <<'PYEOF' || fail "env dispatch failed"
import json, urllib.request, os
base = os.environ["GF_DEV"]
tok = {"Content-Type": "application/json",
       "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]}
req = urllib.request.Request(base + "/api/repos/demo/workflows/dispatch",
    data=json.dumps({"workflow": "prod.yml",
                     "commit_sha": os.environ["GF_SHA4"]}).encode(),
    headers=tok, method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1, runs
assert len(runs[0]["job_ids"]) == 1, runs
job = json.load(urllib.request.urlopen(urllib.request.Request(
    base + "/api/compute/jobs/" + runs[0]["job_ids"][0],
    headers={"Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]}), timeout=10))
assert job["environment"] == "prod", job
assert job["environment_url"] == "https://example.com/prod", job
assert job["env_approval_pending"] is True, job
assert job["status"] == "pending", job
print(runs[0]["id"] + " " + runs[0]["job_ids"][0])
PYEOF
)"
[ -n "$ENV_RUN" ] || fail "empty env run id"
ENV_RUN_ID="${ENV_RUN%% *}"; ENV_JOB_ID="${ENV_RUN##* }"
export GF_ENV_RUN="$ENV_RUN_ID" GF_ENV_JOB="$ENV_JOB_ID"
echo "protected env job queued pending approval"
sleep 12
HELD="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/compute/jobs/'+os.environ['GF_ENV_JOB'],headers={'Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']}); print(json.load(urllib.request.urlopen(q,timeout=10))['status'])" || fail "env job poll failed")"
[ "$HELD" = "pending" ] || fail "protected env job ran without approval (status $HELD)"
echo "protected env job held past a dispatch tick without approval"
python3 - <<'PYEOF' || fail "env approve failed"
import json, urllib.request, os
base = os.environ["GF_DEV"]
req = urllib.request.Request(base + "/api/compute/jobs/" + os.environ["GF_ENV_JOB"] + "/approve",
    data=b"{}",
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
d = json.load(urllib.request.urlopen(req, timeout=30))
assert d["approved"] is True, d
assert d["environment"] == "prod", d
print("approved for prod")
PYEOF
HOME="$TMP/home-dev" "$BIN" --config "$TMP/dev.toml" compute approve "$ENV_JOB_ID" 2>&1 | grep -q "approved" || fail "cli compute approve failed"
echo "cli compute approve works"
REJ_JOB="$(python3 - <<'PYEOF' || fail "reject dispatch failed"
import json, urllib.request, os
base = os.environ["GF_DEV"]
tok = {"Content-Type": "application/json",
       "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]}
req = urllib.request.Request(base + "/api/repos/demo/workflows/dispatch",
    data=json.dumps({"workflow": "prod.yml",
                     "commit_sha": os.environ["GF_SHA4"]}).encode(),
    headers=tok, method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1, runs
job = runs[0]["job_ids"][0]
rej = urllib.request.Request(base + "/api/compute/jobs/" + job + "/reject",
    data=b"{}", headers=tok, method="POST")
d = json.load(urllib.request.urlopen(rej, timeout=30))
assert d["rejected"] is True, d
got = json.load(urllib.request.urlopen(urllib.request.Request(
    base + "/api/compute/jobs/" + job,
    headers={"Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]}), timeout=10))
assert got["status"] == "cancelled", got
assert got["env_approval_pending"] is False, got
print(job)
PYEOF
)"
[ -n "$REJ_JOB" ] || fail "empty reject job id"
HOME="$TMP/home-dev" "$BIN" --config "$TMP/dev.toml" compute reject "$REJ_JOB" 2>&1 | grep -q "rejected" || fail "cli compute reject failed"
echo "cli compute reject works"
ENV_DONE=""
for _ in $(seq 1 45); do
  ENV_SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/workflows/runs/'+os.environ['GF_ENV_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  if [ "$ENV_SUM" = "1/1" ]; then ENV_DONE="yes"; break; fi
  sleep 2
done
[ "$ENV_DONE" = "yes" ] || fail "env run never completed (last summary: $ENV_SUM)"
echo "approved protected env run completed"

cat > "$TMP/repo/.github/workflows/coe.yml" <<'EOF'
name: Tolerant
on:
  workflow_dispatch:
jobs:
  flaky:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - run: echo first
      - run: exit 3
        continue-on-error: true
      - run: echo survived > coe.txt
    artifacts:
      - coe.txt
  doomed:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    continue-on-error: true
    steps:
      - run: exit 9
  after:
    needs: doomed
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - run: echo downstream-ran
EOF
git -C "$TMP/repo" add . && git -C "$TMP/repo" commit -qm "continue-on-error workflow"
SHA5="$(git -C "$TMP/repo" rev-parse HEAD)"
export GF_SHA5="$SHA5"
COE_RUN="$(python3 - <<'PYEOF' || fail "coe dispatch failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/repos/demo/workflows/dispatch",
    data=json.dumps({"workflow": "coe.yml",
                     "commit_sha": os.environ["GF_SHA5"]}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1, runs
assert len(runs[0]["job_ids"]) == 3, runs
print(runs[0]["id"])
PYEOF
)"
[ -n "$COE_RUN" ] || fail "empty coe run id"
export GF_COE_RUN="$COE_RUN"
COE_DONE=""
for _ in $(seq 1 60); do
  COE_SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/workflows/runs/'+os.environ['GF_COE_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed'])+'/'+str(s['failed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  if [ "$COE_SUM" = "2/1/3" ]; then COE_DONE="yes"; break; fi
  sleep 2
done
[ "$COE_DONE" = "yes" ] || fail "coe run never settled (last summary: $COE_SUM)"
python3 - <<'PYEOF' || fail "coe artifact mismatch"
import urllib.request, os, json
base = os.environ["GF_DEV"]
tok = {"Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]}
q = urllib.request.Request(base + "/api/workflows/runs/" + os.environ["GF_COE_RUN"], headers=tok)
jobs = json.load(urllib.request.urlopen(q, timeout=10))["jobs"]
flaky = [j for j in jobs if j["name"].endswith("flaky")]
assert len(flaky) == 1 and flaky[0]["status"] == "completed", jobs
dl = urllib.request.Request(base + "/api/compute/jobs/" + flaky[0]["id"] + "/artifacts/coe.txt")
body = urllib.request.urlopen(dl, timeout=30).read()
assert body == b"survived\n", body
after = [j for j in jobs if j["name"].endswith("after")]
assert len(after) == 1 and after[0]["status"] == "completed", jobs
print("swallowed step artifact:", body.decode().strip())
PYEOF
echo "step and job continue-on-error held: flaky completed, doomed failed, downstream ran"

cat > "$TMP/repo/.github/workflows/cache.yml" <<'EOF'
name: CacheDemo
on:
  workflow_dispatch:
jobs:
  build:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - uses: gitforest/cache
        with:
          path: deps
          key: demo-deps-v1
          restore-keys: demo-deps-
      - run: if [ -f deps/seed.txt ]; then echo restored > result.txt; else mkdir -p deps && echo fresh-seed > deps/seed.txt && echo fresh > result.txt; fi
      - uses: gitforest/cache-save
        with:
          path: deps
          key: demo-deps-v1
    artifacts:
      - result.txt
EOF
git -C "$TMP/repo" add . && git -C "$TMP/repo" commit -qm "cache workflow"
SHA6="$(git -C "$TMP/repo" rev-parse HEAD)"
export GF_SHA6="$SHA6"
CACHE_RUN1="$(python3 - <<'PYEOF' || fail "cache dispatch 1 failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/repos/demo/workflows/dispatch",
    data=json.dumps({"workflow": "cache.yml",
                     "commit_sha": os.environ["GF_SHA6"]}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1 and len(runs[0]["job_ids"]) == 1, runs
print(runs[0]["id"])
PYEOF
)"
[ -n "$CACHE_RUN1" ] || fail "empty cache run 1 id"
export GF_CACHE_RUN1="$CACHE_RUN1"
CACHE_DONE=""
for _ in $(seq 1 60); do
  CACHE_SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/workflows/runs/'+os.environ['GF_CACHE_RUN1'],headers={'Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  if [ "$CACHE_SUM" = "1/1" ]; then CACHE_DONE="yes"; break; fi
  sleep 2
done
[ "$CACHE_DONE" = "yes" ] || fail "cache run 1 never completed (last summary: $CACHE_SUM)"
CACHE_RUN2="$(python3 - <<'PYEOF' || fail "cache dispatch 2 failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/repos/demo/workflows/dispatch",
    data=json.dumps({"workflow": "cache.yml",
                     "commit_sha": os.environ["GF_SHA6"]}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1 and len(runs[0]["job_ids"]) == 1, runs
print(runs[0]["id"])
PYEOF
)"
[ -n "$CACHE_RUN2" ] || fail "empty cache run 2 id"
export GF_CACHE_RUN2="$CACHE_RUN2"
CACHE_DONE=""
for _ in $(seq 1 60); do
  CACHE_SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/workflows/runs/'+os.environ['GF_CACHE_RUN2'],headers={'Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  if [ "$CACHE_SUM" = "1/1" ]; then CACHE_DONE="yes"; break; fi
  sleep 2
done
[ "$CACHE_DONE" = "yes" ] || fail "cache run 2 never completed (last summary: $CACHE_SUM)"
python3 - <<'PYEOF' || fail "cache restore mismatch"
import urllib.request, os, json
base = os.environ["GF_DEV"]
tok = {"Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]}
def artifact(run_id):
    q = urllib.request.Request(base + "/api/workflows/runs/" + run_id, headers=tok)
    job = json.load(urllib.request.urlopen(q, timeout=10))["jobs"][0]
    dl = urllib.request.Request(base + "/api/compute/jobs/" + job["id"] + "/artifacts/result.txt")
    return urllib.request.urlopen(dl, timeout=30).read()
first = artifact(os.environ["GF_CACHE_RUN1"])
second = artifact(os.environ["GF_CACHE_RUN2"])
assert first == b"fresh\n", first
assert second == b"restored\n", second
print("run1:", first.decode().strip(), "run2:", second.decode().strip())
PYEOF
echo "cache shared across runs: first built fresh, second restored"

cat > "$TMP/repo/.github/workflows/matrix.yml" <<'EOF'
name: Matrix
on:
  workflow_dispatch:
jobs:
  test:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 512
    disk-mb: 1024
    strategy:
      matrix:
        cmd: ["exit 1", "sleep 25 && echo ok"]
    steps:
      - run: ${{ matrix.cmd }}
  report:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    needs: [test-0, test-1]
    if: failure()
    steps:
      - run: echo "failure handler ran"
      - run: mkdir -p wd-probe && echo "in-subdir" > wd-probe/where.txt
      - run: cat where.txt
        working-directory: wd-probe
  janitor:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    needs: [test-1]
    if: cancelled()
    steps:
      - run: echo "cancelled handler ran"
EOF
git -C "$TMP/repo" add . && git -C "$TMP/repo" commit -qm "matrix workflow"
SHA2="$(git -C "$TMP/repo" rev-parse HEAD)"
export GF_SHA2="$SHA2"
FF_RUN="$(python3 - <<'PYEOF' || fail "matrix dispatch failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/repos/demo/workflows/dispatch",
    data=json.dumps({"workflow": "matrix.yml",
                     "commit_sha": os.environ["GF_SHA2"]}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1, runs
assert len(runs[0]["job_ids"]) == 4, runs
print(runs[0]["id"])
PYEOF
)"
[ -n "$FF_RUN" ] || fail "empty matrix run id"
export GF_FF_RUN="$FF_RUN"
FF_DONE=""
for _ in $(seq 1 60); do
  SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/workflows/runs/'+os.environ['GF_FF_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']}); d=json.load(urllib.request.urlopen(q,timeout=10)); s=d['summary']; print(str(s['completed'])+'/'+str(s['failed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  if [ "$SUM" = "2/2/4" ]; then FF_DONE="yes"; break; fi
  sleep 2
done
[ "$FF_DONE" = "yes" ] || fail "matrix run never settled (last: $SUM)"
python3 - <<'PYEOF' || fail "fail-fast did not cancel the sibling"
import json, urllib.request, os
q = urllib.request.Request(os.environ['GF_DEV'] + '/api/workflows/runs/' + os.environ['GF_FF_RUN'],
    headers={'Authorization': 'Bearer ' + os.environ['GF_DEV_TOKEN']})
jobs = json.load(urllib.request.urlopen(q, timeout=10))['jobs']
statuses = sorted(j['status'] for j in jobs)
assert statuses == ['cancelled', 'completed', 'completed', 'failed'], statuses
print("matrix statuses:", statuses)
PYEOF
echo "fail-fast cancelled the surviving matrix sibling; failure() and cancelled() handlers completed; working-directory step ran"

cat > "$TMP/repo/.github/workflows/gates.yml" <<'EOF'
name: Gates
on:
  workflow_dispatch:
    inputs:
      env:
        default: staging
jobs:
  build:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    outputs:
      digest: ${{ steps.emit.outputs.SHA }}
    steps:
      - id: emit
        run: echo "SHA=abc123" >> "$GF_OUTPUTS"
      - run: echo gated-step >> gate.txt
        if: inputs.env == 'staging'
      - run: echo SHOULD_NOT_EXIST >> gate.txt
        if: inputs.env == 'prod'
      - run: echo steps-guard-ok >> gate.txt
        if: steps.emit.outputs.SHA == 'abc123' && steps.emit.outcome == 'success'
    artifacts:
      - gate.txt
  deploy:
    needs: build
    if: needs.build.outputs.digest == 'abc123'
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - run: echo deployed > deploy.txt
    artifacts:
      - deploy.txt
  muted:
    if: false
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - run: echo SHOULD_NOT_RUN
EOF
git -C "$TMP/repo" add . && git -C "$TMP/repo" commit -qm "gates workflow"
SHA3="$(git -C "$TMP/repo" rev-parse HEAD)"
export GF_SHA3="$SHA3"
GATE_RUN="$(python3 - <<'PYEOF' || fail "gates dispatch failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/repos/demo/workflows/dispatch",
    data=json.dumps({"workflow": "gates.yml", "commit_sha": os.environ["GF_SHA3"],
                     "inputs": {"env": "staging"}}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1, runs
assert len(runs[0]["job_ids"]) == 3, runs
print(runs[0]["id"])
PYEOF
)"
[ -n "$GATE_RUN" ] || fail "empty gates run id"
export GF_GATE_RUN="$GATE_RUN"
GATE_DONE=""
for _ in $(seq 1 60); do
  GATE_SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/workflows/runs/'+os.environ['GF_GATE_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']}); d=json.load(urllib.request.urlopen(q,timeout=10)); s=d['summary']; print(str(s['completed'])+'/'+str(s['failed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  if [ "$GATE_SUM" = "2/1/3" ]; then GATE_DONE="yes"; break; fi
  sleep 2
done
[ "$GATE_DONE" = "yes" ] || fail "gates run never settled (last: $GATE_SUM)"
python3 - <<'PYEOF' || fail "gates statuses wrong"
import json, urllib.request, os
base = os.environ["GF_DEV"]
tok = {"Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]}
q = urllib.request.Request(base + "/api/workflows/runs/" + os.environ["GF_GATE_RUN"], headers=tok)
jobs = json.load(urllib.request.urlopen(q, timeout=10))["jobs"]
statuses = sorted(j["status"] for j in jobs)
assert statuses == ["cancelled", "completed", "completed"], statuses
by_name = {j["name"].rsplit(" / ", 1)[-1]: j for j in jobs}
assert by_name["muted"]["status"] == "cancelled", jobs
assert by_name["deploy"]["status"] == "completed", jobs
gate = urllib.request.urlopen(base + "/api/compute/jobs/" + by_name["build"]["id"] + "/artifacts/gate.txt", timeout=30).read()
assert b"gated-step" in gate, gate
assert b"steps-guard-ok" in gate, gate
assert b"SHOULD_NOT_EXIST" not in gate, gate
dep = urllib.request.urlopen(base + "/api/compute/jobs/" + by_name["deploy"]["id"] + "/artifacts/deploy.txt", timeout=30).read()
assert dep == b"deployed\n", dep
print("gates statuses:", statuses, "| step-if + needs-outputs gating proven")
PYEOF
echo "if-expressions gated jobs and steps: deploy ran on outputs match, muted skipped, prod step excluded"

cat > "$TMP/repo/.github/workflows/services.yml" <<'EOF'
name: Services
on:
  workflow_dispatch:
jobs:
  dbtest:
    runs-on: ubuntu-latest
    docker-image: postgres:16
    cpu: 1
    ram-mb: 1024
    disk-mb: 2048
    services:
      db:
        image: postgres:16
        env:
          POSTGRES_PASSWORD: demo-pass
        options: >-
          --health-cmd "pg_isready -U postgres"
          --health-interval 5s --health-timeout 3s --health-retries 10
    steps:
      - run: getent hosts db > svc.txt
      - run: PGPASSWORD=demo-pass psql -h db -U postgres -tAc "select 'svc-query-ok'" >> svc.txt
    artifacts:
      - svc.txt
EOF
git -C "$TMP/repo" add . && git -C "$TMP/repo" commit -qm "services workflow"
SHASVC="$(git -C "$TMP/repo" rev-parse HEAD)"
export GF_SHASVC="$SHASVC"
DEV_SVC_JOB="$(python3 - <<'PYEOF' || fail "dev services dispatch failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/repos/demo/workflows/dispatch",
    data=json.dumps({"workflow": "services.yml",
                     "commit_sha": os.environ["GF_SHASVC"]}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1 and len(runs[0]["job_ids"]) == 1, runs
print(runs[0]["job_ids"][0])
PYEOF
)"
[ -n "$DEV_SVC_JOB" ] || fail "empty dev services job id"
export GF_DEV_SVC_JOB="$DEV_SVC_JOB"
sleep 12
HELD_SVC="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/compute/jobs/'+os.environ['GF_DEV_SVC_JOB'],headers={'Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']}); print(json.load(urllib.request.urlopen(q,timeout=10))['status'])" || fail "dev services poll failed")"
[ "$HELD_SVC" = "pending" ] || fail "local worker claimed a services job (status $HELD_SVC)"
echo "local worker skipped the services job past a dispatch tick"
python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/compute/jobs/'+os.environ['GF_DEV_SVC_JOB']+'/cancel',data=b'{}',headers={'Content-Type':'application/json','Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']},method='POST'); urllib.request.urlopen(q,timeout=30)" || fail "dev services cancel failed"

python3 - <<'PYEOF' || fail "svc repo add failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_SVC"] + "/api/repos",
    data=json.dumps({"path": os.environ["GF_REPO"], "name": "demo-svc"}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_SVC_TOKEN"]},
    method="POST")
print(json.load(urllib.request.urlopen(req, timeout=30)))
PYEOF
SVC_RUN="$(python3 - <<'PYEOF' || fail "svc dispatch failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_SVC"] + "/api/repos/demo-svc/workflows/dispatch",
    data=json.dumps({"workflow": "services.yml",
                     "commit_sha": os.environ["GF_SHASVC"]}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_SVC_TOKEN"]},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1 and len(runs[0]["job_ids"]) == 1, runs
print(runs[0]["id"])
PYEOF
)"
[ -n "$SVC_RUN" ] || fail "empty svc run id"
export GF_SVC_RUN="$SVC_RUN"
SVC_DONE=""
for _ in $(seq 1 120); do
  SVC_SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_SVC']+'/api/workflows/runs/'+os.environ['GF_SVC_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_SVC_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  if [ "$SVC_SUM" = "1/1" ]; then SVC_DONE="yes"; break; fi
  sleep 2
done
[ "$SVC_DONE" = "yes" ] || fail "services run never completed (last summary: $SVC_SUM)"
python3 - <<'PYEOF' || fail "services artifact mismatch"
import urllib.request, os, json
base = os.environ["GF_SVC"]
tok = {"Authorization": "Bearer " + os.environ["GF_SVC_TOKEN"]}
q = urllib.request.Request(base + "/api/workflows/runs/" + os.environ["GF_SVC_RUN"], headers=tok)
jobs = json.load(urllib.request.urlopen(q, timeout=10))["jobs"]
assert len(jobs) == 1, jobs
dl = urllib.request.Request(base + "/api/compute/jobs/" + jobs[0]["id"] + "/artifacts/svc.txt")
body = urllib.request.urlopen(dl, timeout=30).read()
assert b"svc-query-ok" in body, body
assert b"db" in body.splitlines()[0], body
print("services artifact:", body.decode().strip().replace("\n", " | "))
PYEOF
echo "docker worker resolved the db sidecar by name and ran a live psql query"
[ "$(docker network ls --format '{{.Name}}' | grep -c '^gf-svc-' || true)" = "0" ] || fail "service networks leaked"
[ -z "$(docker ps -aq --filter 'name=gf-svc-' || true)" ] || fail "service containers leaked"
echo "service networks and containers torn down cleanly"

cat > "$TMP/repo/.github/workflows/container.yml" <<'EOF'
name: ContainerDemo
on:
  workflow_dispatch:
jobs:
  web:
    runs-on: ubuntu-latest
    network: true
    cpu: 1
    ram-mb: 1024
    disk-mb: 2048
    container:
      image: python:3.12-alpine
      env:
        DEMO_ENV: container-env-ok
      ports:
        - "18081:8080"
      volumes:
        - /data/scratch
      options: --cpus 1 --memory 256m
    steps:
      - run: echo "$DEMO_ENV" > web.txt && echo container-port-ok > /data/scratch/index.html && echo volume-marker >> web.txt
      - run: cd /data/scratch && python3 -m http.server 8080 & sleep 45
    artifacts:
      - web.txt
EOF
git -C "$TMP/repo" add . && git -C "$TMP/repo" commit -qm "container workflow"
SHACTR="$(git -C "$TMP/repo" rev-parse HEAD)"
export GF_SHACTR="$SHACTR"
DEV_CTR_JOB="$(python3 - <<'PYEOF' || fail "dev container dispatch failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/repos/demo/workflows/dispatch",
    data=json.dumps({"workflow": "container.yml",
                     "commit_sha": os.environ["GF_SHACTR"]}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1 and len(runs[0]["job_ids"]) == 1, runs
print(runs[0]["job_ids"][0])
PYEOF
)"
[ -n "$DEV_CTR_JOB" ] || fail "empty dev container job id"
export GF_DEV_CTR_JOB="$DEV_CTR_JOB"
sleep 12
HELD_CTR="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/compute/jobs/'+os.environ['GF_DEV_CTR_JOB'],headers={'Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']}); print(json.load(urllib.request.urlopen(q,timeout=10))['status'])" || fail "dev container poll failed")"
[ "$HELD_CTR" = "pending" ] || fail "local worker claimed a container job (status $HELD_CTR)"
echo "local worker skipped the container job past a dispatch tick"
python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_DEV']+'/api/compute/jobs/'+os.environ['GF_DEV_CTR_JOB']+'/cancel',data=b'{}',headers={'Content-Type':'application/json','Authorization':'Bearer '+os.environ['GF_DEV_TOKEN']},method='POST'); urllib.request.urlopen(q,timeout=30)" || fail "dev container cancel failed"

CTR_RUN="$(python3 - <<'PYEOF' || fail "ctr dispatch failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_SVC"] + "/api/repos/demo-svc/workflows/dispatch",
    data=json.dumps({"workflow": "container.yml",
                     "commit_sha": os.environ["GF_SHACTR"]}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_SVC_TOKEN"]},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1 and len(runs[0]["job_ids"]) == 1, runs
print(runs[0]["id"] + " " + runs[0]["job_ids"][0])
PYEOF
)"
[ -n "$CTR_RUN" ] || fail "empty ctr run id"
CTR_RUN_ID="${CTR_RUN%% *}"; CTR_JOB_ID="${CTR_RUN##* }"
export GF_CTR_RUN="$CTR_RUN_ID" GF_CTR_JOB="$CTR_JOB_ID"
SERVED=""
for _ in $(seq 1 40); do
  if curl -s --noproxy '*' --max-time 3 "http://127.0.0.1:18081/" 2>/dev/null | grep -q "container-port-ok"; then SERVED="yes"; break; fi
  sleep 2
done
[ "$SERVED" = "yes" ] || fail "published container port never served the marker page"
echo "job served the marker page on the published host port mid-run"
CTR_DONE=""
for _ in $(seq 1 60); do
  CTR_SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_SVC']+'/api/workflows/runs/'+os.environ['GF_CTR_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_SVC_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  if [ "$CTR_SUM" = "1/1" ]; then CTR_DONE="yes"; break; fi
  sleep 2
done
[ "$CTR_DONE" = "yes" ] || fail "container run never completed (last summary: $CTR_SUM)"
python3 - <<'PYEOF' || fail "container artifact mismatch"
import urllib.request, os, json
base = os.environ["GF_SVC"]
tok = {"Authorization": "Bearer " + os.environ["GF_SVC_TOKEN"]}
q = urllib.request.Request(base + "/api/workflows/runs/" + os.environ["GF_CTR_RUN"], headers=tok)
jobs = json.load(urllib.request.urlopen(q, timeout=10))["jobs"]
assert len(jobs) == 1, jobs
dl = urllib.request.Request(base + "/api/compute/jobs/" + jobs[0]["id"] + "/artifacts/web.txt")
body = urllib.request.urlopen(dl, timeout=30).read()
assert b"container-env-ok" in body, body
assert b"volume-marker" in body, body
print("container artifact:", body.decode().strip().replace("\n", " | "))
PYEOF
echo "container env and volume round-tripped through the artifact"

head -c 2097152 /dev/urandom > "$TMP/weights.gguf"
printf 'GGUF' | dd of="$TMP/weights.gguf" bs=1 seek=0 conv=notrunc status=none
SHA_LOCAL="$(sha256sum "$TMP/weights.gguf" | cut -d' ' -f1)"
export GF_FILE="$TMP/weights.gguf"
BLOB_ID="$(python3 - <<'PYEOF' || fail "blob upload failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/blobs",
    data=json.dumps({"path": os.environ["GF_FILE"]}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
print(json.load(urllib.request.urlopen(req, timeout=30))["blob_id"])
PYEOF
)"
[ -n "$BLOB_ID" ] || fail "empty blob id"
export GF_BLOB="$BLOB_ID"

COLL_ID="$(python3 - <<'PYEOF' || fail "collection create failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/collections",
    data=json.dumps({"name": "demo-llama-1m", "description": "demo tiny model",
        "files": [{"path": "weights.gguf", "blob_id": os.environ["GF_BLOB"], "size": 2097152}],
        "metadata": {"architecture": "llama", "parameter_count": "1000000",
                     "format": "gguf", "quantization": "q4_0"}}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
print(json.load(urllib.request.urlopen(req, timeout=30))["id"])
PYEOF
)"
[ -n "$COLL_ID" ] || fail "empty collection id"
export GF_COLL="$COLL_ID"

python3 - <<'PYEOF' || fail "model publish failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/models/publish",
    data=json.dumps({"collection_id": os.environ["GF_COLL"]}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
d = json.load(urllib.request.urlopen(req, timeout=30))
assert d["status"] == "published", d
print(d)
PYEOF
echo "model published on DHT"

FOUND=""
for _ in $(seq 1 45); do
  if python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_PEER']+'/api/models/search?architecture=llama',headers={'Authorization':'Bearer '+os.environ['GF_PEER_TOKEN']}); raise SystemExit(0 if any(m['name']=='demo-llama-1m' for m in json.load(urllib.request.urlopen(q,timeout=10))) else 1)" 2>/dev/null; then FOUND="yes"; break; fi
  sleep 2
done
[ "$FOUND" = "yes" ] || fail "peer never discovered the model via DHT"
echo "peer discovered demo-llama-1m through the DHT"

PROXIED=""
for _ in $(seq 1 20); do
  PROXIED="$(python3 - <<'PYEOF' 2>/dev/null || true
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_PEER"] + "/api/models/" + os.environ["GF_COLL"] + "/infer",
    data=json.dumps({"prompt": "hello", "max_tokens": 16}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_PEER_TOKEN"]},
    method="POST")
d = json.load(urllib.request.urlopen(req, timeout=60))
assert d.get("proxied") is True, d
assert "demo completion" in d.get("completion", ""), d
print("proxied by " + d.get("served_by", "?") + ": " + d["completion"].strip())
PYEOF
)"
  if [ -n "$PROXIED" ]; then break; fi
  sleep 3
done
[ -n "$PROXIED" ] || fail "peer never proxied inference to the seeder"
echo "$PROXIED"
curl -s --noproxy '*' -o /dev/null -w "%{http_code}" "http://127.0.0.1:$PEER_API/api/blobs/$BLOB_ID/meta" | grep -q 404 || fail "peer holds weights it should not have downloaded"
echo "peer inferred remotely without downloading weights"

python3 - <<'PYEOF' || fail "remote streaming inference failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_PEER"] + "/api/models/" + os.environ["GF_COLL"] + "/infer/stream",
    data=json.dumps({"prompt": "hello", "max_tokens": 16}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_PEER_TOKEN"]},
    method="POST")
resp = urllib.request.urlopen(req, timeout=120)
assert resp.headers.get_content_type() == "text/event-stream", resp.headers
events = [json.loads(line[5:]) for line in resp.read().decode().splitlines() if line.startswith("data:")]
completion = "".join(e.get("chunk", "") for e in events)
assert any("done" in e for e in events), events
assert "demo completion" in completion, completion
print("relayed stream:", completion.strip())
PYEOF
echo "peer streamed inference remotely without downloading weights"
RSTREAMED="$(HOME="$TMP/home-peer" "$BIN" --config "$TMP/peer.toml" llm run --model "$GF_COLL" --prompt hello --max-tokens 16 --stream 2>"$TMP/cli-rstream.err")" || fail "cli remote stream failed: $(cat "$TMP/cli-rstream.err")"
echo "$RSTREAMED" | grep -q "demo completion" || fail "cli remote stream missed the completion: $RSTREAMED"
echo "cli streamed the remote completion: $RSTREAMED"

python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_PEER']+'/api/collections/'+os.environ['GF_COLL']+'/fetch',data=b'{}',headers={'Content-Type':'application/json','Authorization':'Bearer '+os.environ['GF_PEER_TOKEN']},method='POST'); urllib.request.urlopen(q,timeout=30)" || fail "collection fetch failed"
GOT=""
for _ in $(seq 1 60); do
  if curl -s --noproxy '*' -o "$TMP/got.gguf" -w "%{http_code}" "http://127.0.0.1:$PEER_API/api/blobs/$BLOB_ID/download" | grep -q 200; then
    if [ -s "$TMP/got.gguf" ]; then GOT="yes"; break; fi
  fi
  sleep 2
done
[ "$GOT" = "yes" ] || fail "peer never completed the model weights"
SHA_GOT="$(sha256sum "$TMP/got.gguf" | cut -d' ' -f1)"
[ "$SHA_GOT" = "$SHA_LOCAL" ] || fail "bytes differ: $SHA_GOT != $SHA_LOCAL"
PIECES="$(python3 -c "import json,urllib.request,os; print(len(json.load(urllib.request.urlopen(urllib.request.Request(os.environ['GF_PEER']+'/api/blobs/'+os.environ['GF_BLOB']+'/meta'),timeout=10))['piece_hashes']))")"
[ "$PIECES" = "8" ] || fail "expected 8 weight pieces, saw $PIECES"
echo "model weights identical after P2P transfer: $SHA_GOT ($PIECES pieces)"

python3 - <<'PYEOF' || fail "peer inference failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_PEER"] + "/api/models/" + os.environ["GF_COLL"] + "/infer",
    data=json.dumps({"prompt": "hello", "max_tokens": 16}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_PEER_TOKEN"]},
    method="POST")
d = json.load(urllib.request.urlopen(req, timeout=120))
assert d["status"] == "complete", d
assert "demo completion" in d["completion"], d
print("peer inference over transferred weights:", d["completion"].strip())
PYEOF
echo "peer ran inference on the P2P-shared model"

python3 - <<'PYEOF' || fail "streaming inference failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_PEER"] + "/api/models/" + os.environ["GF_COLL"] + "/infer/stream",
    data=json.dumps({"prompt": "hello", "max_tokens": 16}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_PEER_TOKEN"]},
    method="POST")
body = urllib.request.urlopen(req, timeout=120).read().decode()
events = [json.loads(line[5:]) for line in body.splitlines() if line.startswith("data:")]
completion = "".join(e.get("chunk", "") for e in events)
assert any("done" in e for e in events), events
assert "demo completion" in completion, completion
print("streamed completion:", completion.strip())
PYEOF
echo "peer streamed inference over the P2P-shared model"

STREAMED="$(HOME="$TMP/home-peer" "$BIN" --config "$TMP/peer.toml" llm run --model "$GF_COLL" --prompt hello --max-tokens 16 --stream 2>"$TMP/cli-stream.err")" || fail "cli llm run --stream failed: $(cat "$TMP/cli-stream.err")"
echo "$STREAMED" | grep -q "demo completion" || fail "cli stream missed the completion: $STREAMED"
echo "cli streamed the completion: $STREAMED"

PEER_ID="$(python3 -c "import json,urllib.request,os; print(json.load(urllib.request.urlopen(os.environ['GF_PEER']+'/api/status',timeout=10))['peer_id'])")" || fail "peer id failed"
[ -n "$PEER_ID" ] || fail "empty peer id"
export GF_PEER_ID="$PEER_ID"
head -c 307200 /dev/urandom > "$TMP/gated.gguf"
printf 'GGUF' | dd of="$TMP/gated.gguf" bs=1 seek=0 conv=notrunc status=none
export GF_GATED="$TMP/gated.gguf"
GATED_BLOB="$(python3 - <<'PYEOF' || fail "gated blob upload failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/blobs",
    data=json.dumps({"path": os.environ["GF_GATED"]}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
print(json.load(urllib.request.urlopen(req, timeout=30))["blob_id"])
PYEOF
)"
[ -n "$GATED_BLOB" ] || fail "empty gated blob id"
export GF_GATED_BLOB="$GATED_BLOB"
GATED_COLL="$(python3 - <<'PYEOF' || fail "gated collection failed"
import json, urllib.request, os
base = os.environ["GF_DEV"]
tok = {"Content-Type": "application/json",
       "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]}
req = urllib.request.Request(base + "/api/collections",
    data=json.dumps({"name": "gated-llama", "description": "restricted demo model",
        "files": [{"path": "gated.gguf", "blob_id": os.environ["GF_GATED_BLOB"], "size": 307200}],
        "metadata": {"architecture": "llama", "parameter_count": "1000000",
                     "format": "gguf", "quantization": "q4_0"}}).encode(),
    headers=tok, method="POST")
coll = json.load(urllib.request.urlopen(req, timeout=30))["id"]
pub = urllib.request.Request(base + "/api/models/publish",
    data=json.dumps({"collection_id": coll}).encode(), headers=tok, method="POST")
d = json.load(urllib.request.urlopen(pub, timeout=30))
assert d["status"] == "published", d
acl = urllib.request.Request(base + "/api/models/" + coll + "/acl",
    data=json.dumps({"action": "restrict"}).encode(), headers=tok, method="POST")
a = json.load(urllib.request.urlopen(acl, timeout=30))
assert a["restricted"] is True, a
print(coll)
PYEOF
)"
[ -n "$GATED_COLL" ] || fail "empty gated collection id"
export GF_GATED_COLL="$GATED_COLL"
echo "restricted model published and locked to its owner"
GATED_FOUND=""
for _ in $(seq 1 45); do
  if python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_PEER']+'/api/models/search?architecture=llama',headers={'Authorization':'Bearer '+os.environ['GF_PEER_TOKEN']}); raise SystemExit(0 if any(m['name']=='gated-llama' for m in json.load(urllib.request.urlopen(q,timeout=10))) else 1)" 2>/dev/null; then GATED_FOUND="yes"; break; fi
  sleep 2
done
[ "$GATED_FOUND" = "yes" ] || fail "peer never discovered the gated model"
curl -s --noproxy '*' -o /dev/null -w "%{http_code}" "http://127.0.0.1:$DEV_API/api/collections/$GATED_COLL" | grep -q 403 || fail "unsigned manifest fetch was not denied"
curl -s --noproxy '*' -o /dev/null -w "%{http_code}" "http://127.0.0.1:$DEV_API/api/blobs/$GATED_BLOB/meta" | grep -q 403 || fail "unsigned blob meta was not denied"
curl -s --noproxy '*' -o /dev/null -w "%{http_code}" "http://127.0.0.1:$DEV_API/api/blobs/$GATED_BLOB/pieces/0" | grep -q 403 || fail "unsigned piece fetch was not denied"
echo "unsigned direct reads of the gated model are denied"
DENIED=""
for _ in $(seq 1 20); do
  DENIED="$(python3 - <<'PYEOF' 2>/dev/null || true
import json, urllib.request, os, urllib.error
req = urllib.request.Request(os.environ["GF_PEER"] + "/api/models/" + os.environ["GF_GATED_COLL"] + "/infer",
    data=json.dumps({"prompt": "hello", "max_tokens": 16}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_PEER_TOKEN"]},
    method="POST")
try:
    d = json.load(urllib.request.urlopen(req, timeout=60))
    print("granted:" + d.get("status", "?"))
except urllib.error.HTTPError as e:
    print("denied:" + str(e.code))
PYEOF
)"
  case "$DENIED" in
    denied:502) break ;;
    granted:*) fail "peer inferred on a gated model without a grant: $DENIED" ;;
  esac
  DENIED=""
  sleep 3
done
[ "$DENIED" = "denied:502" ] || fail "gated proxied infer was never denied"
echo "proxied inference on the gated model denied without a grant"
python3 - <<'PYEOF' || fail "model grant failed"
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_DEV"] + "/api/models/" + os.environ["GF_GATED_COLL"] + "/acl",
    data=json.dumps({"action": "grant", "peer_id": os.environ["GF_PEER_ID"]}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]},
    method="POST")
d = json.load(urllib.request.urlopen(req, timeout=30))
assert d["restricted"] is True, d
assert os.environ["GF_PEER_ID"] in json.load(urllib.request.urlopen(urllib.request.Request(
    os.environ["GF_DEV"] + "/api/models/" + os.environ["GF_GATED_COLL"] + "/acl",
    headers={"Authorization": "Bearer " + os.environ["GF_DEV_TOKEN"]}), timeout=30))["grants"]
print("peer granted")
PYEOF
HOME="$TMP/home-dev" "$BIN" --config "$TMP/dev.toml" model acl "$GATED_COLL" --action show 2>&1 | grep -q "Granted: $PEER_ID" || fail "cli model acl show missed the grant"
echo "cli model acl lists the peer grant"
GATED_PROXIED=""
for _ in $(seq 1 20); do
  GATED_PROXIED="$(python3 - <<'PYEOF' 2>/dev/null || true
import json, urllib.request, os
req = urllib.request.Request(os.environ["GF_PEER"] + "/api/models/" + os.environ["GF_GATED_COLL"] + "/infer",
    data=json.dumps({"prompt": "hello", "max_tokens": 16}).encode(),
    headers={"Content-Type": "application/json",
             "Authorization": "Bearer " + os.environ["GF_PEER_TOKEN"]},
    method="POST")
d = json.load(urllib.request.urlopen(req, timeout=60))
assert d.get("proxied") is True, d
assert "demo completion" in d.get("completion", ""), d
print("proxied gated inference: " + d["completion"].strip())
PYEOF
)"
  if [ -n "$GATED_PROXIED" ]; then break; fi
  sleep 3
done
[ -n "$GATED_PROXIED" ] || fail "granted peer never completed gated inference"
echo "$GATED_PROXIED"
curl -s --noproxy '*' -o /dev/null -w "%{http_code}" "http://127.0.0.1:$PEER_API/api/blobs/$GATED_BLOB/meta" | grep -q 404 || fail "peer holds gated weights it never fetched"
echo "granted peer inferred remotely without holding gated weights"

echo "WORKFLOWS-MODELS PASS"
