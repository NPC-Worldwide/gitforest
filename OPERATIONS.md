# GitForest Operations Runbook

How to install, run, join, and upgrade a GitForest node. Every command
below matches the current CLI and config surface.

## Ports

Each daemon listens on three ports derived from one setting:

| Purpose | Port | Notes |
|---|---|---|
| HTTP API + web UI | `api_port` (default 7878) | bind 127.0.0.1 unless clustering |
| libp2p / DHT | `p2p_port` (default 9418) | TCP, must be reachable by peers |
| git protocol | `p2p_port + 1` (default 9419) | derived, no separate setting |

Two daemons on one host need P2P ports spaced by 2, or one's git
server loses the bind race (it logs `Git server failed` and keeps
going without git-protocol service). The daemon refuses to start
when `api_port` collides with either of the other two.

## Install

Pick one path. All of them ship `gitforest`, `gtfo` (short alias),
and `git-remote-gitforest` (the `gitforest://` remote helper).

- Script (after the first release is cut):
  `curl -fsSL https://raw.githubusercontent.com/NPC-Worldwide/gitforest/main/install.sh | bash`
  Installs to `~/.local/bin` from the latest GitHub release.
- CLI deb: `sudo dpkg -i gitforest_<version>_amd64.deb` from the
  release page. Binaries land in `/usr/bin`.
- Desktop deb: `sudo dpkg -i GitForest_*_amd64.deb`. Links the CLI
  tools plus `gitforest-desktop` into `/usr/local/bin`.
- From source: `cargo build --release -p gt-daemon -p gt-remote`,
  then use `target/release/{gitforest,gtfo,git-remote-gitforest}`.

Verify: `gitforest --version` and
`curl -s http://127.0.0.1:7878/api/status` once the daemon runs.

## First boot

```sh
gitforest init                       # creates ~/.config/gitforest/identity.key
gitforest id                         # prints your peer ID
gitforest --config ~/.config/gitforest/config.toml daemon
```

On first start the daemon writes a random API token to
`~/.local/share/gitforest/api.token` (mode 0600) and self-heals the
mode on later starts. The CLI reads that file and sends it as
`Authorization: Bearer`. The browser UI gets an HttpOnly
`SameSite=Strict` session cookie, minted only over loopback.

Add a repo: `gitforest add /path/to/repo --name myrepo`.
Clone over the helper: `git clone gitforest://<peer-id>/myrepo`
(the helper talks to the daemon at `GITFOREST_DAEMON_URL`,
default `http://127.0.0.1:7878`).

Never run with `GITFOREST_SKIP_AUTH` outside tests: only the exact
values `1`/`true` disable auth, and the docker entrypoint unsets
it in daemon mode.

## Joining a cluster

Add to `config.toml`:

```toml
bootstrap_peers = ["/ip4/203.0.113.5/tcp/9418/api/7878"]
cluster_token = "shared-secret"
```

Notes:

- Keep the `/api/7878` segment on bootstrap addresses when the
  peer's API port is non-default; without it the dialer assumes
  7878. Since the exchange-tagging fix, a plain bootstrap is
  enough for resolution — no `advertise_addr` required on either
  side for basic clustering.
- Set `advertise_addr` (e.g. `"/ip4/203.0.113.5/tcp/9418/api/7878"`)
  on nodes behind NAT so others learn the external address.
- `cluster_token` lets cluster peers call each other's authed
  endpoints. The value lives in `config.toml` in cleartext:
  `chmod 600` the file when it carries a token.
- Discovery is asynchronous: a fresh node tags its bootstrap
  within ~2s of it answering, plus DHT identify over mDNS/LAN.
  `GET /api/peers` shows `discovered_peers`; a `/gf/<id>`
  entry means that peer is dialable.

## Compute (federated runners)

Enable worker mode in `config.toml`:

```toml
[compute]
worker_enabled = true
max_cpu_cores = 4
max_ram_mb = 8192
hourly_rate_sats = 0
```

or at runtime: `gitforest compute worker-start --cpu 4 --ram 8192`.
Jobs run in Docker by default; set `allowed_images` and leave
`allow_network_jobs` off unless you trust the submitters.

Result trust: per-job `require_verification`, or flip it globally
with `compute.require_verification = true` to force independent
replay of every result (mismatch marks the job Disputed, fails both
parties' reputation, and refunds the escrow).

Payments, in order of setup cost:

1. Mutual credit (default, no setup): bilateral receipts net into
   balances owed to the network, plus a seeding/leeching
   reputation that throttles pure leechers.
2. Lightning: point the daemon at your LND (REST or gRPC with
   cert fingerprint pinning); jobs escrow with hold invoices,
   release on accepted result, cancel on refund.
3. EVM L2: create a keystore once —
   `gitforest compute evm-keystore-create --out keystore.json`
   (password from a 0600 file or `GITFOREST_EVM_KEY_PASSWORD`,
   never argv), then set `keystore_path` + `from_address` on the
   backend. Refund drops the payment intent; nothing is broadcast.
   Rotate with `gitforest compute evm-keystore-rotate --from old.json
   --path new.json --password-file pw` (add `--new-password-file` to
   change the password too), then follow its checklist: fund the new
   address, repoint `keystore_path`/`from_address`, restart, drain
   and delete the old key.

## Docker

`docker compose up` builds the workspace image; per-node knobs are
`GF_P2P_PORT`, `GF_API_PORT`, `GF_BOOTSTRAP_PEERS` (comma-separated
quoted multiaddrs), `GF_ADVERTISE_ADDR`, `GF_CLUSTER_TOKEN`,
`GF_REPO_SOURCE` (cloned once into the node). Data lives in
`GF_DATA_DIR` (default `/root/.local/share/gitforest`).

## Upgrade

1. Stop the daemon (`gitforest daemon --stop`, or your supervisor).
2. Replace the binaries (re-run `install.sh`, `dpkg -i`, or copy
   the fresh `target/release` trio over the old ones).
3. Start the daemon. Config is forward-compatible: unknown sections
   parse with defaults, and registries (`workers.json`,
   `submitters.json`) reload from `data_dir`.

## Cutting a release (maintainers)

Push a `v*` tag. The release workflow builds the Linux/macOS CLI
tarballs, the CLI deb, and the desktop deb/AppImage/dmg, then
attaches everything to the GitHub release — which is what
`install.sh` downloads. No release exists yet: until the first tag
lands, `install.sh` 404s and from-source is the only path.

## Troubleshooting

- `Address already in use` on start: another daemon (or a dead
  test run — check `ps aux | grep target/debug/gitforest`) holds
  the API, P2P, or git port.
- Remote refs/fetch/push 404: the target peer ID is not tagged
  yet — check `GET /api/peers` for `/gf/<id>` and confirm the
  bootstrap address is reachable from this host.
- Helper push rejected `not authorized`: the ACL grant must name
  the pusher's own peer ID (`gitforest id` on the pushing
  machine); signatures bind every push to its key.
- `401 Unauthorized` from the CLI: the daemon rewrites
  `~/.local/share/gitforest/api.token` with a fresh token on
  every start, and the CLI reads that file per invocation — so a
  401 means CLI and daemon disagree on HOME, or two daemons share
  one HOME (the second start overwrites the first one's token).
  Run one daemon per HOME and keep HOME consistent.
