# Feature Specification: gitforest Production Product (Desktop Client + Binary Release)

**Feature**: 001-production-product
**Created**: 2026-10-23
**Status**: Draft
**Input**: Ship gitforest as a real product: raw binaries + minimal desktop GUI with desktop-client-style repo management, analytics, profile/identity management, machine management, and account sign-in.

---

## Current State Assessment (baseline)

Working today (12.5k LOC Rust, 113 tests, green CI):
- DHT/mDNS networking (libp2p Kademlia), signed Ed25519 identity
- `git clone gitforest://<peer>/<repo>` via remote helper; push with ACL + proxying
- Signed patches, fork+merge requests, ACLs persisted to config.toml
- LFS blob store (content-addressed SHA-256, adaptive chunking, collection manifests)
- Compute marketplace skeleton: Docker runner executes CI jobs; payments = `StubPaymentBackend`
- Embedded single-page web dashboard (1,116-line static index.html) + REST API w/ random bearer token
- Release workflow builds Linux/macOS tarballs + .deb; install.sh one-liner exists
- Terraform + scripts for 3 GCE bootstrap nodes (deploy/gcloud) — not yet applied

Known gaps to close (from code + .claude/plan.md):
- No desktop app — the web UI is the only frontend
- No onboarding/first-run experience at all
- No analytics (transfer stats raw, no history/persistence)
- Account/identity UX is a CLI command; key backup/restore doesn't exist
- Payments stubbed; `peak_cpu_percent` TODO; compute job status CLI TODOs
- gitforest releases: workflows exist but no tagged release cut yet
- Bootstrap network not yet live

---

## User Scenarios & Testing

### US-1 — Install & first-run onboarding (P1)

A developer downloads the binary (or runs install.sh), launches the gitforest desktop app.
The app detects the daemon isn't running, starts it, generates an Ed25519 identity, and presents a
guided first-run: shows peer ID, offers key backup (export passphrase-encrypted key file), suggests
seeding a first repo or joining the network by cloning the demo repo.

**Why P1**: Without onboarding nothing else matters — this is the front door of the product.

**Independent Test**: Fresh VM (no ~/.config/gitforest) → install binary → launch app → within
5 minutes user has cloned the canonical demo repo from the public bootstrap network without
touching a terminal.

**Acceptance Scenarios**:
1. **Given** a machine with no gitforest state, **When** app launches, **Then** daemon auto-starts with health check and identity is created
2. **Given** first-run wizard, **When** user clicks "Back up my key", **Then** an encrypted key export is written to a user-chosen path
3. **Given** daemon running with identity, **When** user chooses "Clone demo repo", **Then** clone succeeds from a bootstrap node and appears in the repo list
4. **Given** app closed and reopened, **When** daemon already running, **Then** app reconnects via stored API token, no wizard shown

---

### US-2 — Repository management, desktop-client style (P1)

User sees all hosted repos in the desktop client with: branches, commit log, working-copy status,
clone URL copy button, seeding toggle, ACL viewer. Can add an existing local repo or create a new
one from the GUI. Incoming merge requests and patches surface as notifications with accept/reject.

**Why P1**: This is the daily-driver loop — the desktop-client parity ask.

**Independent Test**: With daemon running, create a repo in the app, commit via normal git,
see commit appear in app log; receive a merge request from a second peer, review diff summary,
accept it, verify ref updated.

**Acceptance Scenarios**:
1. **Given** daemon running, **When** user clicks "Add repository" and picks a folder, **Then** repo is registered and announced on DHT
2. **Given** a hosted repo, **When** new commits are made locally, **Then** the app's commit log refreshes within 30s
3. **Given** an incoming MR, **When** user clicks Accept, **Then** a signed merge occurs via existing API and the requester is notified
4. **Given** a repo, **When** user copies clone URL, **Then** `gitforest://<peer>/<repo>` is on clipboard

---

### US-3 — Identity, profile & "sign-in" (P1)

Desktop "Profile" page: shows peer ID, public key fingerprint, display name (self-asserted, signed),
repos hosted/seeded counts. Supports: export encrypted identity backup, restore identity from backup
on a new machine (= this network's version of account sign-in), and publishing a signed profile
directory record to the DHT (name + peer ID + optional email hash).

**Why P1**: The user's ask for "sign into an account" maps to portable identity + recovery. Trust
in key backup is what makes P2P identity safe for normals.

**Independent Test**: Create identity on machine A, export backup, wipe config on machine B,
restore from backup → same peer ID, repos re-announce, profile directory record resolves.

**Acceptance Scenarios**:
1. **Given** an identity, **When** user exports backup with a passphrase, **Then** an encrypted file is written (rage/age format)
2. **Given** a backup file, **When** restored on a fresh install, **Then** peer ID matches the original
3. **Given** profile fields filled, **When** saved, **Then** a signed profile record is published to the DHT and retrievable by other peers

---

### US-4 — Network & transfer analytics dashboard (P2)

Analytics page: live connected peers, per-repo seeder/leecher counts, historical upload/download
throughput sparklines (persisted locally, sampled every 5s), compute job history with duration,
storage usage of blob store vs repos. Backed by a new `/api/analytics/*` family with an on-disk
metrics ring buffer (sqlite at data_dir/metrics.db, capped 7 days).

**Why P2**: Differentiates the product, but the app is useful without graphs. Needs daemon-side
metrics plumbing, so it's a real chunk of work.

**Independent Test**: Seed a repo, have a peer clone it, confirm transfer appears in the analytics
timeline with correct byte counts; restart daemon → history survives.

**Acceptance Scenarios**:
1. **Given** active transfer, **When** analytics page opens, **Then** live throughput renders within 2s
2. **Given** daemon restart, **When** analytics page reopens, **Then** 24h of prior history is visible
3. **Given** a completed compute job, **When** job history viewed, **Then** duration, image, exit code shown

---

### US-5 — Machine & daemon management (P2)

"Computer" page: daemon start/stop/restart, port configuration, data directory usage, log viewer
(tailed from daemon), update checker (compares local version to latest gitforest release),
config editor for a safe subset of config.toml with validation.

**Why P2**: Power-user feature; required for a self-service product ("way to manage their computer").

**Independent Test**: From the app only, stop the daemon, change API port, restart, and the client
reconnects on the new port; update banner appears when a newer gitforest release exists (mockable).

**Acceptance Scenarios**:
1. **Given** app's daemon running, **When** user clicks Stop, **Then** graceful SIGTERM, status shows "stopped" within 3s
2. **Given** a newer release on gitforest, **When** update check runs, **Then** banner shows version + instructions
3. **Given** invalid port entered, **When** saving config, **Then** validation error shows, daemon untouched

---

### US-6 — Production release infrastructure (P1)

Tag-driven release: versioned builds for linux-x64 tar.gz, macos-arm64 tar.gz, .deb; SHA256SUMS;
install.sh verified against latest release; first tag v0.3.0 cut during hackathon. Bootstrap
network live: terraform deploys 3 GCE nodes seeding the canonical demo repo, and
the generated bootstrap peer list is baked into default client config.

**Why P1**: "Raw binary available" is an explicit ask; bootstrap nodes make US-1's clone-demo
scenario actually work.

**Independent Test**: On a fresh VM, run install.sh → binary runs --version; clone of demo
repo works with zero config.

**Acceptance Scenarios**:
1. **Given** tag v0.3.0 pushed, **When** workflow completes, **Then** release page has 3 assets + SHA256SUMS
2. **Given** fresh machine, **When** install.sh runs, **Then** `gitforest` + `git-remote-gitforest` are on PATH
3. **Given** bootstrap nodes applied, **When** fresh daemon starts, **Then** it discovers >=1 bootstrap peer within 60s

---

### US-7 — Compute payments hardening (P3)

Keep stub payments but gate behind `compute.payments_enabled = false` default; label UI
"Experimental". Document real-payment design (Lightning escrow) as a spec for post-hackathon.

**Why P3**: Honesty in shipping; real money flows are not hackathon material.

**Independent Test**: Default install never touches payment code path; toggling flag enables stub
escrow recording visible in job details.

---

## Edge Cases

- Something else owns port 7878 → client detects "not my daemon" vs mine
- Two client instances open simultaneously → token race; second attaches read-only
- Identity restore onto a machine that already has an identity → require explicit overwrite confirm
- User deletes the repo folder on disk while seeding → repo marked "missing", DHT announcement withdrawn
- All bootstrap nodes unreachable → app shows degraded banner, mDNS LAN discovery still works
- Client upgrade with old daemon running → version mismatch warning with restart prompt

## Success Criteria (measurable)

| Metric | Target |
|---|---|
| Fresh install → first P2P clone | < 5 min, zero terminal |
| Release assets per tag | linux-x64, macos-arm64, .deb + checksums |
| Bootstrap network | 3 nodes deployed, seeded, mutually discoverable |
| Test suite | existing tests green; +new tests for identity backup, analytics buffer, profile signing |
| App startup → dashboard data | < 3s on localhost |

## Out of Scope (explicit)
- Windows builds; real crypto settlement; mobile; multi-tenant server mode
