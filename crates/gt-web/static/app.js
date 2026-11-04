const API = '';
let fullPeerId = '';
let currentRepoName = '';

// ===== Tab Management =====

function showTab(name) {
    document.querySelectorAll('main > div').forEach(d => d.classList.add('hidden'));
    const el = document.getElementById('tab-' + name);
    if (el) el.classList.remove('hidden');
    document.querySelectorAll('nav a').forEach(a => a.classList.remove('active'));
    const link = document.querySelector(`nav a[href="#${name}"]`);
    if (link) link.classList.add('active');
}

function showRepoSubTab(name) {
    ['commits', 'branches', 'acl'].forEach(n => {
        const el = document.getElementById('repo-sub-' + n);
        if (el) el.classList.toggle('hidden', n !== name);
    });
    document.querySelectorAll('#tab-repo-detail .sub-tabs button').forEach(b => {
        b.classList.toggle('active', b.textContent.toLowerCase().includes(name));
    });
}

function showComputeSubTab(name) {
    ['jobs', 'workers', 'payments'].forEach(n => {
        const el = document.getElementById('compute-sub-' + n);
        if (el) el.classList.toggle('hidden', n !== name);
    });
    document.querySelectorAll('#tab-compute .sub-tabs button').forEach(b => {
        b.classList.toggle('active', b.textContent.toLowerCase().includes(name));
    });
}

function showProposalSubTab(name) {
    ['merge-requests', 'patches'].forEach(n => {
        const el = document.getElementById('proposals-' + n);
        if (el) el.classList.toggle('hidden', n !== name);
    });
    document.querySelectorAll('#tab-proposals .sub-tabs button').forEach(b => {
        b.classList.toggle('active', b.textContent.toLowerCase().replace(' ', '-').includes(name));
    });
}

// ===== Toast =====

function toast(msg) {
    const t = document.createElement('div');
    t.className = 'toast';
    t.textContent = msg;
    document.body.appendChild(t);
    setTimeout(() => t.remove(), 3000);
}

function copyPeerId() {
    if (fullPeerId) {
        navigator.clipboard.writeText(fullPeerId);
        toast('Peer ID copied to clipboard');
    }
}

// ===== Data Loading =====

async function loadIdentity() {
    try {
        const resp = await fetch(API + '/api/id');
        const data = await resp.json();
        fullPeerId = data.peer_id;
        const short = data.peer_id.slice(0, 16) + '...';
        document.getElementById('peer-id').textContent = short;
        document.getElementById('peer-id').title = data.peer_id;
        document.getElementById('identity-peer-id').textContent = data.peer_id;
    } catch (e) {
        document.getElementById('peer-id').textContent = 'daemon offline';
    }
}

async function loadRepos() {
    try {
        const resp = await fetch(API + '/api/repos');
        const repos = await resp.json();
        const container = document.getElementById('repos-list');

        if (repos.length === 0) {
            container.innerHTML = '<div class="empty">No repos yet. Use <code>gitforest add &lt;path&gt;</code> to add one.</div>';
            return;
        }

        container.innerHTML = repos.map(repo => `
            <div class="card">
                <div class="card-header">
                    <div>
                        <div class="repo-name" data-action="open-repo" data-arg="${escHtml(repo.name)}">${escHtml(repo.name)}</div>
                        ${repo.description ? `<div class="repo-desc">${escHtml(repo.description)}</div>` : ''}
                    </div>
                    <span class="badge ${repo.seeding ? 'badge-green' : 'badge-orange'}">
                        ${repo.seeding ? 'seeding' : 'paused'}
                    </span>
                </div>
                <div class="card-body">
                    <div class="clone-url" data-action="copy-text" data-arg="git clone ${escHtml(repo.clone_url)}" title="Click to copy">
                        git clone ${escHtml(repo.clone_url)}
                    </div>
                </div>
            </div>
        `).join('');
    } catch (e) {
        document.getElementById('repos-list').innerHTML =
            '<div class="empty">Cannot connect to daemon. Is it running?</div>';
    }
}

async function openRepo(name) {
    currentRepoName = name;
    showTab('repo-detail');

    // Header
    document.getElementById('repo-detail-header').innerHTML = `<h2>${escHtml(name)}</h2>`;

    // Load commits, branches, ACL in parallel
    loadRepoCommits(name);
    loadRepoBranches(name);
    loadRepoAcl(name);
    showRepoSubTab('commits');
}

async function loadRepoCommits(name) {
    const container = document.getElementById('repo-sub-commits');
    try {
        const resp = await fetch(API + `/api/repos/${name}/log`);
        const commits = await resp.json();

        if (commits.length === 0) {
            container.innerHTML = '<div class="empty">No commits yet.</div>';
            return;
        }

        container.innerHTML = `<div class="card"><ul class="commit-list">
            ${commits.map(c => `
                <li class="commit-item">
                    <span class="commit-sha">${escHtml(c.short_sha)}</span>
                    <span class="commit-msg">${escHtml(c.message)}</span>
                    <span class="commit-meta">${escHtml(c.author)} &middot; ${escHtml(c.time)}</span>
                </li>
            `).join('')}
        </ul></div>`;
    } catch (e) {
        container.innerHTML = '<div class="empty">Failed to load commits.</div>';
    }
}

async function loadRepoBranches(name) {
    const container = document.getElementById('repo-sub-branches');
    try {
        const resp = await fetch(API + `/api/repos/${name}/branches`);
        const branches = await resp.json();

        if (branches.length === 0) {
            container.innerHTML = '<div class="empty">No branches.</div>';
            return;
        }

        container.innerHTML = `<div class="card">
            ${branches.map(b => `
                <div class="branch-item">
                    <span class="branch-icon">&#9679;</span>
                    <span class="fw-500">${escHtml(b.name)}</span>
                    <span class="commit-sha">${escHtml(b.sha)}</span>
                    <span class="muted-flex-13">${escHtml(b.message)}</span>
                </div>
            `).join('')}
        </div>`;
    } catch (e) {
        container.innerHTML = '<div class="empty">Failed to load branches.</div>';
    }
}

async function loadRepoAcl(name) {
    const container = document.getElementById('repo-sub-acl');
    try {
        const resp = await fetch(API + `/api/repos/${name}/acl`);
        if (!resp.ok) {
            container.innerHTML = '<div class="empty">ACL not available for this repo.</div>';
            return;
        }
        const acl = await resp.json();

        let grantsHtml = '<div class="empty">No collaborators. Only the owner can push.</div>';
        if (acl.grants && acl.grants.length > 0) {
            grantsHtml = `<div class="card"><table>
                <thead><tr><th>Peer ID</th><th>Ref Pattern</th><th></th></tr></thead>
                <tbody>
                ${acl.grants.map(g => `
                    <tr>
                        <td class="mono-12">${g.peer_id.slice(0, 16)}...</td>
                        <td><span class="badge badge-blue">${escHtml(g.ref_pattern)}</span></td>
                        <td><a href="#" data-action="revoke-access" data-arg="${escHtml(name)}" data-arg2="${escHtml(g.peer_id)}" class="danger-12">revoke</a></td>
                    </tr>
                `).join('')}
                </tbody>
            </table></div>`;
        }

        container.innerHTML = `
            <div class="muted-block-mb12">
                Owner: <span class="mono">${acl.owner.slice(0, 16)}...</span>
                &middot; ACL version: ${acl.version}
            </div>
            ${grantsHtml}
        `;
    } catch (e) {
        container.innerHTML = '<div class="empty">Failed to load access control.</div>';
    }
}

async function revokeAccess(repoName, peerId) {
    try {
        await fetch(API + `/api/repos/${repoName}/acl`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ action: 'revoke', peer_id: peerId }),
        });
        toast('Access revoked');
        loadRepoAcl(repoName);
    } catch (e) {
        toast('Failed to revoke access');
    }
}

async function loadStatus() {
    try {
        const resp = await fetch(API + '/api/status');
        const data = await resp.json();
        document.getElementById('stat-repos').textContent = data.repos_hosted;
        document.getElementById('stat-seeding').textContent = data.repos_seeding;
        document.getElementById('stat-p2p-port').textContent = data.p2p_port;
    } catch (e) {}

    try {
        const resp = await fetch(API + '/api/peers');
        const data = await resp.json();
        document.getElementById('stat-peers').textContent = data.connected;
    } catch (e) {}
}

async function loadProposals() {
    try {
        const reposResp = await fetch(API + '/api/repos');
        const repos = await reposResp.json();

        let allMrs = [];
        let allPatches = [];

        for (const repo of repos) {
            try {
                const mrResp = await fetch(API + `/api/repos/${repo.name}/merge-requests`);
                const mrs = await mrResp.json();
                allMrs.push(...mrs.map(mr => ({ ...mr, repo: repo.name })));
            } catch (e) {}
            try {
                const pResp = await fetch(API + `/api/repos/${repo.name}/patches`);
                const patches = await pResp.json();
                allPatches.push(...patches.map(p => ({ ...p, repo: repo.name })));
            } catch (e) {}
        }

        const mrContainer = document.getElementById('proposals-merge-requests');
        if (allMrs.length === 0) {
            mrContainer.innerHTML = '<div class="empty">No merge requests yet.</div>';
        } else {
            mrContainer.innerHTML = `<div class="card">${allMrs.map(mr => `
                <div class="proposal-item">
                    <div class="proposal-title">${escHtml(mr.title)}</div>
                    <div class="proposal-meta">
                        <span class="badge badge-purple">MR</span>
                        ${escHtml(mr.source_branch)} &rarr; ${escHtml(mr.target_branch)}
                        &middot; by ${mr.author.slice(0, 8)}...
                        &middot; repo: ${escHtml(mr.repo)}
                    </div>
                </div>
            `).join('')}</div>`;
        }

        const pContainer = document.getElementById('proposals-patches');
        if (allPatches.length === 0) {
            pContainer.innerHTML = '<div class="empty">No patches yet.</div>';
        } else {
            pContainer.innerHTML = `<div class="card">${allPatches.map(p => `
                <div class="proposal-item">
                    <div class="proposal-title">${escHtml(p.title)}</div>
                    <div class="proposal-meta">
                        <span class="badge badge-blue">Patch</span>
                        ${p.patch_count} patch(es) for ${escHtml(p.target_branch)}
                        &middot; by ${p.author.slice(0, 8)}...
                        &middot; repo: ${escHtml(p.repo)}
                    </div>
                </div>
            `).join('')}</div>`;
        }
    } catch (e) {}
}

// ===== Compute =====

async function loadComputeJobs() {
    const container = document.getElementById('compute-sub-jobs');
    try {
        const resp = await fetch(API + '/api/compute/jobs');
        const jobs = await resp.json();

        document.getElementById('stat-jobs').textContent = jobs.length;

        if (jobs.length === 0) {
            container.innerHTML = '<div class="empty">No compute jobs yet. Submit one with <code>gitforest compute submit &lt;repo&gt;</code></div>';
            return;
        }

        container.innerHTML = `<div class="card">${jobs.map(j => `
            <div class="proposal-item">
                <div class="flex-between-center">
                    <div class="proposal-title">${escHtml(j.repo_name)} @ ${j.commit_sha.slice(0, 8)}</div>
                    <span class="badge ${j.status === 'pending' ? 'badge-orange' : j.status === 'completed' ? 'badge-green' : 'badge-blue'}">
                        ${escHtml(j.status)}
                    </span>
                </div>
                <div class="proposal-meta">
                    Job: <span class="mono">${j.id.slice(0, 8)}...</span>
                    &middot; Max: ${j.max_payment_sats} sats
                    &middot; ${j.created_at ? new Date(j.created_at).toLocaleString() : ''}
                </div>
                ${j.env_approval_pending && j.status === 'pending' ? `<div class="proposal-meta mt-8"><span class="badge badge-orange">awaiting approval</span> environment: ${escHtml(j.environment || '?')} &middot; approving releases the job, rejecting cancels it</div>` : ''}
                ${j.env_approval_pending && j.status === 'pending' ? `<button class="btn-accent mt-8" data-action="approve-job" data-arg="${escHtml(j.id)}">Approve for ${escHtml(j.environment || '?')}</button><button class="btn-danger mt-8 ml-8" data-action="reject-job" data-arg="${escHtml(j.id)}">Reject</button>` : ''}
                ${['pending', 'blocked', 'matched', 'verifying', 'running'].includes(j.status) ? `<button class="btn-danger mt-8 ${j.env_approval_pending ? 'ml-8' : ''}" data-action="cancel-job" data-arg="${escHtml(j.id)}">Cancel job</button>` : ''}
                ${['completed', 'failed'].includes(j.status) ? `<button class="btn-ghost mt-8 ml-8" data-action="view-job-logs" data-arg="${escHtml(j.id)}">View logs</button>` : ''}
                <div id="logs-${escHtml(j.id)}"></div>
            </div>
        `).join('')}</div>`;
    } catch (e) {
        container.innerHTML = '<div class="empty">Failed to load jobs.</div>';
    }
}

async function loadComputeWorkers() {
    const container = document.getElementById('compute-sub-workers');
    try {
        const resp = await fetch(API + '/api/compute/offers');
        const offers = await resp.json();

        document.getElementById('stat-workers').textContent = offers.length;

        if (offers.length === 0) {
            container.innerHTML = '<div class="empty">No workers available. Start one with <code>gitforest compute worker-start</code></div>';
            return;
        }

        container.innerHTML = `<div class="card"><table>
            <thead><tr>
                <th>Worker</th>
                <th>CPU</th>
                <th>RAM</th>
                <th>GPU</th>
                <th>Rate</th>
                <th>Load</th>
                <th>Reputation</th>
                <th>Chains</th>
            </tr></thead>
            <tbody>
            ${offers.map(o => `
                <tr>
                    <td class="mono-12">${o.worker_id.slice(0, 12)}...</td>
                    <td>${o.cpu_cores} cores</td>
                    <td>${o.ram_mb} MB</td>
                    <td>${o.has_gpu ? '<span class="badge badge-green">Yes</span>' : '<span class="badge badge-orange">No</span>'}</td>
                    <td>${o.hourly_rate_sats} sats/hr</td>
                    <td>${o.current_load}/${o.max_concurrent_jobs}</td>
                    <td>${(o.reputation * 100).toFixed(0)}%</td>
                    <td>${o.accepted_chains.map(c => `<span class="badge badge-purple">${escHtml(c)}</span>`).join(' ')}</td>
                </tr>
            `).join('')}
            </tbody>
        </table></div>`;
    } catch (e) {
        container.innerHTML = '<div class="empty">Failed to load workers.</div>';
    }
}

async function loadPaymentChannels() {
    const container = document.getElementById('compute-sub-payments');
    try {
        const resp = await fetch(API + '/api/compute/payments');
        const channels = await resp.json();

        document.getElementById('stat-channels').textContent = channels.length;

        if (channels.length === 0) {
            container.innerHTML = '<div class="empty">No payment channels yet.</div>';
            return;
        }

        container.innerHTML = `<div class="card"><table>
            <thead><tr>
                <th>Channel</th>
                <th>Job</th>
                <th>Chain</th>
                <th>Escrow</th>
                <th>Released</th>
                <th>Status</th>
            </tr></thead>
            <tbody>
            ${channels.map(c => `
                <tr>
                    <td class="mono-12">${c.id.slice(0, 8)}...</td>
                    <td class="mono-12">${c.job_id.slice(0, 8)}...</td>
                    <td><span class="badge badge-purple">${escHtml(c.chain)}</span></td>
                    <td>${c.escrow_amount} sats</td>
                    <td>${c.released_amount} sats</td>
                    <td><span class="badge ${c.status === 'Released' ? 'badge-green' : c.status === 'Disputed' ? 'badge-orange' : 'badge-blue'}">${escHtml(c.status)}</span></td>
                </tr>
            `).join('')}
            </tbody>
        </table></div>`;
    } catch (e) {
        container.innerHTML = '<div class="empty">Failed to load payment channels.</div>';
    }
}

async function loadSubmitRepos() {
    const sel = document.getElementById('submit-repo');
    if (!sel || sel.options.length > 0) return;
    try {
        const resp = await fetch(API + '/api/repos');
        const repos = await resp.json();
        sel.innerHTML = repos.map(r => `<option value="${escHtml(r.name)}">${escHtml(r.name)}</option>`).join('');
        prefillCiYaml();
    } catch (e) {
        sel.innerHTML = '';
    }
}

async function prefillCiYaml() {
    const sel = document.getElementById('submit-repo');
    const yaml = document.getElementById('submit-yaml');
    if (!sel || !yaml || !sel.value) return;
    try {
        const resp = await fetch(API + `/api/repos/${encodeURIComponent(sel.value)}/ci-config`);
        if (!resp.ok) {
            yaml.value = '';
            yaml.placeholder = 'No .gitforest-ci.yml in this repo — paste job config here';
            return;
        }
        const data = await resp.json();
        yaml.value = data.yaml || '';
        yaml.placeholder = 'Prefilled from the selected repo';
    } catch (e) {
        yaml.value = '';
    }
}

async function submitComputeJob() {
    const result = document.getElementById('submit-result');
    const repo = document.getElementById('submit-repo').value;
    if (!repo) {
        result.innerHTML = '<div class="empty">Add a repo first with <code>gitforest add &lt;path&gt;</code></div>';
        return;
    }
    try {
        const resp = await fetch(API + '/api/compute/jobs', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
                repo_name: repo,
                commit_sha: document.getElementById('submit-commit').value || 'HEAD',
                job_config_yaml: document.getElementById('submit-yaml').value,
                max_payment_sats: parseInt(document.getElementById('submit-sats').value, 10) || 0,
                preferred_chain: document.getElementById('submit-chain').value,
            }),
        });
        if (!resp.ok) {
            result.innerHTML = `<div class="empty">Submit failed: HTTP ${resp.status}</div>`;
            return;
        }
        const data = await resp.json();
        result.innerHTML = `<div class="proposal-item bordered-round">
            <div class="proposal-title">Job submitted: <span class="mono">${escHtml(data.id)}</span></div>
            <div class="proposal-meta">Status: ${escHtml(data.status || 'Pending')}</div>
            ${data.warning ? `<div class="proposal-meta text-orange">Warning: ${escHtml(data.warning)}</div>` : ''}
        </div>`;
        toast('Job submitted');
        loadComputeJobs();
    } catch (e) {
        result.innerHTML = '<div class="empty">Submit failed: could not reach daemon.</div>';
    }
}

async function cancelJob(jobId) {
    if (!jobId) return;
    try {
        const resp = await fetch(API + `/api/compute/jobs/${encodeURIComponent(jobId)}/cancel`, {
            method: 'POST',
        });
        if (!resp.ok) {
            toast('Cancel failed');
            return;
        }
        toast('Job cancelled');
        loadComputeJobs();
    } catch (e) {
        toast('Cancel failed');
    }
}

async function approveJob(jobId) {
    if (!jobId) return;
    try {
        const resp = await fetch(API + `/api/compute/jobs/${encodeURIComponent(jobId)}/approve`, {
            method: 'POST',
        });
        if (!resp.ok) {
            toast('Approve failed');
            return;
        }
        toast('Job approved for its environment');
        loadComputeJobs();
    } catch (e) {
        toast('Approve failed');
    }
}

async function rejectJob(jobId) {
    if (!jobId) return;
    try {
        const resp = await fetch(API + `/api/compute/jobs/${encodeURIComponent(jobId)}/reject`, {
            method: 'POST',
        });
        if (!resp.ok) {
            toast('Reject failed');
            return;
        }
        toast('Job rejected');
        loadComputeJobs();
    } catch (e) {
        toast('Reject failed');
    }
}

async function viewJobLogs(jobId) {
    if (!jobId) return;
    const box = document.getElementById('logs-' + jobId);
    if (!box) return;
    if (box.dataset.loaded === '1') {
        box.innerHTML = '';
        delete box.dataset.loaded;
        return;
    }
    try {
        const resp = await fetch(API + `/api/compute/jobs/${encodeURIComponent(jobId)}`);
        if (!resp.ok) {
            toast('Failed to load logs');
            return;
        }
        const job = await resp.json();
        const result = job.result || {};
        const cap = (s) => {
            s = s || '';
            if (s.length > 65536) {
                return { text: s.slice(-65536), cut: s.length - 65536 };
            }
            return { text: s, cut: 0 };
        };
        const out = cap(result.stdout_tail);
        const err = cap(result.stderr_tail);
        const block = (label, c) => {
            if (!c.text && !c.cut) return '';
            return `<div class="log-label">${label}${c.cut ? ` <span class="log-truncated">(showing last 64 KB, ${c.cut} chars omitted)</span>` : ''}</div>` +
                `<pre class="log-block">${escHtml(c.text)}</pre>`;
        };
        box.innerHTML =
            `<div class="log-label">Exit code: ${escHtml(String(result.exit_code ?? '?'))}` +
            ` &middot; Worker: <span class="mono">${escHtml(result.worker_id || '?')}</span>` +
            ` &middot; Time: ${escHtml(String(result.execution_time_ms ?? '?'))} ms</div>` +
            block('stdout', out) + block('stderr', err);
        box.dataset.loaded = '1';
    } catch (e) {
        toast('Failed to load logs');
    }
}

function loadCompute() {
    loadComputeJobs();
    loadComputeWorkers();
    loadPaymentChannels();
    loadSubmitRepos();
}

// ===== Workflows =====

function runStatusBadge(s) {
    if (s.failed > 0) return '<span class="badge badge-red">failed</span>';
    if (s.running > 0) return '<span class="badge badge-blue">running</span>';
    if (s.pending > 0 || s.blocked > 0) return '<span class="badge badge-orange">pending</span>';
    if (s.total > 0 && s.completed === s.total) return '<span class="badge badge-green">passed</span>';
    return '<span class="badge badge-blue">empty</span>';
}

function jobStatusBadge(status) {
    const cls = status === 'completed' ? 'badge-green'
        : (status === 'failed' || status === 'disputed' || status === 'cancelled') ? 'badge-red'
        : (status === 'running' || status === 'matched' || status === 'verifying') ? 'badge-blue'
        : 'badge-orange';
    return `<span class="badge ${cls}">${escHtml(status)}</span>`;
}

async function loadWorkflowRuns() {
    const container = document.getElementById('runs-list');
    try {
        const resp = await fetch(API + '/api/workflows/runs');
        const runs = await resp.json();

        document.getElementById('stat-runs').textContent = runs.length;

        if (runs.length === 0) {
            container.innerHTML = '<div class="empty">No workflow runs yet. Push to a repo with <code>.gitforest/workflows/*.yml</code> to trigger one.</div>';
            return;
        }

        container.innerHTML = runs.map(r => `
            <div class="card">
                <div class="card-header">
                    <div>
                        <div class="repo-name" data-action="view-run" data-arg="${escHtml(r.id)}">${escHtml(r.workflow)} <span class="muted-12">in ${escHtml(r.repo)}</span></div>
                        <div class="repo-desc">${escHtml(r.event || '?')} &middot; ${escHtml(r.branch || '?')} &middot; <span class="mono">${escHtml((r.commit_sha || '').slice(0, 8))}</span></div>
                    </div>
                    <div class="text-right">${runStatusBadge(r.summary || {})}</div>
                </div>
                <div class="card-body">
                    <div class="muted-12">
                        ${r.summary ? `${r.summary.completed}/${r.summary.total} jobs passed${r.summary.failed ? ` &middot; ${r.summary.failed} failed` : ''}${r.summary.running ? ` &middot; ${r.summary.running} running` : ''}${r.summary.pending ? ` &middot; ${r.summary.pending} pending` : ''}${r.summary.blocked ? ` &middot; ${r.summary.blocked} blocked` : ''}` : ''}
                        ${r.created_at ? ` &middot; ${new Date(r.created_at).toLocaleString()}` : ''}
                    </div>
                </div>
            </div>
        `).join('');
    } catch (e) {
        container.innerHTML = '<div class="empty">Failed to load workflow runs. Is the daemon running?</div>';
    }
}

async function viewRun(id) {
    try {
        const resp = await fetch(API + `/api/workflows/runs/${encodeURIComponent(id)}`);
        if (!resp.ok) {
            toast('Failed to load run');
            return;
        }
        const r = await resp.json();
        const container = document.getElementById('runs-list');
        container.innerHTML = `
            <a class="back-btn" data-action="load-runs">&larr; Back to runs</a>
            <div class="card">
                <div class="card-header">
                    <div>
                        <div class="repo-name">${escHtml(r.workflow)}</div>
                        <div class="repo-desc">${escHtml(r.repo)} &middot; ${escHtml(r.file || '')}</div>
                    </div>
                    <div class="text-right">${runStatusBadge(r.summary || {})}</div>
                </div>
                <div class="card-body">
                    <div class="muted-block-mb12">
                        ${escHtml(r.event || '?')} &middot; ${escHtml(r.branch || '?')} &middot; <span class="mono">${escHtml(r.commit_sha || '')}</span>
                    </div>
                    ${(r.jobs || []).length === 0 ? '<div class="empty">No jobs in this run.</div>' : `<table>
                        <thead><tr><th>Job</th><th>ID</th><th>Status</th></tr></thead>
                        <tbody>
                        ${(r.jobs || []).map(j => `
                            <tr>
                                <td>${escHtml(j.name || j.id)}</td>
                                <td class="mono-12">${escHtml(j.id.slice(0, 12))}...</td>
                                <td>${jobStatusBadge(j.status || '?')}</td>
                            </tr>
                        `).join('')}
                        </tbody>
                    </table>`}
                </div>
            </div>
        `;
    } catch (e) {
        toast('Failed to load run');
    }
}

// ===== Blobs =====

function showBlobSubTab(name) {
    ['blobs', 'collections', 'transfers', 'caches'].forEach(n => {
        const el = document.getElementById('blob-sub-' + n);
        if (el) el.classList.toggle('hidden', n !== name);
    });
    document.querySelectorAll('#tab-blobs .sub-tabs button').forEach(b => {
        b.classList.toggle('active', b.textContent.toLowerCase().includes(name));
    });
}

async function loadBlobs() {
    const container = document.getElementById('blob-sub-blobs');
    try {
        const resp = await fetch(API + '/api/blobs');
        const blobs = await resp.json();

        document.getElementById('stat-blobs').textContent = blobs.length;

        let totalSize = 0;
        blobs.forEach(b => { if (b.size) totalSize += b.size; });
        document.getElementById('stat-blob-size').textContent = formatSize(totalSize);

        if (blobs.length === 0) {
            container.innerHTML = '<div class="empty">No blobs stored. Use <code>gitforest blob add &lt;path&gt;</code> to import a file.</div>';
            return;
        }

        container.innerHTML = `<div class="card"><table>
            <thead><tr>
                <th>Blob ID</th>
                <th>Size</th>
                <th>Pieces</th>
                <th>Piece Size</th>
                <th>Seeders</th>
                <th>Leechers</th>
                <th></th>
            </tr></thead>
            <tbody>
            ${blobs.map(b => `
                <tr>
                    <td class="mono-12-click" data-action="copy-text" data-arg="${escHtml(b.blob_id)}" title="${escHtml(b.blob_id)}">${b.blob_id.slice(0, 16)}...</td>
                    <td>${b.size ? formatSize(b.size) : '?'}</td>
                    <td>${b.piece_count || '?'}</td>
                    <td>${b.piece_size ? formatSize(b.piece_size) : '?'}</td>
                    <td><span class="badge badge-green">${b.seeders || 0}</span></td>
                    <td><span class="badge badge-orange">${b.leechers || 0}</span></td>
                    <td><a href="#" data-action="remove-blob" data-arg="${escHtml(b.blob_id)}" class="danger-12">remove</a></td>
                </tr>
            `).join('')}
            </tbody>
        </table></div>`;
    } catch (e) {
        container.innerHTML = '<div class="empty">Failed to load blobs. Is the daemon running?</div>';
    }
}

async function removeCache(key) {
    try {
        const resp = await fetch(API + `/api/caches/${encodeURIComponent(key)}`, { method: 'DELETE' });
        if (!resp.ok) {
            toast('Remove failed');
            return;
        }
        const data = await resp.json();
        toast(`Cache removed, freed ${formatSize(data.bytes_freed || 0)}`);
        loadCaches();
    } catch (e) {
        toast('Failed to remove cache');
    }
}

async function removeBlob(blobId) {
    try {
        await fetch(API + `/api/blobs/${blobId}`, { method: 'DELETE' });
        toast('Blob removed');
        loadBlobs();
    } catch (e) {
        toast('Failed to remove blob');
    }
}

async function loadCollections() {
    const container = document.getElementById('blob-sub-collections');
    try {
        const resp = await fetch(API + '/api/collections');
        const collections = await resp.json();

        document.getElementById('stat-collections').textContent = collections.length;

        if (collections.length === 0) {
            container.innerHTML = '<div class="empty">No collections yet. Create one via the API or <code>gitforest collection create</code></div>';
            return;
        }

        container.innerHTML = collections.map(c => `
            <div class="card">
                <div class="card-header">
                    <div>
                        <div class="repo-name" data-action="view-collection" data-arg="${escHtml(c.id)}">${escHtml(c.name)}</div>
                        <div class="repo-desc">${escHtml(c.description)}</div>
                    </div>
                    <div class="text-right">
                        <span class="badge badge-blue">${c.file_count} file(s)</span>
                        <span class="badge badge-green">${formatSize(c.total_size)}</span>
                    </div>
                </div>
                <div class="card-body">
                    <div class="muted-12">
                        Owner: <span class="mono">${c.owner.slice(0, 16)}...</span>
                        &middot; Created: ${new Date(c.created_at).toLocaleDateString()}
                        ${c.metadata && Object.keys(c.metadata).length > 0
                            ? '&middot; ' + Object.entries(c.metadata).map(([k, v]) => `<span class="badge badge-purple">${escHtml(k)}: ${escHtml(v)}</span>`).join(' ')
                            : ''}
                    </div>
                </div>
            </div>
        `).join('');
    } catch (e) {
        container.innerHTML = '<div class="empty">Failed to load collections.</div>';
    }
}

async function viewCollection(id) {
    try {
        const resp = await fetch(API + `/api/collections/${id}`);
        const c = await resp.json();
        const container = document.getElementById('blob-sub-collections');

        container.innerHTML = `
            <a class="back-btn" data-action="load-collections">&larr; Back to collections</a>
            <div class="card">
                <div class="card-header">
                    <div>
                        <div class="repo-name">${escHtml(c.name)}</div>
                        <div class="repo-desc">${escHtml(c.description)}</div>
                    </div>
                    <span class="badge ${c.verified ? 'badge-green' : 'badge-red'}">${c.verified ? 'verified' : 'unverified'}</span>
                </div>
                <div class="card-body">
                    <div class="muted-block-mb12">
                        Owner: <span class="mono">${c.owner.slice(0, 16)}...</span>
                        &middot; Total: ${formatSize(c.total_size)}
                        &middot; ${c.files.length} file(s)
                    </div>
                    ${c.metadata && Object.keys(c.metadata).length > 0 ? `
                        <div class="mb-12">
                            ${Object.entries(c.metadata).map(([k, v]) => `<span class="badge badge-purple">${escHtml(k)}: ${escHtml(v)}</span>`).join(' ')}
                        </div>
                    ` : ''}
                    <table>
                        <thead><tr><th>Path</th><th>Blob ID</th><th>Size</th><th>Type</th></tr></thead>
                        <tbody>
                        ${c.files.map(f => `
                            <tr>
                                <td>${escHtml(f.path)}</td>
                                <td class="mono-12-click" data-action="copy-text" data-arg="${escHtml(f.blob_id)}">${f.blob_id.slice(0, 16)}...</td>
                                <td>${formatSize(f.size)}</td>
                                <td class="muted-12">${f.media_type || '-'}</td>
                            </tr>
                        `).join('')}
                        </tbody>
                    </table>
                </div>
            </div>
        `;
    } catch (e) {
        toast('Failed to load collection');
    }
}

async function loadTransfers() {
    const container = document.getElementById('blob-sub-transfers');
    try {
        const resp = await fetch(API + '/api/transfers');
        const transfers = await resp.json();

        if (transfers.length === 0) {
            container.innerHTML = '<div class="empty">No active transfers. Downloads and uploads will appear here.</div>';
            return;
        }

        container.innerHTML = transfers.map(t => {
            const pct = (t.progress * 100).toFixed(1);
            const downloaded = formatSize(t.total_size * t.progress);
            const stateClass = t.state === 'Complete' ? 'badge-green'
                : t.state.includes('Failed') ? 'badge-orange'
                : t.state === 'Verifying' ? 'badge-purple'
                : 'badge-blue';
            return `
                <div class="card mb-12">
                    <div class="card-body">
                        <div class="flex-between-center-mb8">
                            <span class="mono-13-click" data-action="copy-text" data-arg="${escHtml(t.blob_id)}" title="${escHtml(t.blob_id)}">${t.blob_id.slice(0, 20)}...</span>
                            <span class="badge ${stateClass}">${escHtml(t.state)}</span>
                        </div>
                        <div class="progress-bar">
                            <div class="progress-bar-fill" data-pct="${pct}"></div>
                        </div>
                        <div class="flex-status-row">
                            <span>${downloaded} / ${formatSize(t.total_size)} (${pct}%)</span>
                            <span>${t.pieces_done} / ${t.piece_count} pieces</span>
                            <span>${t.active_peers.length} peer(s)</span>
                        </div>
                    </div>
                </div>
            `;
        }).join('');
        container.querySelectorAll('.progress-bar-fill[data-pct]').forEach(el => {
            el.style.width = el.dataset.pct + '%';
        });
    } catch (e) {
        container.innerHTML = '<div class="empty">Failed to load transfers.</div>';
    }
}

function loadBlobsTab() {
    loadBlobs();
    loadCollections();
    loadTransfers();
    loadCaches();
}

async function loadCaches() {
    const container = document.getElementById('blob-sub-caches');
    try {
        const resp = await fetch(API + '/api/caches/list');
        if (!resp.ok) {
            container.innerHTML = `<div class="empty">Cache directory unavailable: HTTP ${resp.status}</div>`;
            return;
        }
        const data = await resp.json();
        const local = data.local || [];
        const remote = data.remote || [];
        if (local.length === 0 && remote.length === 0) {
            container.innerHTML = '<div class="empty">No cache keys yet. Workflow <code>gitforest/cache</code> steps populate this directory across daemons.</div>';
            return;
        }
        const localTable = local.length === 0 ? '' : `<div class="card"><div class="card-body">
            <div class="proposal-title mb-12">Stored on this daemon</div>
            <table>
                <thead><tr><th>Key</th><th>Size</th><th>Stored</th><th></th></tr></thead>
                <tbody>
                ${local.map(c => `
                    <tr>
                        <td class="mono-12">${escHtml(c.key)}</td>
                        <td>${formatSize(c.size)}</td>
                        <td class="muted-12">${c.created_at ? new Date(c.created_at).toLocaleString() : ''}</td>
                        <td><a href="#" data-action="remove-cache" data-arg="${escHtml(c.key)}" class="danger-12">remove</a></td>
                    </tr>
                `).join('')}
                </tbody>
            </table>
        </div></div>`;
        const remoteTable = remote.length === 0 ? '' : `<div class="card"><div class="card-body">
            <div class="proposal-title mb-12">Advertised by peers</div>
            <table>
                <thead><tr><th>Key</th><th>Seeders</th><th>Size</th><th>Updated</th></tr></thead>
                <tbody>
                ${remote.map(c => `
                    <tr>
                        <td class="mono-12">${escHtml(c.key)}</td>
                        <td><span class="badge badge-purple">${c.seeders}</span></td>
                        <td>${formatSize(c.size)}</td>
                        <td class="muted-12">${c.updated_at ? new Date(c.updated_at).toLocaleString() : ''}</td>
                    </tr>
                `).join('')}
                </tbody>
            </table>
        </div></div>`;
        container.innerHTML = localTable + remoteTable;
    } catch (e) {
        container.innerHTML = '<div class="empty">Failed to load caches. Is the daemon running?</div>';
    }
}

// ===== Models =====

function formatParams(n) {
    if (n === null || n === undefined) return '?';
    if (n >= 1e9) return (n / 1e9).toFixed(1) + 'B';
    if (n >= 1e6) return (n / 1e6).toFixed(1) + 'M';
    if (n >= 1e3) return (n / 1e3).toFixed(1) + 'K';
    return String(n);
}

async function loadModels() {
    const container = document.getElementById('models-list');
    try {
        const input = document.getElementById('model-search');
        const text = input && input.value ? input.value.trim() : '';
        const url = API + '/api/models/search' + (text ? `?text=${encodeURIComponent(text)}` : '');
        const resp = await fetch(url);
        const models = await resp.json();

        document.getElementById('stat-models').textContent = models.length;

        if (models.length === 0) {
            container.innerHTML = '<div class="empty">No models found. Publish one with <code>gitforest model publish</code></div>';
            return;
        }

        container.innerHTML = models.map(m => `
            <div class="card">
                <div class="card-header">
                    <div>
                        <div class="repo-name">${escHtml(m.name)}</div>
                        <div class="repo-desc">${escHtml(m.architecture || '?')} &middot; ${formatParams(m.parameter_count)} params${m.quantization ? ` &middot; ${escHtml(m.quantization)}` : ''}${m.format ? ` &middot; ${escHtml(m.format)}` : ''}</div>
                    </div>
                    <div class="text-right">
                        ${m.local ? '<span class="badge badge-green">local</span>' : '<span class="badge badge-blue">remote</span>'}
                        <span class="badge badge-purple">${m.seeders} seeder(s)</span>
                    </div>
                </div>
                <div class="card-body">
                    <div class="muted-12">Owner: <span class="mono">${escHtml((m.owner || '').slice(0, 16))}...</span></div>
                    <div class="mt-8">
                        ${m.local ? '' : `<button class="btn-ghost" data-action="pull-model" data-arg="${escHtml(m.manifest_id)}">Pull weights</button>`}
                        <button class="btn-accent ${m.local ? '' : 'ml-8'}" data-action="toggle-infer" data-arg="${escHtml(m.manifest_id)}">Test inference</button>
                        <button class="btn-ghost ml-8" data-action="toggle-model-acl" data-arg="${escHtml(m.manifest_id)}">Access</button>
                    </div>
                    <div id="infer-box-${escHtml(m.manifest_id)}"></div>
                    <div id="acl-box-${escHtml(m.manifest_id)}"></div>
                </div>
            </div>
        `).join('');
    } catch (e) {
        container.innerHTML = '<div class="empty">Failed to load models. Is the daemon running?</div>';
    }
}

async function pullModel(id) {
    if (!id) return;
    try {
        const resp = await fetch(API + `/api/models/${encodeURIComponent(id)}/pull`, {
            method: 'POST',
        });
        if (!resp.ok) {
            toast('Pull failed');
            return;
        }
        const data = await resp.json();
        toast(data.status === 'complete' ? 'Weights already complete' : 'Pull started');
        loadModels();
    } catch (e) {
        toast('Pull failed');
    }
}

function toggleInfer(id) {
    if (!id) return;
    const box = document.getElementById('infer-box-' + id);
    if (!box) return;
    if (box.dataset.open === '1') {
        box.innerHTML = '';
        delete box.dataset.open;
        return;
    }
    box.innerHTML = `
        <div class="submit-grid mt-8">
            <div>
                <label for="infer-prompt-${escHtml(id)}">Prompt</label>
                <input id="infer-prompt-${escHtml(id)}" spellcheck="false" placeholder="Say hello">
            </div>
        </div>
        <button class="btn-accent mt-8" data-action="run-infer" data-arg="${escHtml(id)}">Run</button>
        <div id="infer-out-${escHtml(id)}" class="mt-8"></div>
    `;
    box.dataset.open = '1';
}

function toggleModelAcl(id) {
    if (!id) return;
    const box = document.getElementById('acl-box-' + id);
    if (!box) return;
    if (box.dataset.open === '1') {
        box.innerHTML = '';
        delete box.dataset.open;
        return;
    }
    box.dataset.open = '1';
    loadModelAcl(id);
}

async function loadModelAcl(id) {
    const box = document.getElementById('acl-box-' + id);
    if (!box) return;
    box.innerHTML = '<div class="empty">Loading access control...</div>';
    try {
        const resp = await fetch(API + `/api/models/${encodeURIComponent(id)}/acl`);
        if (!resp.ok) {
            box.innerHTML = `<div class="empty">ACL unavailable: HTTP ${resp.status}</div>`;
            return;
        }
        const acl = await resp.json();
        const grants = acl.grants || [];
        box.innerHTML = `
            <div class="proposal-meta mt-8">
                ${acl.restricted
                    ? `<span class="badge badge-orange">restricted</span> owner: <span class="mono">${escHtml((acl.owner || '').slice(0, 16))}...</span> &middot; version ${escHtml(String(acl.version ?? '?'))}`
                    : '<span class="badge badge-green">open</span> any peer may infer'}
            </div>
            ${grants.length > 0 ? `<div class="card mt-8"><table>
                <thead><tr><th>Peer ID</th><th></th></tr></thead>
                <tbody>
                ${grants.map(g => `
                    <tr>
                        <td class="mono-12">${escHtml(g.slice(0, 24))}...</td>
                        <td><a href="#" data-action="model-acl-revoke" data-arg="${escHtml(id)}" data-arg2="${escHtml(g)}" class="danger-12">revoke</a></td>
                    </tr>
                `).join('')}
                </tbody>
            </table></div>` : ''}
            <div class="submit-grid mt-8">
                <div>
                    <label for="acl-grant-${escHtml(id)}">Grant peer ID</label>
                    <input id="acl-grant-${escHtml(id)}" spellcheck="false" placeholder="peer id hex">
                </div>
            </div>
            <div class="mt-8">
                <button class="btn-accent" data-action="model-acl-grant" data-arg="${escHtml(id)}">Grant</button>
                ${acl.restricted
                    ? `<button class="btn-ghost ml-8" data-action="model-acl-act" data-arg="${escHtml(id)}" data-arg2="open">Open to all</button>`
                    : `<button class="btn-ghost ml-8" data-action="model-acl-act" data-arg="${escHtml(id)}" data-arg2="restrict">Restrict to owner</button>`}
            </div>
        `;
    } catch (e) {
        box.innerHTML = '<div class="empty">Failed to load access control.</div>';
    }
}

async function modelAclAct(id, action, peerId) {
    if (!id || !action) return;
    if (action === 'grant') {
        const input = document.getElementById('acl-grant-' + id);
        peerId = input ? input.value.trim() : '';
        if (!peerId) {
            toast('Enter a peer ID first');
            return;
        }
    }
    try {
        const body = peerId ? { action, peer_id: peerId } : { action };
        const resp = await fetch(API + `/api/models/${encodeURIComponent(id)}/acl`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(body),
        });
        if (!resp.ok) {
            toast(resp.status === 403 ? 'Only the owner can change access' : 'Access update failed');
            return;
        }
        toast('Access updated');
        loadModelAcl(id);
    } catch (e) {
        toast('Access update failed');
    }
}

async function runInfer(id) {
    if (!id) return;
    const promptEl = document.getElementById('infer-prompt-' + id);
    const out = document.getElementById('infer-out-' + id);
    if (!promptEl || !out) return;
    const prompt = promptEl.value.trim();
    if (!prompt) {
        toast('Enter a prompt first');
        return;
    }
    out.innerHTML = '<div class="empty">Running inference...</div>';
    try {
        const resp = await fetch(API + `/api/models/${encodeURIComponent(id)}/infer`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ prompt, max_tokens: 256 }),
        });
        if (!resp.ok) {
            out.innerHTML = `<div class="empty">Inference failed: HTTP ${resp.status}</div>`;
            return;
        }
        const data = await resp.json();
        if (data.status === 'fetching') {
            out.innerHTML = '<div class="empty">Weights are still downloading; try again shortly.</div>';
            return;
        }
        out.innerHTML = `<div class="log-label">Backend: ${escHtml(data.backend || '?')} &middot; ${escHtml(String(data.elapsed_ms ?? '?'))} ms${data.truncated ? ' &middot; truncated' : ''}</div>` +
            `<pre class="log-block">${escHtml(data.completion || '')}</pre>`;
    } catch (e) {
        out.innerHTML = '<div class="empty">Inference failed: could not reach daemon.</div>';
    }
}

function formatSize(bytes) {
    if (bytes === 0) return '0 B';
    const units = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(1024));
    return (bytes / Math.pow(1024, i)).toFixed(i > 0 ? 1 : 0) + ' ' + units[i];
}

// ===== Helpers =====

function escHtml(str) {
    if (!str) return '';
    return str.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

function copyText(text) {
    navigator.clipboard.writeText(text);
    toast('Copied to clipboard');
}

// ===== Click Delegation (CSP: no inline handlers) =====

document.addEventListener('click', (event) => {
    const el = event.target.closest('[data-action]');
    if (!el) return;
    const action = el.dataset.action;
    const arg = el.dataset.arg;
    const arg2 = el.dataset.arg2;
    switch (action) {
        case 'show-tab': showTab(arg); break;
        case 'show-repo-sub': showRepoSubTab(arg); break;
        case 'show-compute-sub': showComputeSubTab(arg); break;
        case 'show-proposal-sub': showProposalSubTab(arg); break;
        case 'show-blob-sub': showBlobSubTab(arg); break;
        case 'copy-peer-id': copyPeerId(); break;
        case 'open-repo': openRepo(arg); break;
        case 'copy-text': copyText(arg); break;
        case 'revoke-access': event.preventDefault(); revokeAccess(arg, arg2); break;
        case 'remove-blob': event.preventDefault(); removeBlob(arg); break;
        case 'remove-cache': event.preventDefault(); removeCache(arg); break;
        case 'view-collection': viewCollection(arg); break;
        case 'load-collections': loadCollections(); break;
        case 'submit-job': submitComputeJob(); break;
        case 'cancel-job': cancelJob(arg); break;
        case 'approve-job': approveJob(arg); break;
        case 'reject-job': rejectJob(arg); break;
        case 'load-models': loadModels(); break;
        case 'pull-model': pullModel(arg); break;
        case 'toggle-infer': toggleInfer(arg); break;
        case 'run-infer': runInfer(arg); break;
        case 'toggle-model-acl': toggleModelAcl(arg); break;
        case 'model-acl-act': modelAclAct(arg, arg2, null); break;
        case 'model-acl-grant': modelAclAct(arg, 'grant', null); break;
        case 'model-acl-revoke': event.preventDefault(); modelAclAct(arg, 'revoke', arg2); break;
        case 'view-run': viewRun(arg); break;
        case 'load-runs': loadWorkflowRuns(); break;
        case 'view-job-logs': viewJobLogs(arg); break;
    }
});

document.addEventListener('change', (event) => {
    if (event.target && event.target.id === 'submit-repo') prefillCiYaml();
});

// ===== Init =====

loadIdentity();
loadRepos();
loadStatus();
loadProposals();
loadCompute();
loadBlobsTab();
loadModels();
loadWorkflowRuns();

// Refresh periodically
setInterval(() => { loadStatus(); }, 10000);
setInterval(() => { loadRepos(); }, 30000);
setInterval(() => { loadCompute(); }, 15000);
setInterval(() => { loadBlobsTab(); }, 15000);
setInterval(() => {
    const box = document.getElementById('runs-list');
    if (box && !box.querySelector('.back-btn')) loadWorkflowRuns();
}, 15000);
