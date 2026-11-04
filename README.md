<p align="center">
  <img src="https://raw.githubusercontent.com/NPC-Worldwide/gitforest/main/desktop/gitforest.png" alt="GitForest logo" width="200">
</p>

<h1 align="center">GitForest</h1>

<p align="center">
  <strong>Decentralized P2P git hosting.</strong>
</p>

<p align="center">
  <a href="https://github.com/NPC-Worldwide/gitforest/blob/main/LICENSE"><img src="https://img.shields.io/github/license/NPC-Worldwide/gitforest" alt="License"></a>
  <a href="https://github.com/NPC-Worldwide/gitforest/releases"><img src="https://img.shields.io/github/v/release/NPC-Worldwide/gitforest?include_prereleases" alt="Release"></a>
  <a href="https://github.com/NPC-Worldwide/gitforest/actions/workflows/ci.yml"><img src="https://github.com/NPC-Worldwide/gitforest/actions/workflows/ci.yml/badge.svg" alt="Build Status"></a>
</p>

<p align="center">
  <a href="https://github.com/NPC-Worldwide/gitforest/releases"><strong>Download for Linux and macOS</strong></a>
</p>

---

Decentralized P2P git hosting. Users host their own repos, share them over a BitTorrent-style DHT network, and collaborate directly peer-to-peer. Seeder/leecher counts serve as organic quality signals. No company controls the network.

Git is already a decentralized VCS — GitForest just gives it a proper P2P hosting layer.

## Architecture

```
┌──────────────────────────────────────────────────┐
│                 User's Machine                    │
│                                                   │
│  git clone gitforest://<peer_id>/<repo>           │
│       │                                           │
│       ▼                                           │
│  git-remote-gitforest  ──▶  gitforest daemon      │
│  (custom git transport)     │                     │
│                             ├── DHT Node (libp2p) │
│                             ├── Git Protocol      │
│                             ├── Compute Runner    │
│                             ├── HTTP API (axum)   │
│                             └── Web UI            │
│                                  │                │
│                     http://127.0.0.1:7878         │
└──────────────────────────────────────────────────┘
                      │
           P2P Network (DHT + mDNS)
                      │
┌─────────────────────┴────────────────────────────┐
│             Other Peers (same stack)              │
└──────────────────────────────────────────────────┘
```

## Crates

| Crate | Description |
|-------|-------------|
| `gt-core` | Identity (Ed25519), config, protocol message types, shared types |
| `gt-dht` | DHT node using libp2p Kademlia + mDNS for repo and compute discovery |
| `gt-transport` | Git bundle creation, git smart protocol bridge, torrent-style piece verification |
| `gt-collab` | Collaboration: signed patches, fork + merge requests, access control lists |
| `gt-compute` | P2P compute marketplace: CI/CD job execution, Docker runner, crypto payments |
| `gt-lfs` | Large file storage: content-addressed blobs, adaptive chunking, collection manifests for ML models and datasets |
| `gt-daemon` | Main binaries (`gitforest`, `gtfo`): CLI, HTTP API, network management |
| `gt-remote` | Git remote helper binary (`git-remote-gitforest`) for `gitforest://` URLs |
| `gt-web` | Embedded web dashboard (static HTML/JS served by the daemon) |
| `gt-mcp` | MCP server binary (`gitforest-mcp`) for AI agents over stdio |

## Prerequisites

- **Rust** (stable, 2021 edition) — install via [rustup](https://rustup.rs/)
- **Git** — for git operations and the remote helper
- **Docker** — only if compute workers run in Docker mode; `worker-start --local` runs jobs directly on the host instead
- No system OpenSSL or libgit2 needed — both are vendored

## Building

```sh
# Clone the repo
git clone https://github.com/NPC-Worldwide/gitforest.git
cd git-forest

# Build everything
cargo build --release

# The four binaries are:
#   target/release/gitforest              (daemon + CLI)
#   target/release/gtfo                   (shorthand alias for gitforest)
#   target/release/git-remote-gitforest   (git remote helper, invoked by git)
#   target/release/gitforest-mcp          (MCP server for AI agents, stdio)
```

To make `git clone gitforest://...` work, the `git-remote-gitforest` binary must be on your `$PATH`:

```sh
# Option 1: symlink into a PATH directory
ln -s $(pwd)/target/release/git-remote-gitforest ~/.local/bin/

# Option 2: add target/release to PATH
export PATH="$PATH:$(pwd)/target/release"
```

## Getting Started

### 1. Initialize your identity

```sh
gitforest init
```

This generates an Ed25519 keypair at `~/.config/gitforest/identity.key`. Your public key IS your identity on the network — no accounts, no passwords.

### 2. Add a repo to the network

```sh
gitforest add /path/to/your/git/repo --name myproject --description "My cool project"
```

This registers the repo in your config at `~/.config/gitforest/config.toml`.

### 3. Start the daemon

```sh
gitforest daemon
```

This starts:
- **DHT node** on port 9418 (announces your repos, discovers peers via mDNS on LAN)
- **Git protocol server** on port 9419 (for direct P2P git fetch/push)
- **HTTP API + Web UI** at http://127.0.0.1:7878

### 4. Clone from the network

On another machine (or the same one for testing):

```sh
git clone gitforest://<peer_id>/<repo_name>
```

## CLI Reference

```
gitforest [--config P] [--identity P]           Global flags (all commands)
gitforest init [--name N]                       Create a new identity
gitforest id                                    Show your peer ID
gitforest name set <name> | name show           Set or show your display name
gitforest add <path> [-n NAME] [-d DESC]        Add a local git repo to the network
gitforest list                                  List hosted repos
gitforest remove <name>                         Remove a repo from the network
gitforest status                                Show network status
gitforest daemon [--foreground] [--stop]        Start (default fg) or stop the daemon
gitforest peer add <multiaddr>                  Add a bootstrap peer
gitforest peer list | peer info                 List peers / show own conn info
gitforest peer remove <addr>                    Remove a bootstrap peer
gitforest peer alias <tag> <peer-id>            Save a peer name tag
gitforest acl grant <repo> <peer> [ref]         Grant push access (ref default refs/heads/*)
gitforest acl revoke <repo> <peer>              Revoke push access
gitforest acl show <repo>                       Show a repo's ACL
gitforest mr create <repo> --target-peer P --source-repo R --source-branch B [--target-branch T] -t TITLE [-d DESC]
gitforest mr list <repo> | mr info <repo> <id>  List / show merge requests

gitforest blob add <path>                       Import a file into the blob store
gitforest blob list                             List all stored blobs
gitforest blob info <id>                        Show blob details (size, pieces, path)
gitforest blob remove <id>                      Remove a blob from the store
gitforest blob pointer <id>                     Generate a pointer file for a blob

gitforest collection create --name N [-d DESC] [--files id:path:size,...] [--metadata k=v,...]
gitforest collection list                       List collections
gitforest collection info <id>                  Show collection details

gitforest compute submit <repo> [--commit C] [--max-sats N] [--chain CHAIN]
gitforest compute status <id>                   Check job status
gitforest compute cancel <id>                   Cancel one of your jobs
gitforest compute approve <id> | reject <id>    Approve/reject env-gated job
gitforest compute download-artifact <id> <name> [--out FILE]
gitforest compute worker-start [--cpu N] [--ram MB] [--rate R] [--local]
gitforest compute worker-stop                   Stop offering compute
gitforest compute offers                        List available workers
gitforest compute wallet <chain> <addr>         Configure payment wallet
gitforest compute credit                        Show mutual-credit balance
gitforest compute evm-keystore-create --path P --password-file F (--key-file K | --generate)
gitforest compute evm-keystore-rotate --from OLD --path NEW --password-file F [--new-password-file NF]

gitforest workflow list <repo>                  List workflows in a repo
gitforest workflow dispatch <repo> [--workflow W] [--branch B | --tag T] [--commit C] [--input K=V ...]
gitforest workflow runs                         List workflow runs
gitforest workflow show <run>                   Show one run with job status
gitforest workflow rerun <run>                  Re-run failed jobs as a new run

gitforest model publish <collection>            Publish a collection as an LLM model
gitforest model search [-q Q] [--architecture A] [--quantization Q] [--format F] [--min-params N] [--max-params N]
gitforest model info <id>                       Show model details and shards
gitforest model pull <id> [--timeout-secs N]    Pull a model and its weight blobs
gitforest model acl <id> [--action show|restrict|open|grant|revoke] [--peer P]
gitforest cache save --key K --path P           Archive a path into the workflow cache
gitforest cache restore --key K --path P [--restore-keys PREFIXES]
gitforest cache delete --key K                  Delete a cache key

gitforest llm run --model <id>                  Run one inference, print completion
  --prompt <text> | --prompt-file <f>           Prompt source
  --max-tokens <n> --temperature <t>            Sampling controls
  --system <text> --timeout <secs> --stream     System prompt, backend timeout, streaming

gitforest secrets <repo> set <key> [value]      Store a repo secret (or --from-file F)
gitforest secrets <repo> list                   List secret names (never values)
gitforest secrets <repo> delete <key>           Delete a repo secret

gtfo <same commands>                            Shorthand alias for gitforest
```

## Web UI

When the daemon is running, open http://127.0.0.1:7878 for:

- **Repos** — repo cards with seeding status and click-to-copy clone URLs; click through for commits, branches, and access control (grant/revoke)
- **Blobs** — stored blobs with seeder/leecher counts, collections (models, datasets) with file manifests, live transfer progress, and the shared workflow-cache directory (local + peer-advertised keys)
- **Models** — search local and DHT-known models, pull weights, test inference with a prompt, manage per-model access (open/restricted, per-peer grants)
- **Compute** — submit CI jobs from a form (repo, commit, max sats, chain, YAML prefilled from `.gitforest-ci.yml`), view job statuses with logs, approve/reject held environment deployments, cancel jobs, browse workers, monitor payment channels
- **Workflows** — workflow runs with per-status job counts and per-job status detail
- **Network** — connected-peer count, repos hosted/seeding counts, P2P port
- **Proposals** — incoming merge requests and patches across all repos
- **Identity** — your peer ID and `gitforest://` repo address format

## Desktop app

`desktop/` is the Electron desktop client (`gitforest-desktop`). The packaged app
bundles the `gitforest`, `git-remote-gitforest`, and `gtfo` binaries and spawns
`gitforest daemon` itself on launch (attaching to an already-running daemon if
one answers on port 7878). Its six pages are Repos, Network (peer list,
transfers), Analytics (peer history, storage), Compute (job submit, worker
offers, local worker control), Profile (display name, identity backup), and
Settings (daemon start/stop/restart, log tail, updates).

The desktop UI is covered by an automated smoke test (`./gui-test/run.sh`,
driven by `gui-test/smoke.js`): it walks all six pages, publishes a display
name, submits a compute job through the real UI, and fails on any console
error.

## Compute Marketplace

GitForest includes a decentralized CI/CD system. Peers can offer their compute resources and get paid for running builds and tests — in crypto (Lightning, EVM L2) or network mutual credit.

### As a job submitter

1. Create a `.gitforest-ci.yml` in your repo root:

```yaml
name: Build and Test
docker_image: rust:1.75
timeout_secs: 600
steps:
  - name: Build
    run: cargo build --release
  - name: Test
    run: cargo test
artifacts:
  - target/release/myapp
```

2. Submit the job:

```sh
gitforest compute submit myrepo --max-sats 10000
```

### As a compute worker

1. Make sure Docker is installed and running.

2. Configure your wallet:

```sh
gitforest compute wallet lightning lnbc1youraddress...
```

3. Start offering compute:

```sh
gitforest compute worker-start --cpu 4 --ram 8192 --rate 1000
# Add --local to run job shells directly on the host instead of Docker
```

4. Your compute offer is announced on the DHT immediately — no restart needed.

### Supported payment chains

- **Mutual credit** (default) — earn credit owed by the network, spend it with anyone; no crypto required
- **Bitcoin Lightning** — hold invoices, settled per job on completion
- **Ethereum L2** — Arbitrum, Optimism, Base, Polygon; on-chain escrow with receipt confirmation
- **Solana** — planned, no hold-invoice equivalent yet

Payment backends are pluggable via the `PaymentBackend` trait. See `specs/003-payment-settlement/spec.md` for the full design.

## Large File Storage (LFS)

GitForest includes decentralized large file storage for sharing ML model weights, datasets, and other large artifacts over the P2P network. Think Git LFS, but fully decentralized with BitTorrent-style distribution.

### How it works

1. **Content-addressed blobs** — Files are hashed (SHA-256) and stored outside git. The hash IS the file's identity on the network.
2. **Adaptive chunking** — Files are split into pieces for parallel download and verification:
   - < 100 MB: 256 KB pieces
   - 100 MB – 1 GB: 1 MB pieces
   - \>= 1 GB: 4 MB pieces
3. **Pointer files** — Small text files in git that reference the blob hash:
   ```
   version gitforest-lfs/1
   oid sha256:e3b0c44298fc1c149afbf4c8996fb924...
   size 4000000000
   ```
4. **DHT discovery** — Blob announcements are stored on the DHT so peers can find who seeds which files.
5. **Collection manifests** — Group related blobs into signed collections (e.g., a model's weights + tokenizer + config).
6. **Swarm fetch** — Each tick pulls up to 64 missing pieces concurrently, spread across known seeders with per-piece fallback to the next seeder; every piece is SHA-256-verified before it counts, so a download can reassemble from seeders that each hold only part of the file. Downloaders announce partial progress on the DHT and serve the verified pieces they already hold, so slow or partial peers still contribute to the swarm.

### GitHub-style workflows

Repos can ship Actions-style workflows in `.github/workflows/*.yml`
(or `.gitforest/workflows/`). Pushes and merge requests trigger matching
workflows automatically; jobs with `needs:` wait for their dependencies
and `strategy.matrix` fans out into parallel jobs:

```yaml
name: CI
on:
  push:
    branches: [main]
  pull_request:
  workflow_dispatch:
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: cargo build --release
  test:
    needs: build
    strategy:
      matrix:
        rust: [stable, beta]
        exclude:
          - rust: beta
    steps:
      - run: cargo test --toolchain ${{ matrix.rust }}
  notify:
    needs: [build, test]
    if: failure()
    runs-on: ubuntu-latest
    steps:
      - run: echo something failed
```

`push:` also accepts `tags:` (with `tags-ignore:`) for release-style
flows: dispatching with a tag runs workflows whose tag globs match, and
a tags-only `push:` filter stays quiet on branch pushes, matching
Actions. Pushing a tag to a hosted repo fires matching workflows
automatically, and `POST /api/repos/{name}/workflows/dispatch` takes
an optional `tag` alongside `branch` (a tag wins when both are given).

Manual dispatches take declared inputs: declare them under
`workflow_dispatch.inputs` (each with `required:` and `default:`),
reference them as `${{ inputs.NAME }}` in steps, names, and env, and
pass values in the dispatch `inputs` object (or repeat
`--input KEY=VALUE` on `gitforest workflow dispatch`). Missing
required inputs, undeclared inputs, and references to undeclared
inputs all skip the workflow instead of running it half-configured,
and dispatch values flow through into reusable-call `with:` entries.
Branch and tag filters are globs: `*` spans any run of characters
(including `/`, except in a trailing `/*` single-segment match), `**`
matches everything, and `prefix/**` matches a subtree.

Jobs accept `if: success()`, `failure()`, `always()`, or
`cancelled()` to gate on their `needs:` outcomes, and matrices support
`include:`/`exclude:` rules. `cancelled()` is job-level only: running
scripts have no mid-run cancel signal, so cleanup jobs watch for
cancelled needs while steps reject the condition at parse time.
Individual steps take the other `if:` conditions, gated on earlier steps in
the job; each step runs fail-fast in its own shell, sharing the workspace
but not shell variables. Steps run in a `working-directory:` when set
(relative paths only, no `..`), defaulted per job with
`defaults.run.working-directory:`; an explicit step value wins. Steps and
jobs take `continue-on-error: true` to
tolerate failure: a forgiven step logs its exit code and lets later steps
run, and a forgiven job still reports failed but releases its default
`needs:` dependents (while `failure()` handlers still see the failure).
Matrix fan-out is capped with `strategy.max-parallel`, which holds
siblings queued until fewer than that many are active.

Dependencies and build output persist across runs with the
`gitforest/cache` and `gitforest/cache-save` actions: restore takes
`path:`, `key:`, and optional comma-separated `restore-keys:` prefixes,
sets a `cache-hit` step output (true only on an exact key match), and
save stores the path under the key (existing keys are kept, archives
cap at 256 MB). Entries live in the daemon's blob store with a
persistent key index. Steps reach the daemon through the injected
`GF_API_URL`/`GF_JOB_TOKEN` pair (scoped to the job's `permissions:`),
with `GITFOREST_CONFIG` and a `PATH` containing the daemon's own
directory as backup; cache steps shell out to the `gitforest` CLI, so
they need local execution mode (container images do not ship the
binary). Saves upload raw bytes (`POST /api/blobs/upload`) and then
reference the blob id, so remote workers save the same way local ones
do. Saved keys are also announced on the DHT with a signed
advertisement, so a restore on another daemon fetches the archive from
a seeding peer (`"source": "peer"`) instead of rebuilding; exact keys
win, prefix `restore-keys:` prefer the longest match across local and
remote keys, and stale advertisements expire after five minutes.

Jobs declare the worker they need with `cpu:`, `ram-mb:`, and `disk-mb:`
(defaults 2 cores, 4096 MB RAM, 10240 MB disk; `ram_mb`/`disk_mb`
spellings also work). Only workers meeting every requirement pick a job
up, so small jobs on small workers stay schedulable while heavy jobs
wait for capable ones. `gpu: true` and `network: true` add capability
gates on top; workers opt into GPU work with `gpu_enabled = true` in
`[compute]`, and also probe for GPUs at startup unless
`gpu_auto_detect = false` is set (`nvidia-smi -L` for NVIDIA,
DRM sysfs vendor scan for AMD, `system_profiler` on macOS), so
GPU machines advertise automatically. A `gitforest/llm` step with `gpu: true` in `with:`
marks its whole job as GPU-required so inference lands on a GPU
worker.

A job fails when any step exits nonzero; its signed result (exit code,
logs, outputs, artifacts) is still stored and served so failures stay
inspectable. Matrix strategies cancel their surviving siblings on the
first failure unless `fail-fast: false` is set, matching Actions —
queued and assigned siblings are cancelled outright (assigned workers
are notified and escrows settle back), while already-running jobs run
to completion.

Workflows and jobs take a `concurrency:` key, either a plain group
name or a mapping with `group:` plus `cancel-in-progress: true`. Group
names substitute `matrix:`, `inputs:`, and the `github:` context
(`event_name`, `ref`, `ref_name`, `sha`, `repository`, `workflow`,
`run_id`, `head_ref`); branchless dispatches resolve the ref against
the default branch `main`. Queued jobs wait while another job holds
their group, and a new run with `cancel-in-progress: true` retires
queued or assigned prior jobs in the same group — running jobs, which
cannot be preempted, run to completion. A workflow-level entry acts as
the default for jobs that do not set their own.

Jobs name a deployment target with `environment:`, either a plain name
or a mapping with `name:` and `url:`. Names substitute `matrix:`,
`inputs:`, and the `github:` context. Environments listed in the
daemon's `protected_environments` compute setting hold their jobs out
of the claim queue until someone approves them with
`POST /api/compute/jobs/{id}/approve` (or `gitforest compute approve`);
unlisted environments run immediately, and cancelling a held job
rejects it.

Jobs declare files to keep with an `artifacts:` list of workspace
paths (up to 16 files, 64 MB each). Executors persist the bytes into
the content-addressed blob store and pin `(name, blob id, size)` in
the signed result, so artifacts travel to the submitter with the
result message and download with `gitforest compute download-artifact`.
Workers announce artifact blobs on the DHT when jobs finish, and
downloads fetch missing bytes over P2P automatically (up to 90s)
instead of failing. `gitforest compute status` lists each artifact
with its size and whether its bytes are already local.

Jobs can carry `secrets:` alongside `env:`. Secrets reach the job as
environment variables, referenced as `${{ secrets.NAME }}` (unknown names
expand to empty), and every value is redacted to `***` in captured logs
before results are hashed and signed — so replay verification also proves
the redaction happened. Workers still see secret values by construction,
so only run secret-bearing jobs on workers you trust.

Secrets worth keeping out of git live in the repo secrets store instead
of workflow files: `gitforest secrets <repo> set TOKEN --from-file token.txt`
writes them to an owner-only file under the data dir, sealed with
XChaCha20-Poly1305 under a key derived from the daemon identity (repo
name bound as associated data, so files cannot move between repos).
Plaintext files from older versions migrate to the envelope on first
read. Set `GF_SECRETS_PASSPHRASE` on the daemon to seal with a v2
envelope instead, keyed by PBKDF2-SHA256 (210k rounds, fresh 32-byte
salt per write) rather than the identity key — v1 files upgrade on
first read, and removing the passphrase leaves v2 files unreadable
until it is restored, so keep it in the process environment, never in
a file next to the data. The API only ever lists names, and dispatch
merges stored secrets over workflow-declared ones before jobs expand.

Workflows shared between jobs live in reusable files. A file with an
`on: workflow_call` trigger declares `inputs:` (with `required:` and
`default:`) and `secrets:`, and callers inline it with `uses:`:

```yaml
# .github/workflows/greet.yml
name: Greet
on:
  workflow_call:
    inputs:
      greeting:
        required: true
jobs:
  say:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ inputs.greeting }}
```

```yaml
jobs:
  hello:
    uses: ./greet.yml
    with:
      greeting: hi
```

Called jobs are prefixed (`hello/say`), keep their internal `needs:`
relationships, and inherit the caller's matrix and entry `needs:`. Calls
nest, and cycles or missing required inputs fail the workflow with an
error instead of running.

Calls also reach across repos: `uses: other-repo/.github/workflows/build.yml@v1`
checks out the callee at the pinned ref and inlines it the same way.
Callers read callee outputs two ways — `needs.<call>.outputs.<name>`
for the call's declared outputs, or `needs.<call>/<job>.outputs.<key>`
for one inner job's step outputs — and a `needs: [<call>]` edge fans
out to every inlined job. Matrices fan out over calls too.

Single-file composite actions (`runs.using: composite`) inline shared
step lists under a `uses:` step with declared `inputs:`, resolved from
local paths or `repo/path@ref` remotes the same way as reusable calls.

Beyond `push`, `pull_request`, and `workflow_dispatch`, workflows trigger
on `workflow_run` (chaining after another workflow's conclusion) and on
`schedule:` cron expressions, evaluated by the daemon's ticker once per
minute slot. Failed jobs re-run as a linked run with
`POST /api/workflows/runs/{id}/rerun` (or `gitforest workflow rerun`),
which clones failed jobs plus never-ran cancelled dependents under a
new run id.

Jobs declare API access with GitHub-style `permissions:` (top-level
defaults, per-job overrides, `read-all`/`write-all` shorthands) over
three scopes:

```yaml
permissions:
  contents: read
jobs:
  deploy:
    permissions:
      contents: write
      workflows: read
```

Each job gets a daemon-minted bearer token (`GF_JOB_TOKEN`, usable
against `GF_API_URL`) scoped to its effective permissions; reusable
calls pass the caller's grant down and the callee can only narrow it.
Steps and the in-step `gitforest` CLI authenticate with it, so a job
with `contents: read` can download artifacts but gets `403` on
workflow dispatch. Steps also append markdown to `$GF_SUMMARY` for a
rendered per-job summary, collected alongside outputs and artifacts.

Steps publish outputs by appending `KEY=value` lines (or `KEY<<EOF`
heredoc blocks) to the `$GF_OUTPUTS` file. Downstream jobs read them as
`${{ needs.<job-id>.outputs.<KEY> }}` once their `needs:` complete —
missing keys from a finished need expand to empty, while unknown job
names are left in place so typos stay visible. Matrix jobs are addressed
by expanded id (`test-0`, `test-1`). Outputs travel in signed results,
are covered by replay verification, and any secret values in them are
scrubbed to `***` before signing.

Workflows can also run inference against a pulled P2P model with the
built-in `gitforest/llm` action. The executing worker resolves the model
from its own blob store and reports the completion as the step's
`completion` output:

```yaml
jobs:
  summarize:
    runs-on: ubuntu-latest
    steps:
      - uses: gitforest/llm
        with:
          model: tiny-llama
          prompt: Summarize the changes on this branch
          max-tokens: "256"
          temperature: "0.2"
  report:
    needs: summarize
    runs-on: ubuntu-latest
    steps:
      - run: echo "${{ needs.summarize.outputs.completion }}"
```

The runner shells out to a llama.cpp-style backend (`llama-cli` by
default, overridable per node with `llm_backend` in config.toml or the
`GF_LLM_BACKEND` environment variable). `gitforest llm run` exposes the
same path outside workflows for scripts and debugging. Long generations
can stream instead of blocking: `POST /api/models/{id}/infer/stream`
returns `text/event-stream` with `{"chunk": ...}` events, a terminal
`{"done": {"truncated": ..., "elapsed_ms": ...}}` event, and
`{"error": ...}` on failure; `gitforest llm run --stream` prints each
chunk as it arrives.

Inference does not need local weights: when the model is not stored
locally but a DHT seeder is known and both daemons share a
`cluster_token`, the blocking `/infer` endpoint proxies the request
to the holder and relays the completion annotated with `proxied: true`
and `served_by`; the streaming endpoint relays the holder's SSE
bytes the same way. Peer-authenticated requests never re-proxy (one
hop max). Treat the cluster token as full mutual trust — holders
spend their own compute on your prompts.

Models are open to the cluster by default, and can be restricted per
peer: `POST /api/models/{id}/acl` with `{"action": "restrict"}`
locks a model to its owner, `grant`/`revoke` manage peer access, and
`open` lifts the restriction (grants persist in config across
restarts). Peers authenticate with signed `X-GF-Peer` / `X-GF-Timestamp`
/ `X-GF-Signature` headers over the method, path, timestamp, and
SHA-256 of the request body, which every daemon attaches to its
outbound peer calls, so proxied prompts cannot be swapped in flight.
The gate covers
inference, manifests, blob metadata, piece serving, and whole-blob
downloads; model discovery stays open.

```sh
gitforest workflow list myrepo
gitforest workflow dispatch myrepo --branch main
gitforest workflow runs
gitforest workflow show <run-id>
gitforest compute download-artifact <job-id> <name> [--out <file>]
```

### Sharing a model

Publish a collection as a discoverable LLM model once its metadata carries
`architecture`, `parameter_count`, and `format`:

```sh
gitforest model publish <collection-id>
gitforest model search --architecture llama --min-params 1B
gitforest model info <manifest-id>
```

```sh
# The API needs your local auth token (the daemon writes it on startup)
TOKEN=$(cat ~/.local/share/gitforest/api.token)

# Add a model file to the blob store
curl -H "Authorization: Bearer $TOKEN" -X POST http://127.0.0.1:7878/api/blobs -d '{"path": "/path/to/model.safetensors"}'
# Returns: {"blob_id": "abc123...", "size": 4000000000, "piece_count": 954}

# Create a collection manifest grouping model files
curl -H "Authorization: Bearer $TOKEN" -X POST http://127.0.0.1:7878/api/collections -d '{
  "name": "llama-7b-q4",
  "description": "Quantized LLaMA 7B model",
  "files": [
    {"blob_id": "abc123...", "path": "model.safetensors", "size": 4000000000},
    {"blob_id": "def456...", "path": "tokenizer.json", "size": 2000000}
  ],
  "metadata": {"architecture": "llama", "quantization": "q4_0"}
}'
```

### Storage layout

Blobs are stored content-addressed on disk:
```
~/.local/share/gitforest/blobs/<first-2-hex>/<next-2-hex>/<full-sha256-hex>
```

## Collaboration

Three modes of collaboration, all cryptographically signed:

1. **Signed Patches** — publish a signed patch set to the network targeting a repo. Fully decentralized, no direct connection to the owner needed.

2. **Fork + Merge Request** — clone a repo, make changes, publish your fork, send a signed merge request to the owner.

3. **Shared Access (ACL)** — repo owner grants push access to specific peer IDs with ref-pattern restrictions (e.g., `refs/heads/*` but not tags).

## Security Model

- **Identity**: Ed25519 keypairs. Public key = identity. No accounts, no passwords.
- **Repo ownership**: Only the creating keypair can modify repo metadata/ACL.
- **Push auth**: All pushes must be signed. Daemon verifies against repo ACL.
- **Transport encryption**: All P2P connections encrypted via Noise protocol (libp2p).
- **Content integrity**: Git SHA hashes verify content. Torrent piece hashes verify transfer.
- **Compute sandboxing**: Jobs run in Docker containers with CPU/RAM/network limits and timeouts — unless the worker runs `worker-start --local` / `execution_mode = "local"`, which executes job shells directly on the host without container isolation.

## Configuration

Config lives at `~/.config/gitforest/config.toml`:

```toml
data_dir = "/home/user/.local/share/gitforest"
p2p_port = 9418
api_port = 7878
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
# Optional top-level: bootstrap_peers, advertise_addr, cluster_token,
# peer_names, llm_backend, p2p_listen, enable_mdns (default true)

[[repos]]
name = "myrepo"
path = "/home/user/myrepo"
owner = "<64-hex-char peer ID>; `gitforest add` fills this in"
# description = "optional"
seeding = true

[compute]
worker_enabled = true
max_cpu_cores = 4
max_ram_mb = 8192
max_disk_mb = 51200
docker_socket = "/var/run/docker.sock"
hourly_rate_sats = 1000
accepted_chains = ["lightning", "arbitrum"]
max_concurrent_jobs = 2
allowed_images = []
# Optional (with defaults):
max_job_timeout_secs = 3600
allow_network_jobs = false
gpu_enabled = false
gpu_auto_detect = true
execution_mode = "docker"   # or "local"
require_verification = false
credit_allowance_secs = 3600
protected_environments = []  # e.g. ["production"]

[compute.wallets]
lightning = "lnbc1youraddress..."
arbitrum = "0xYourAddress..."
```

## Key Dependencies

| Dependency | Purpose |
|-----------|---------|
| `ed25519-dalek` | Ed25519 signing and verification |
| `libp2p` 0.54 | DHT (Kademlia), mDNS discovery, Noise encryption |
| `git2` | Git operations (vendored libgit2) |
| `axum` | HTTP API framework |
| `tokio` | Async runtime |
| `bollard` | Docker API client (compute job execution) |
| `reqwest` | HTTP client (remote helper, uses rustls-tls) |
| `clap` | CLI argument parsing |
| `serde` / `serde_json` / `serde_yaml` | Serialization |
| `sha2` | SHA-256 hashing for content verification |
| `rustls` | TLS for the HTTP client (no system OpenSSL needed) |

## Running Tests

```sh
cargo test --workspace --no-fail-fast
```

The suite covers: identity lifecycle, repo addressing, ACL authorization, patch/merge request signing, torrent piece verification, bundle create/verify/clone/fetch, git bridge ref listing, repo index operations, compute job creation/verification, offer matching, payment channel lifecycle, worker reputation, CI config parsing, blob content addressing, adaptive piece sizing, blob store operations, pointer file encoding, collection manifest signing, seeder/leecher tracking, P2P transfer manager lifecycle, piece serving, piece assignment strategy, workflow parsing/expansion/triggers/permissions, and API auth scoping. End-to-end behavior is pinned by executable demos (`demos/demo-*.sh`, each ending in `PASS`) plus the desktop smoke test (`./gui-test/run.sh`).

## HTTP API Endpoints

Most endpoints need `Authorization: Bearer <api-token>` (the daemon writes the
token to `~/.local/share/gitforest/api.token` on startup). Endpoints marked
scoped also accept a job token (`GF_JOB_TOKEN`) carrying the named permission.

### Identity
- `GET /api/id` — your peer ID
- `GET/POST /api/profile` — show/update your profile
- `POST /api/identity/export` — export identity to an encrypted file
- `POST /api/identity/import` — import identity from a file

### Repos
- `GET /api/repos` — list hosted repos
- `POST /api/repos` — add a repo
- `POST /api/repos/clone` — clone a repo from the network
- `GET /api/repos/{name}` — repo details
- `DELETE /api/repos/{name}` — remove a repo
- `GET /api/repos/{name}/refs` — list refs
- `GET /api/repos/{name}/branches` — list branches
- `GET /api/repos/{name}/log` — commit log
- `GET /api/repos/{name}/ci-config` — parsed `.gitforest-ci.yml`

### Collaboration
- `GET/POST /api/repos/{name}/patches` — list/submit patches
- `GET/POST /api/repos/{name}/merge-requests` — list/submit merge requests
- `POST /api/repos/{name}/merge-requests/{id}/accept` — accept a merge request
- `POST /api/repos/{name}/merge-requests/{id}/reject` — reject a merge request
- `GET/POST /api/repos/{name}/acl` — view/update access control

### Compute
- `GET /api/compute/jobs` — list compute jobs
- `POST /api/compute/jobs` — submit a compute job
- `GET /api/compute/jobs/{id}` — job status and result (scoped: `contents: read`)
- `POST /api/compute/jobs/{id}/cancel` — cancel one of your jobs
- `POST /api/compute/jobs/{id}/approve` — approve an env-gated job
- `POST /api/compute/jobs/{id}/reject` — reject an env-gated job
- `GET /api/compute/jobs/{id}/artifacts/{name}` — download a job artifact (scoped: `contents: read`)
- `POST /api/compute/remote/submit` — worker intake for a shipped job (cluster auth)
- `POST /api/compute/remote/result` — worker returns a result (cluster auth)
- `POST /api/compute/remote/cancel` — worker cancel notice (cluster auth)
- `GET /api/compute/offers` — list available workers
- `GET /api/compute/worker` — your worker offer status
- `POST /api/compute/worker/start` — announce as worker
- `POST /api/compute/worker/stop` — withdraw worker offer
- `GET /api/compute/trust` — worker trust scores
- `GET /api/compute/credit` — mutual-credit balance and per-peer ledger
- `GET /api/compute/payments` — list payment channels

### Large File Storage
- `GET /api/blobs` — list stored blobs
- `POST /api/blobs` — import a server-side file into the blob store (scoped: `contents: write`)
- `POST /api/blobs/upload` — upload raw request bytes as a blob (scoped: `contents: write`)
- `GET /api/blobs/{id}` — blob info (size, pieces, seeders)
- `DELETE /api/blobs/{id}` — remove a blob
- `GET /api/blobs/{id}/download` — download blob content (scoped: `contents: read`)
- `GET /api/blobs/{id}/meta` — blob metadata (scoped: `contents: read`)
- `POST /api/blobs/{id}/fetch` — fetch a blob from the network
- `GET /api/collections` — list collection manifests
- `POST /api/collections` — create a collection (model/dataset)
- `GET /api/collections/{id}` — collection details
- `POST /api/collections/{id}/fetch` — fetch a collection from the network

### Workflow cache
- `GET /api/caches/list` — list cache keys, local and peer-advertised (scoped: `contents: read`)
- `GET /api/caches?prefix=P` — newest key under a prefix (scoped: `contents: read`)
- `POST /api/caches` — save a path or blob id under a key (scoped: `contents: write`)
- `GET /api/caches/{key}` — cache entry (scoped: `contents: read`)
- `DELETE /api/caches/{key}` — delete a cache key (scoped: `contents: write`)

### Repo secrets
- `GET /api/repos/{name}/secrets` — list secret names (never values)
- `POST /api/repos/{name}/secrets` — store a secret
- `DELETE /api/repos/{name}/secrets/{key}` — delete a secret

### Workflows
- `GET /api/repos/{name}/workflows` — list parsed workflows in a repo
- `POST /api/repos/{name}/workflows/dispatch` — manually dispatch workflows (scoped: `workflows: write`)
- `GET /api/workflows/runs` — list workflow runs with status summaries (scoped: `workflows: read`)
- `GET /api/workflows/runs/{id}` — one run with per-job status (scoped: `workflows: read`)
- `POST /api/workflows/runs/{id}/rerun` — re-run failed jobs as a linked run (scoped: `workflows: write`)

### Models
- `POST /api/models/publish` — publish a collection as an LLM model on the DHT (scoped: `models: write`)
- `GET /api/models/search` — search shared models (text, architecture, quantization, format, param range)
- `GET /api/models/{id}` — model details with shard layout
- `POST /api/models/{id}/pull` — pull a model manifest and missing blobs (scoped: `models: read`)
- `POST /api/models/{id}/infer` — run one inference (scoped: `models: write`)
- `POST /api/models/{id}/infer/stream` — stream an inference over SSE (scoped: `models: write`)
- `GET/POST /api/models/{id}/acl` — show/change model access (grant/revoke/restrict/open)

### Transfers
- `GET /api/transfers` — list active blob transfers (downloads/uploads)
- `GET /api/transfers/{blob_id}` — transfer status with progress
- `GET /api/blobs/{id}/pieces/{index}` — serve a specific piece (for P2P transfer, scoped: `contents: read`)

### Network
- `GET /api/status` — network status
- `GET /api/peers` — connected peers
- `GET /api/peers/exchange` — peer/cache/model directory for gossip
- `GET /api/analytics/summary` — peer/storage totals for dashboards
- `GET /api/analytics/history` — sampled peer-count history
- `GET /api/resolve/{identifier}` — resolve a peer name tag or alias

### Remote helpers
- `GET /api/remote/{peer_id}/{repo}/refs` — list a remote peer's repo refs
- `POST /api/remote/{peer_id}/{repo}/fetch` — fetch from a remote peer
- `POST /api/remote/{peer_id}/{repo}/push` — push to a remote peer

## Deploy Bootstrap Nodes to Google Cloud

See `deploy/gcloud/README.md` for Terraform and helper scripts that create 3 persistent GitForest bootstrap/seed nodes on Google Compute Engine with static IPs, persistent data disks, and firewall rules for P2P, git protocol, and Web UI traffic.

## MCP Server (AI agents)

`gitforest-mcp` exposes the daemon to MCP clients over stdio
(JSON-RPC), 26 tools: status, peers, profile, repos
(list/details/add/log), workflows (list/dispatch/runs/run-detail), jobs
(list/status/submit/approve/reject/cancel), offers, credit balance,
blobs, models (search/get/pull/infer/publish), and caches. It talks
to the local daemon (`GITFOREST_DAEMON_URL`, default
`http://127.0.0.1:7878`) using the daemon's API token file, or
`GITFOREST_API_TOKEN`.

```json
{
  "mcpServers": {
    "gitforest": {
      "command": "gitforest-mcp"
    }
  }
}
```

## License

AGPL-3.0
