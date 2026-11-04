# Tasks: gitforest Production Product (001)

Derived from spec.md + plan.md. Check off during hackathon. Order = dependency order.

## Phase 0 — Freeze baseline
- [ ] T0.1 `cargo test` green on main; record count (baseline: 113)
- [ ] T0.2 Bump workspace version 0.1.0 → 0.3.0 in Cargo.toml (workspace.package)

## Phase 1 — Network & release (WS-A, WS-B)
- [ ] T1.1 Confirm `advertise_addr` config + `GF_ADVERTISE_ADDR` env path works (dockerized daemon: /api/status shows external addr)
- [ ] T1.2 `terraform apply` deploy/gcloud → 3 nodes up with persistent identity disks
- [ ] T1.3 Run `generate-bootstrap-peers.sh`; commit default bootstrap list into gt-core/src/config.rs defaults
- [ ] T1.4 Release workflow: add SHA256SUMS artifact; verify .deb installs on clean ubuntu:22.04 container
- [ ] T1.5 install.sh E2E on fresh VM (both binaries on PATH, `gitforest --version` runs)

## Phase 2 — Daemon API (WS-C)
- [ ] T2.1 `GET /api/first-run` (identity? backup_done flag?)
- [ ] T2.2 `POST /api/identity/export` — rage/age-encrypted, passphrase min 12 chars; test: roundtrip preserves peer ID
- [ ] T2.3 `POST /api/identity/import` — explicit overwrite flag; refuses silent clobber of existing identity
- [ ] T2.4 `GET/POST /api/profile` — signed profile record at DHT key `profile/<peer_id>`
- [ ] T2.5 Metrics ring buffer: sqlite at data_dir/metrics.db, 5s sampler (peers, bytes up/down, active transfers, jobs), 7-day cap, delete-on-corrupt
- [ ] T2.6 `GET /api/analytics/summary` + `GET /api/analytics/history?window=`
- [ ] T2.7 `POST /api/daemon/restart`, `GET /api/daemon/logs?tail=N`, `GET /api/version/latest` (cached gitforest check)
- [ ] T2.8 Fix runner.rs `peak_cpu_percent` TODO via bollard stats stream (or honestly drop the field)
- [ ] T2.9 Fill compute status/offers CLI TODOs in main.rs (query daemon API instead of stubbing)
- [ ] T2.10 `POST /api/repos/clone` — accepts {url, dest}, runs `git clone gitforest://…` server-side so the desktop first-run wizard can clone without shelling out

## Phase 3 — Desktop client (WS-D)
- [x] T3.1 Scaffold `desktop/` Electron+Vite+npcts app (separate tree); main process reads token from ~/.local/share/gitforest/api.token and proxies API calls — renderer never handles auth
- [x] T3.2 Daemon sidecar: spawn/attach in Electron main; health probe + token check for "not my daemon on 7878"; SIGTERM on app quit
- [x] T3.3 Sidebar shell: Repos / Network / Analytics / Compute / Profile / Settings + daemon status dot in TitleBar
- [~] T3.4 Repos page: list/detail(branches/log)/add-existing/clone-URL-copy DONE; refs/ACL detail + create-new pending backend shape
- [~] T3.5 MR inbox UI + accept/reject buttons done; pending backend per-MR action routes (collection GET/POST exists today)
- [~] T3.6 Profile page UI done (ID, fingerprint, display-name form, backup/restore buttons); live behavior gates on T2.2–T2.4, degrades gracefully
- [~] T3.7 Settings: start/stop/restart, log viewer, gitforest-release update banner DONE; port edit pending T2.7 config endpoint
- [~] T3.8 First-run wizard UI complete (5-step stepper, progress bar, identity gen, backup CTA, clone CTA); E2E pending T2.2 + POST /api/repos/clone backend route

## Phase 4 — Analytics UI (WS-E)
- [ ] T4.1 uPlot integration; live throughput sparkline (2s poll)
- [ ] T4.2 24h history view survives daemon restart (uses T2.5 data)
- [ ] T4.3 Compute job history table; blob-store disk usage bar

## Phase 5 — Payments gate (WS-F)
- [ ] T5.1 `compute.payments_enabled = false` default + UI "Experimental" banner
- [ ] T5.2 Write `specs/002-real-payments/spec.md` design stub (Lightning escrow flow, threat model)

## Phase 6 — Freeze & ship (WS-G)
- [ ] T6.1 README quickstart rewrite (desktop-first)
- [ ] T6.2 New tests green: identity export/import, ring buffer cap, first-run endpoint, profile sign/verify
- [ ] T6.3 clippy -D warnings + cargo fmt clean
- [ ] T6.4 Tag v0.3.0; verify 3 assets + checksums on release page
- [ ] T6.5 Two-laptop demo; fresh-VM install.sh → clone demo repo < 5 min
