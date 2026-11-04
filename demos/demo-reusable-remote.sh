#!/bin/bash
set -u
export no_proxy='*' NO_PROXY='*'
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/gitforest"
TMP="$(mktemp -d /tmp/gf-reuse-remote-XXXXXX)"
CLUSTER="reuse-remote-proof"

RX_API=17908; RX_P2P=19480

cleanup() {
  for pid in $(pgrep -f -- "gitforest --config $TMP/" 2>/dev/null | grep -v "^$$\$" || true); do
    kill "$pid" 2>/dev/null || true
  done
  sleep 1
  rm -rf "$TMP"
}
trap cleanup EXIT

fail() {
  echo "REUSABLE-REMOTE FAIL: $1"
  echo "--- daemon log tail ---"
  tail -n 15 "$TMP/node.log" 2>/dev/null
  exit 1
}

echo "== gitforest cross-repo reusable workflow proof =="
(cd "$ROOT" && cargo build --release -p gt-daemon) || fail "build failed"

mkdir -p "$TMP/home" "$TMP/data" "$TMP/shared/.gitforest/workflows" "$TMP/caller/.gitforest/workflows"
cat > "$TMP/shared/.gitforest/workflows/build.yml" <<'EOF'
name: SharedBuild
on:
  workflow_call:
    inputs:
      greeting:
        required: true
      punctuation:
        default: "!"
    secrets:
      TOKEN:
        required: true
    outputs:
      digest: ${{ jobs.greet.outputs.digest }}
jobs:
  greet:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    outputs:
      digest: ${{ steps.emit.outputs.SHA }}
    steps:
      - id: emit
        run: echo "SHA=${{ inputs.greeting }}-digest" >> "$GF_OUTPUTS"
      - run: echo "${{ inputs.greeting }}${{ inputs.punctuation }} ${{ secrets.TOKEN }}" > greet.txt
    artifacts:
      - greet.txt
EOF
git -C "$TMP/shared" init -q --initial-branch=main
git -C "$TMP/shared" config user.email t@t; git -C "$TMP/shared" config user.name t
git -C "$TMP/shared" add .; git -C "$TMP/shared" commit -qm init
git -C "$TMP/shared" tag v1
mkdir -p "$TMP/shared/.gitforest/actions/tagline"
cat > "$TMP/shared/.gitforest/actions/tagline/action.yml" <<'EOF'
name: Tagline
inputs:
  text:
    required: true
runs:
  using: composite
  steps:
    - run: echo "tagline-${{ inputs.text }}" > tagline.txt
EOF
git -C "$TMP/shared" add .; git -C "$TMP/shared" commit -qm "composite action"

cat > "$TMP/caller/.gitforest/workflows/ci.yml" <<'EOF'
name: Caller
on:
  push:
jobs:
  hello:
    uses: shared/.gitforest/workflows/build.yml@main
    with:
      greeting: hi
    secrets: inherit
  pinned:
    uses: shared/.gitforest/workflows/build.yml@v1
    with:
      greeting: yo
    secrets: inherit
  consume:
    needs: [hello]
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - run: echo "${{ needs.hello.outputs.digest }}" > downstream.txt
      - run: echo "## consume got ${{ needs.hello.outputs.digest }}" >> "$GF_SUMMARY"
    artifacts:
      - downstream.txt
  consume_inner:
    needs: [hello/greet]
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - run: echo "${{ needs.hello/greet.outputs.digest }}" > downstream-inner.txt
    artifacts:
      - downstream-inner.txt
  setup:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - uses: ../actions/prefix/action.yml
        with:
          text: local
    artifacts:
      - prefixed.txt
  brand:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - uses: shared/.gitforest/actions/tagline/action.yml@main
        with:
          text: remote
    artifacts:
      - tagline.txt
EOF
mkdir -p "$TMP/caller/.gitforest/actions/prefix"
cat > "$TMP/caller/.gitforest/actions/prefix/action.yml" <<'EOF'
name: Prefix
inputs:
  text:
    required: true
runs:
  using: composite
  steps:
    - run: echo "prefix-${{ inputs.text }}" > prefixed.txt
EOF
cat > "$TMP/caller/.gitforest/workflows/bad.yml" <<'EOF'
name: BadCall
on:
  workflow_dispatch:
jobs:
  hello:
    uses: nosuch/.gitforest/workflows/build.yml@main
EOF
cat > "$TMP/caller/.gitforest/workflows/fail.yml" <<'EOF'
name: Flaky
on:
  workflow_dispatch:
jobs:
  good:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - run: echo ok
  bad:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - run: exit 1
EOF
cat > "$TMP/caller/.gitforest/workflows/shell.yml" <<'EOF'
name: ShellSel
on:
  workflow_dispatch:
jobs:
  pick:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - run: '[[ "a" == "a" ]] && echo bash-ok > shell.txt'
        shell: bash
      - run: echo sh-ok >> shell.txt
    artifacts:
      - shell.txt
EOF
cat > "$TMP/caller/.gitforest/workflows/env.yml" <<'EOF'
name: EnvPrec
on:
  workflow_dispatch:
env:
  WF_LEVEL: from-workflow
  SHARED: from-workflow
jobs:
  show:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    env:
      SHARED: from-job
      JOB_ONLY: job-here
    steps:
      - run: echo "$WF_LEVEL/$SHARED/$JOB_ONLY" > env.txt
        env:
          SHARED: from-step
    artifacts:
      - env.txt
EOF
cat > "$TMP/caller/.gitforest/workflows/sched.yml" <<'EOF'
name: Heartbeat
on:
  schedule:
    - cron: '* * * * *'
jobs:
  beat:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - run: echo heartbeat-ok > beat.txt
    artifacts:
      - beat.txt
EOF
cat > "$TMP/caller/.gitforest/workflows/downstream.yml" <<'EOF'
name: Chained
on:
  workflow_run:
    workflows: [Caller]
    types: [completed]
jobs:
  report:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 256
    disk-mb: 512
    steps:
      - run: echo chained-ok > chained.txt
    artifacts:
      - chained.txt
EOF
git -C "$TMP/caller" init -q --initial-branch=main
git -C "$TMP/caller" config user.email t@t; git -C "$TMP/caller" config user.name t
git -C "$TMP/caller" add .; git -C "$TMP/caller" commit -qm init
SHA="$(git -C "$TMP/caller" rev-parse HEAD)"

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

HOME="$TMP/home" "$BIN" init --name reuseproof >/dev/null 2>&1 || fail "init failed"
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
def post(path, payload):
    req = urllib.request.Request(base + path, data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json", "Authorization": "Bearer " + tok},
        method="POST")
    return json.load(urllib.request.urlopen(req, timeout=30))
tmp = os.environ["GF_TMP"]
post("/api/repos", {"path": tmp + "/shared", "name": "shared"})
post("/api/repos", {"path": tmp + "/caller", "name": "caller"})
d = post("/api/repos/caller/secrets", {"key": "TOKEN", "value": "tok-live-probe"})
assert d["status"] == "stored", d
print("repos registered, caller secret stored")
PYEOF

RUN_ID="$(python3 - <<'PYEOF' || fail "dispatch failed"
import json, urllib.request, os
base, tok = os.environ["GF_BASE"], os.environ["GF_TOKEN"]
req = urllib.request.Request(base + "/api/repos/caller/workflows/dispatch",
    data=json.dumps({"workflow": "ci.yml", "branch": "main",
                     "commit_sha": os.environ["GF_SHA"]}).encode(),
    headers={"Content-Type": "application/json", "Authorization": "Bearer " + tok},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1, runs
assert runs[0]["workflow"] == "Caller", runs
assert len(runs[0]["job_ids"]) == 6, runs
print(runs[0]["id"])
PYEOF
)"
[ -n "$RUN_ID" ] || fail "empty run id"
echo "cross-repo call dispatched (branch ref + tag ref + call/inner consumers + composite steps)"
export GF_RUN="$RUN_ID"
DONE=""
for _ in $(seq 1 60); do
  SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_BASE']+'/api/workflows/runs/'+os.environ['GF_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  [ "$SUM" = "6/6" ] && { DONE="yes"; break; }
  sleep 2
done
[ "$DONE" = "yes" ] || fail "run never completed (last summary: $SUM)"
echo "all six jobs completed"

python3 - <<'PYEOF' || fail "artifact proof failed"
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
greets = sorted(b for j in jobs for b in [artifact(j["id"], "greet.txt")] if b)
assert greets == [b"hi! tok-live-probe\n", b"yo! tok-live-probe\n"], greets
downs = [b for j in jobs for b in [artifact(j["id"], "downstream.txt")] if b]
assert downs == [b"hi-digest\n"], downs
inners = [b for j in jobs for b in [artifact(j["id"], "downstream-inner.txt")] if b]
assert inners == [b"hi-digest\n"], inners
prefs = [b for j in jobs for b in [artifact(j["id"], "prefixed.txt")] if b]
assert prefs == [b"prefix-local\n"], prefs
tags = [b for j in jobs for b in [artifact(j["id"], "tagline.txt")] if b]
assert tags == [b"tagline-remote\n"], tags
summ = None
for j in jobs:
    if artifact(j["id"], "downstream.txt"):
        qq = urllib.request.Request(base + "/api/compute/jobs/" + j["id"], headers=h)
        summ = json.load(urllib.request.urlopen(qq, timeout=10))["result"]["summary"]
assert summ and "## consume got hi-digest" in summ, summ
print("inputs+secrets substituted in both callees; outputs flowed via call and inner paths; composite steps inlined local and remote")
PYEOF

CHAIN_RUN=""
for _ in $(seq 1 45); do
  CHAIN_RUN="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_BASE']+'/api/workflows/runs',headers={'Authorization':'Bearer '+os.environ['GF_TOKEN']}); rs=[r for r in json.load(urllib.request.urlopen(q,timeout=10)) if r['event']=='workflow_run']; print(rs[0]['id'] if rs else '')" 2>/dev/null || true)"
  [ -n "$CHAIN_RUN" ] && break
  sleep 2
done
[ -n "$CHAIN_RUN" ] || fail "workflow_run chain never fired"
export GF_CHAIN="$CHAIN_RUN"
echo "caller completion fired the chained workflow"
CHAIN_DONE=""
for _ in $(seq 1 45); do
  SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_BASE']+'/api/workflows/runs/'+os.environ['GF_CHAIN'],headers={'Authorization':'Bearer '+os.environ['GF_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  [ "$SUM" = "1/1" ] && { CHAIN_DONE="yes"; break; }
  sleep 2
done
[ "$CHAIN_DONE" = "yes" ] || fail "chained run never completed (last: $SUM)"
CART="$(python3 -c "import json,urllib.request,os; b=os.environ['GF_BASE']; h={'Authorization':'Bearer '+os.environ['GF_TOKEN']}; q=urllib.request.Request(b+'/api/workflows/runs/'+os.environ['GF_CHAIN'],headers=h); j=json.load(urllib.request.urlopen(q,timeout=10))['jobs'][0]['id']; print(urllib.request.urlopen(urllib.request.Request(b+'/api/compute/jobs/'+j+'/artifacts/chained.txt',headers=h),timeout=30).read().decode())")"
[ "$CART" = "chained-ok" ] || fail "chained artifact mismatch: $CART"
echo "chained run completed with artifact: $CART"

SCHED_RUN=""
for _ in $(seq 1 60); do
  SCHED_RUN="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_BASE']+'/api/workflows/runs',headers={'Authorization':'Bearer '+os.environ['GF_TOKEN']}); rs=[r for r in json.load(urllib.request.urlopen(q,timeout=10)) if r['event']=='schedule']; print(rs[0]['id'] if rs else '')" 2>/dev/null || true)"
  [ -n "$SCHED_RUN" ] && break
  sleep 2
done
[ -n "$SCHED_RUN" ] || fail "scheduled run never fired"
export GF_SCHED="$SCHED_RUN"
echo "scheduler fired the heartbeat workflow"
SCHED_DONE=""
for _ in $(seq 1 45); do
  SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_BASE']+'/api/workflows/runs/'+os.environ['GF_SCHED'],headers={'Authorization':'Bearer '+os.environ['GF_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  [ "$SUM" = "1/1" ] && { SCHED_DONE="yes"; break; }
  sleep 2
done
[ "$SCHED_DONE" = "yes" ] || fail "scheduled run never completed (last: $SUM)"
BART="$(python3 -c "import json,urllib.request,os; b=os.environ['GF_BASE']; h={'Authorization':'Bearer '+os.environ['GF_TOKEN']}; q=urllib.request.Request(b+'/api/workflows/runs/'+os.environ['GF_SCHED'],headers=h); j=json.load(urllib.request.urlopen(q,timeout=10))['jobs'][0]['id']; print(urllib.request.urlopen(urllib.request.Request(b+'/api/compute/jobs/'+j+'/artifacts/beat.txt',headers=h),timeout=30).read().decode())")"
[ "$BART" = "heartbeat-ok" ] || fail "scheduled artifact mismatch: $BART"
echo "scheduled run completed with artifact: $BART"

ENV_RUN="$(python3 - <<'PYEOF' || fail "env dispatch failed"
import json, urllib.request, os
base, tok = os.environ["GF_BASE"], os.environ["GF_TOKEN"]
req = urllib.request.Request(base + "/api/repos/caller/workflows/dispatch",
    data=json.dumps({"workflow": "env.yml",
                     "commit_sha": os.environ["GF_SHA"]}).encode(),
    headers={"Content-Type": "application/json", "Authorization": "Bearer " + tok},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1, runs
print(runs[0]["id"])
PYEOF
)"
[ -n "$ENV_RUN" ] || fail "empty env run id"
export GF_ENV_RUN="$ENV_RUN"
ENV_DONE=""
for _ in $(seq 1 45); do
  SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_BASE']+'/api/workflows/runs/'+os.environ['GF_ENV_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  [ "$SUM" = "1/1" ] && { ENV_DONE="yes"; break; }
  sleep 2
done
[ "$ENV_DONE" = "yes" ] || fail "env run never completed (last: $SUM)"
EART="$(python3 -c "import json,urllib.request,os; b=os.environ['GF_BASE']; h={'Authorization':'Bearer '+os.environ['GF_TOKEN']}; q=urllib.request.Request(b+'/api/workflows/runs/'+os.environ['GF_ENV_RUN'],headers=h); j=json.load(urllib.request.urlopen(q,timeout=10))['jobs'][0]['id']; print(urllib.request.urlopen(urllib.request.Request(b+'/api/compute/jobs/'+j+'/artifacts/env.txt',headers=h),timeout=30).read().decode())")"
[ "$EART" = "from-workflow/from-step/job-here" ] || fail "env precedence mismatch: $EART"
echo "workflow/job/step env precedence proven: $EART"

SHELL_RUN="$(python3 - <<'PYEOF' || fail "shell dispatch failed"
import json, urllib.request, os
base, tok = os.environ["GF_BASE"], os.environ["GF_TOKEN"]
req = urllib.request.Request(base + "/api/repos/caller/workflows/dispatch",
    data=json.dumps({"workflow": "shell.yml",
                     "commit_sha": os.environ["GF_SHA"]}).encode(),
    headers={"Content-Type": "application/json", "Authorization": "Bearer " + tok},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1, runs
print(runs[0]["id"])
PYEOF
)"
[ -n "$SHELL_RUN" ] || fail "empty shell run id"
export GF_SHELL_RUN="$SHELL_RUN"
SHELL_DONE=""
for _ in $(seq 1 45); do
  SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_BASE']+'/api/workflows/runs/'+os.environ['GF_SHELL_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed'])+'/'+str(s['total']))" 2>/dev/null || true)"
  [ "$SUM" = "1/1" ] && { SHELL_DONE="yes"; break; }
  sleep 2
done
[ "$SHELL_DONE" = "yes" ] || fail "shell run never completed (last: $SUM)"
SART="$(python3 -c "import json,urllib.request,os; b=os.environ['GF_BASE']; h={'Authorization':'Bearer '+os.environ['GF_TOKEN']}; q=urllib.request.Request(b+'/api/workflows/runs/'+os.environ['GF_SHELL_RUN'],headers=h); j=json.load(urllib.request.urlopen(q,timeout=10))['jobs'][0]['id']; print(urllib.request.urlopen(urllib.request.Request(b+'/api/compute/jobs/'+j+'/artifacts/shell.txt',headers=h),timeout=30).read().decode())")"
[ "$SART" = "bash-ok
sh-ok" ] || fail "shell artifact mismatch: $SART"
echo "bash step ran under bash, sh step under sh"

FAIL_RUN="$(python3 - <<'PYEOF' || fail "fail dispatch failed"
import json, urllib.request, os
base, tok = os.environ["GF_BASE"], os.environ["GF_TOKEN"]
req = urllib.request.Request(base + "/api/repos/caller/workflows/dispatch",
    data=json.dumps({"workflow": "fail.yml",
                     "commit_sha": os.environ["GF_SHA"]}).encode(),
    headers={"Content-Type": "application/json", "Authorization": "Bearer " + tok},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert len(runs) == 1, runs
print(runs[0]["id"])
PYEOF
)"
[ -n "$FAIL_RUN" ] || fail "empty fail run id"
export GF_FAIL_RUN="$FAIL_RUN"
FAIL_SETTLED=""
for _ in $(seq 1 45); do
  SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_BASE']+'/api/workflows/runs/'+os.environ['GF_FAIL_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed']+s['failed'])+'/'+str(s['total'])+'/'+str(s['failed']))" 2>/dev/null || true)"
  [ "$SUM" = "2/2/1" ] && { FAIL_SETTLED="yes"; break; }
  sleep 2
done
[ "$FAIL_SETTLED" = "yes" ] || fail "fail run never settled (last: $SUM)"
RERUN_RUN="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_BASE']+'/api/workflows/runs/'+os.environ['GF_FAIL_RUN']+'/rerun',headers={'Authorization':'Bearer '+os.environ['GF_TOKEN']},method='POST'); rs=json.load(urllib.request.urlopen(q,timeout=30))['runs']; assert len(rs)==1 and len(rs[0]['job_ids'])==1 and rs[0]['rerun_of']==os.environ['GF_FAIL_RUN'], rs; print(rs[0]['id'])")"
[ -n "$RERUN_RUN" ] || fail "rerun produced no run"
export GF_RERUN_RUN="$RERUN_RUN"
RERUN_SETTLED=""
for _ in $(seq 1 45); do
  SUM="$(python3 -c "import json,urllib.request,os; q=urllib.request.Request(os.environ['GF_BASE']+'/api/workflows/runs/'+os.environ['GF_RERUN_RUN'],headers={'Authorization':'Bearer '+os.environ['GF_TOKEN']}); s=json.load(urllib.request.urlopen(q,timeout=10))['summary']; print(str(s['completed']+s['failed'])+'/'+str(s['total'])+'/'+str(s['failed']))" 2>/dev/null || true)"
  [ "$SUM" = "1/1/1" ] && { RERUN_SETTLED="yes"; break; }
  sleep 2
done
[ "$RERUN_SETTLED" = "yes" ] || fail "rerun never settled (last: $SUM)"
echo "failed job reran as a linked run and settled"
HOME="$TMP/home" "$BIN" --config "$TMP/node.toml" workflow rerun "$GF_RUN" | grep -q "No failed jobs" || fail "rerun CLI on clean run misbehaved"
echo "rerun CLI reports clean runs correctly"

python3 - <<'PYEOF' || fail "negative dispatch failed"
import json, urllib.request, os
base, tok = os.environ["GF_BASE"], os.environ["GF_TOKEN"]
req = urllib.request.Request(base + "/api/repos/caller/workflows/dispatch",
    data=json.dumps({"workflow": "bad.yml",
                     "commit_sha": os.environ["GF_SHA"]}).encode(),
    headers={"Content-Type": "application/json", "Authorization": "Bearer " + tok},
    method="POST")
runs = json.load(urllib.request.urlopen(req, timeout=30))["runs"]
assert runs == [], runs
print("unknown-repo call produced no runs")
PYEOF

echo "REUSABLE-REMOTE PASS"
