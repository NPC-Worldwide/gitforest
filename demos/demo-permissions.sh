#!/bin/bash
set -u
export no_proxy='*' NO_PROXY='*'
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/gitforest"
TMP="$(mktemp -d /tmp/gf-permissions-XXXXXX)"
CLUSTER="permissions-proof"

RX_API=17912; RX_P2P=19484

cleanup() {
  for pid in $(pgrep -f -- "gitforest --config $TMP/" 2>/dev/null | grep -v "^$$\$" || true); do
    kill "$pid" 2>/dev/null || true
  done
  sleep 1
  rm -rf "$TMP"
}
trap cleanup EXIT

fail() {
  echo "PERMISSIONS FAIL: $1"
  echo "--- daemon log tail ---"
  tail -n 15 "$TMP/node.log" 2>/dev/null
  exit 1
}

echo "== gitforest job permissions + token proof =="
(cd "$ROOT" && cargo build --release -p gt-daemon) || fail "build failed"

mkdir -p "$TMP/perms/.github/workflows" "$TMP/perms/probes"
cat > "$TMP/perms/.github/workflows/tokens.yml" <<'EOF'
name: TokenProof
on: push
permissions:
  contents: read
jobs:
  roundtrip:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    permissions:
      contents: write
    steps:
      - run: python3 probes/roundtrip.py
    artifacts:
      - roundtrip.txt
  reader:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    permissions:
      workflows: read
    steps:
      - run: python3 probes/reader.py
    artifacts:
      - reader.txt
  denied:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    permissions:
      contents: read
    steps:
      - run: python3 probes/denied.py
    artifacts:
      - denied.txt
  narrow:
    uses: ./reusable-tokens.yml
    permissions: inherit
  cacher:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    permissions:
      contents: write
    steps:
      - run: mkdir -p deps && echo "cached-payload" > deps/lib.txt
      - uses: gitforest/cache-save@v1
        with:
          key: perms-deps-v1
          path: deps
      - run: echo "save-ok" > cache-save.txt
    artifacts:
      - cache-save.txt
  restorer:
    needs: [cacher]
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    permissions:
      contents: read
    steps:
      - uses: gitforest/cache@v1
        with:
          key: perms-deps-v1
          path: deps
      - run: test "$(cat deps/lib.txt)" = "cached-payload" && echo "restore-ok" > cache-restore.txt
    artifacts:
      - cache-restore.txt
EOF
cat > "$TMP/perms/.github/workflows/reusable-tokens.yml" <<'EOF'
name: TokenCallee
on:
  workflow_call:
permissions:
  contents: write
jobs:
  probe:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - run: python3 probes/narrow.py
    artifacts:
      - narrow.txt
EOF
cat > "$TMP/perms/probes/roundtrip.py" <<'EOF'
import json, os, urllib.request
base = os.environ["GF_API_URL"]
tok = os.environ["GF_JOB_TOKEN"]
assert tok.startswith("gfj."), "missing job token"
payload = b"perm-probe-payload\n"
with open("payload.bin", "wb") as f:
    f.write(payload)
up = urllib.request.Request(
    base + "/api/blobs",
    data=json.dumps({"path": os.path.abspath("payload.bin")}).encode(),
    headers={"Content-Type": "application/json", "Authorization": "Bearer " + tok},
    method="POST",
)
blob = json.load(urllib.request.urlopen(up, timeout=30))["blob_id"]
dl = urllib.request.Request(
    base + "/api/blobs/" + blob + "/download",
    headers={"Authorization": "Bearer " + tok},
)
got = urllib.request.urlopen(dl, timeout=30).read()
assert got == payload, (len(got), len(payload))
open("roundtrip.txt", "w").write("roundtrip-ok\n")
print("upload+download roundtrip byte-identical under contents:write")
EOF
cat > "$TMP/perms/probes/reader.py" <<'EOF'
import json, os, urllib.request
base = os.environ["GF_API_URL"]
tok = os.environ["GF_JOB_TOKEN"]
q = urllib.request.Request(
    base + "/api/workflows/runs", headers={"Authorization": "Bearer " + tok}
)
runs = json.load(urllib.request.urlopen(q, timeout=30))
assert isinstance(runs, list) and len(runs) >= 1, runs
open("reader.txt", "w").write("reader-ok\n")
print("workflows:read listed runs")
EOF
cat > "$TMP/perms/probes/denied.py" <<'EOF'
import os, urllib.error, urllib.request
base = os.environ["GF_API_URL"]
tok = os.environ["GF_JOB_TOKEN"]
q = urllib.request.Request(
    base + "/api/workflows/runs", headers={"Authorization": "Bearer " + tok}
)
try:
    urllib.request.urlopen(q, timeout=30)
except urllib.error.HTTPError as e:
    assert e.code == 403, e.code
    open("denied.txt", "w").write("denied-ok\n")
    print("contents:read token correctly got 403 on workflows:read")
else:
    raise SystemExit("expected 403, got 200")
EOF
cat > "$TMP/perms/probes/narrow.py" <<'EOF'
import json, os, urllib.error, urllib.request
base = os.environ["GF_API_URL"]
tok = os.environ["GF_JOB_TOKEN"]
q = urllib.request.Request(
    base + "/api/caches/list", headers={"Authorization": "Bearer " + tok}
)
caches = json.load(urllib.request.urlopen(q, timeout=30))
assert "local" in caches, caches
save = urllib.request.Request(
    base + "/api/caches",
    data=json.dumps({"key": "narrowprobe1", "path": "/tmp/gf-narrow-nope"}).encode(),
    headers={"Content-Type": "application/json", "Authorization": "Bearer " + tok},
    method="POST",
)
try:
    urllib.request.urlopen(save, timeout=30)
except urllib.error.HTTPError as e:
    assert e.code == 403, e.code
    open("narrow.txt", "w").write("narrow-ok\n")
    print("inherited read passed through, write capped to 403")
else:
    raise SystemExit("expected 403 on capped write, got 200")
EOF
git -C "$TMP/perms" init -q --initial-branch=main
git -C "$TMP/perms" config user.email t@t; git -C "$TMP/perms" config user.name t
git -C "$TMP/perms" add .; git -C "$TMP/perms" commit -qm init
SHA="$(git -C "$TMP/perms" rev-parse HEAD)"

cat > "$TMP/node.toml" <<EOF
data_dir = "$TMP/data"
p2p_port = $RX_P2P
p2p_listen = "127.0.0.1"
api_port = $RX_API
advertise_addr = "/ip4/127.0.0.1/tcp/$RX_P2P/api/$RX_API"
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
execution_mode = "local"
gpu_enabled = false
EOF

HOME="$TMP/home" "$BIN" init --name permproof >/dev/null 2>&1 || fail "init failed"
HOME="$TMP/home" setsid -f "$BIN" --config "$TMP/node.toml" daemon </dev/null >>"$TMP/node.log" 2>&1
ready=no
for _ in $(seq 1 30); do
  CODE="$(curl -s --noproxy '*' -o /dev/null -w "%{http_code}" "http://127.0.0.1:$RX_API/api/status" || true)"
  [ "$CODE" = "200" ] && { ready=yes; break; }
  sleep 1
done
[ "$ready" = "yes" ] || fail "daemon did not start"
TOK="$(cat "$TMP/home/.local/share/gitforest/api.token")"
[ -n "$TOK" ] || fail "missing api token"
export GF_BASE="http://127.0.0.1:$RX_API" GF_TOKEN="$TOK" GF_SHA="$SHA" GF_TMP="$TMP"
echo "daemon up"

python3 - <<'PYEOF' || fail "repo setup failed"
import json, urllib.request, os
base, tok = os.environ["GF_BASE"], os.environ["GF_TOKEN"]
tmp = os.environ["GF_TMP"]
req = urllib.request.Request(base + "/api/repos",
    data=json.dumps({"path": tmp + "/perms", "name": "perms"}).encode(),
    headers={"Content-Type": "application/json", "Authorization": "Bearer " + tok},
    method="POST")
json.load(urllib.request.urlopen(req, timeout=30))
print("repo registered")
PYEOF

RUN_ID="$(python3 - <<'PYEOF' || fail "dispatch failed"
import json, urllib.request, os
base, tok = os.environ["GF_BASE"], os.environ["GF_TOKEN"]
req = urllib.request.Request(base + "/api/repos/perms/workflows/dispatch",
    data=json.dumps({"workflow": "tokens.yml", "branch": "main",
                     "commit_sha": os.environ["GF_SHA"]}).encode(),
    headers={"Content-Type": "application/json", "Authorization": "Bearer " + tok},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1, runs
assert runs[0]["workflow"] == "TokenProof", runs
assert len(runs[0]["job_ids"]) == 6, runs
print(runs[0]["id"])
PYEOF
)"
[ -n "$RUN_ID" ] || fail "empty run id"
echo "permission run dispatched (6 jobs incl. narrowing call + cache pair)"
export GF_RUN="$RUN_ID"
DONE=""
for _ in $(seq 1 60); do
  SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_BASE']+'/api/workflows/runs/'+os.environ['GF_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  [ "$SUM" = "6/6" ] && { DONE="yes"; break; }
  sleep 5
done
[ "$DONE" = "yes" ] || fail "run never completed (last summary: $SUM)"

python3 - <<'PYEOF' || fail "permission proof failed"
import urllib.request, os, json
base, tok = os.environ["GF_BASE"], os.environ["GF_TOKEN"]
h = {"Authorization": "Bearer " + tok}
q = urllib.request.Request(base + "/api/workflows/runs/" + os.environ["GF_RUN"], headers=h)
jobs = json.load(urllib.request.urlopen(q, timeout=10))["jobs"]
assert len(jobs) == 6, jobs
def artifact(job_id, name):
    dl = urllib.request.Request(base + "/api/compute/jobs/" + job_id + "/artifacts/" + name, headers=h)
    try:
        return urllib.request.urlopen(dl, timeout=30).read()
    except Exception:
        return None
marks = {}
for j in jobs:
    for name in ("roundtrip.txt", "reader.txt", "denied.txt", "narrow.txt", "cache-save.txt", "cache-restore.txt"):
        b = artifact(j["id"], name)
        if b:
            marks[name] = b
assert marks.get("roundtrip.txt") == b"roundtrip-ok\n", marks
assert marks.get("reader.txt") == b"reader-ok\n", marks
assert marks.get("denied.txt") == b"denied-ok\n", marks
assert marks.get("narrow.txt") == b"narrow-ok\n", marks
assert marks.get("cache-save.txt") == b"save-ok\n", marks
assert marks.get("cache-restore.txt") == b"restore-ok\n", marks
print("write roundtrip ok; read ok; cross-scope 403 ok; call narrowing ok; in-step cache save/restore ok")
PYEOF

FIRST_JOB="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_BASE']+'/api/workflows/runs/'+os.environ['GF_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_TOKEN']}); print(json.load(urllib.request.urlopen(q,timeout=10))['jobs'][0]['id'])" 2>/dev/null || true)"
[ -n "$FIRST_JOB" ] || fail "no job id for CLI check"
HOME="$TMP/home" "$BIN" --config "$TMP/node.toml" compute status "$FIRST_JOB" | grep -q "Permissions: " || fail "CLI hides job permissions"
echo "CLI shows job permissions"

echo "PERMISSIONS PASS"
