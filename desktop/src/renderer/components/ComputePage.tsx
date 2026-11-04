import React, { useState, useEffect, useCallback } from 'react';
import { Cpu, RefreshCw, Play, Square } from 'lucide-react';

function jobBadge(status: string) {
  if (status === 'completed') return 'bg-emerald-900/50 text-emerald-400';
  if (status === 'failed') return 'bg-red-900/50 text-red-400';
  if (status === 'running') return 'bg-emerald-900/50 text-emerald-400';
  if (status === 'matched') return 'bg-amber-900/50 text-amber-400';
  if (status === 'disputed' || status === 'cancelled') return 'bg-red-900/50 text-red-400';
  return 'bg-blue-900/50 text-blue-400';
}

const CHAINS = ['lightning', 'eth-base', 'eth-arbitrum', 'eth-optimism', 'eth-polygon', 'solana'];

export default function ComputePage() {
  const [jobs, setJobs] = useState<any[]>([]);
  const [offers, setOffers] = useState<any[]>([]);
  const [workerActive, setWorkerActive] = useState(false);
  const [workerBusy, setWorkerBusy] = useState(false);
  const [error, setError] = useState('');
  const [repos, setRepos] = useState<any[]>([]);
  const [repo, setRepo] = useState('');
  const [commit, setCommit] = useState('HEAD');
  const [sats, setSats] = useState('10000');
  const [chain, setChain] = useState('lightning');
  const [yaml, setYaml] = useState('');
  const [yamlHint, setYamlHint] = useState('Prefilled from the selected repo');
  const [submitBusy, setSubmitBusy] = useState(false);
  const [cancelBusy, setCancelBusy] = useState<string | null>(null);
  const [submitError, setSubmitError] = useState('');
  const [submitResult, setSubmitResult] = useState<{ id: string; status: string; warning?: string } | null>(null);
  const [credit, setCredit] = useState<{ given_secs: number; taken_secs: number; own_deficit_secs: number; allowance_secs: number; may_spend: boolean } | null>(null);
  const [jobLogs, setJobLogs] = useState<Record<string, { loading: boolean; exitCode?: number; usage?: string; stdout?: string; stderr?: string; error?: string }>>({});

  const load = useCallback(async () => {
    let j: any; let o: any; let w: any; let c: any;
    try {
      [j, o, w, c] = await Promise.all([
        window.gf.api('/api/compute/jobs'),
        window.gf.api('/api/compute/offers'),
        window.gf.api('/api/compute/worker'),
        window.gf.api('/api/compute/credit'),
      ]);
    } catch (e) {
      console.error('[gf:compute] load threw', e);
      return;
    }
    if (j.ok) setJobs(Array.isArray(j.data) ? j.data : j.data?.jobs || []);
    else console.error('[gf:compute] /api/compute/jobs failed', { status: j.status, error: j.error });
    if (o.ok) setOffers(Array.isArray(o.data) ? o.data : o.data?.offers || []);
    else console.error('[gf:compute] /api/compute/offers failed', { status: o.status, error: o.error });
    if (w.ok && typeof w.data?.worker_enabled === 'boolean') setWorkerActive(w.data.worker_enabled);
    else if (!w.ok) console.error('[gf:compute] /api/compute/worker failed', { status: w.status, error: w.error });
    if (c.ok && c.data && typeof c.data === 'object') setCredit(c.data as any);
    else if (!c.ok) console.error('[gf:compute] /api/compute/credit failed', { status: c.status, error: c.error });
  }, []);

  useEffect(() => { load(); const t = setInterval(load, 8000); return () => clearInterval(t); }, [load]);

  useEffect(() => {
    window.gf.api('/api/repos').then((r: any) => {
      if (!r.ok) {
        console.error('[gf:compute] repo list for submit form failed', { status: r.status, error: r.error });
        return;
      }
      const list = Array.isArray(r.data) ? r.data : r.data?.repos || [];
      setRepos(list);
      if (list.length > 0) setRepo(list[0].name);
    }).catch((e) => console.error('[gf:compute] repo list for submit form threw', e));
  }, []);

  useEffect(() => {
    if (!repo) return;
    window.gf.api(`/api/repos/${encodeURIComponent(repo)}/ci-config`).then((r: any) => {
      if (r.ok && typeof r.data?.yaml === 'string') {
        setYaml(r.data.yaml);
        setYamlHint('Prefilled from the selected repo');
      } else {
        setYaml('');
        setYamlHint('No .gitforest-ci.yml in this repo — paste job config here');
      }
    }).catch((e) => console.error('[gf:compute] ci-config prefill threw', e));
  }, [repo]);

  const submitJob = async () => {
    if (!repo || submitBusy) return;
    setSubmitBusy(true);
    setSubmitError('');
    setSubmitResult(null);
    console.log('[gf:compute] submit job', { repo, commit: commit || 'HEAD', sats, chain });
    try {
      const res = await window.gf.api('/api/compute/jobs', {
        method: 'POST',
        body: {
          repo_name: repo,
          commit_sha: commit || 'HEAD',
          job_config_yaml: yaml,
          max_payment_sats: parseInt(sats, 10) || 0,
          preferred_chain: chain,
        },
      });
      if (res.ok) {
        console.log('[gf:compute] submit job ok', res.data);
        setSubmitResult({ id: res.data?.id || '?', status: res.data?.status || 'Pending', warning: res.data?.warning });
        load();
      } else if (res.status === 402) {
        console.error('[gf:compute] submit job failed: credit exhausted', res.data);
        setSubmitError('Mutual credit exhausted — offer this machine as a worker to earn credit, or configure a money backend.');
      } else {
        console.error('[gf:compute] submit job failed', { status: res.status, error: res.error, data: res.data });
        setSubmitError(res.data?.message || res.error || `Request failed (HTTP ${res.status})`);
      }
    } catch (e: any) {
      console.error('[gf:compute] submit job threw', e);
      setSubmitError(e?.message || 'Request failed');
    } finally {
      setSubmitBusy(false);
    }
  };

  const cancelJob = async (jobId: string) => {
    if (!jobId || cancelBusy) return;
    setCancelBusy(jobId);
    console.log('[gf:compute] cancel job', { jobId });
    try {
      const res = await window.gf.api(`/api/compute/jobs/${encodeURIComponent(jobId)}/cancel`, {
        method: 'POST',
      });
      if (!res.ok) {
        console.error('[gf:compute] cancel job failed', { jobId, status: res.status, error: res.error, data: res.data });
        setError(res.data?.message || res.error || `Cancel failed (HTTP ${res.status})`);
      } else {
        console.log('[gf:compute] cancel job ok', { jobId });
        setError('');
      }
      load();
    } catch (e: any) {
      console.error('[gf:compute] cancel job threw', { jobId, error: e });
      setError(e?.message || 'Cancel failed');
    } finally {
      setCancelBusy(null);
    }
  };

  const toggleLogs = async (jobId: string) => {
    if (!jobId) return;
    if (jobLogs[jobId] && !jobLogs[jobId].loading) {
      setJobLogs((prev) => {
        const next = { ...prev };
        delete next[jobId];
        return next;
      });
      return;
    }
    setJobLogs((prev) => ({ ...prev, [jobId]: { loading: true } }));
    try {
      const res = await window.gf.api(`/api/compute/jobs/${encodeURIComponent(jobId)}`);
      const result = res.ok ? (res.data as any)?.result : null;
      if (!result) {
        if (!res.ok) console.error('[gf:compute] job detail failed', { jobId, status: res.status, error: res.error });
        setJobLogs((prev) => ({ ...prev, [jobId]: { loading: false, error: 'No result yet' } }));
        return;
      }
      const cap = (s: unknown): string | undefined => {
        if (typeof s !== 'string' || s.length === 0) return undefined;
        return s.length > 65536 ? s.slice(-65536) : s;
      };
      const usage = result.resource_usage
        ? `${Number(result.resource_usage.peak_cpu_percent ?? 0).toFixed(1)}% CPU · ${result.resource_usage.peak_ram_mb ?? 0} MB RAM · ${result.resource_usage.disk_used_mb ?? 0} MB disk`
        : undefined;
      setJobLogs((prev) => ({
        ...prev,
        [jobId]: { loading: false, exitCode: result.exit_code, usage, stdout: cap(result.stdout_tail), stderr: cap(result.stderr_tail) },
      }));
    } catch (e: any) {
      setJobLogs((prev) => ({ ...prev, [jobId]: { loading: false, error: e?.message || 'Failed to load logs' } }));
    }
  };

  const toggleWorker = async () => {
    setWorkerBusy(true);
    setError('');
    const action = workerActive ? 'stop' : 'start';
    console.log('[gf:compute] worker toggle', { action });
    try {
      const route = workerActive ? '/api/compute/worker/stop' : '/api/compute/worker/start';
      const res = await window.gf.api(route, { method: 'POST', body: workerActive ? {} : { cpu: 4, ram_mb: 8192, rate_sats: 1000 } });
      if (res.ok && res.data?.status !== 'error') {
        console.log('[gf:compute] worker toggle ok', { action, data: res.data });
        if (typeof res.data?.worker_enabled === 'boolean') setWorkerActive(res.data.worker_enabled);
        else setWorkerActive(!workerActive);
      } else {
        console.error('[gf:compute] worker toggle failed', { action, status: res.status, error: res.error, data: res.data });
        setError(res.data?.message || res.error || `Request failed (HTTP ${res.status})`);
      }
    } catch (e: any) {
      console.error('[gf:compute] worker toggle threw', { action, error: e });
      setError(e?.message || 'Request failed');
    } finally {
      setWorkerBusy(false);
    }
  };

  return (
    <div className="p-6">
      <div className="flex items-center justify-between mb-6">
        <h1 className="text-xl font-bold theme-text-primary flex items-center gap-2">
          <Cpu size={20} className="text-emerald-500" /> Compute marketplace
        </h1>
        <div className="flex items-center gap-2">
          <span className="text-[10px] theme-text-muted bg-amber-900/40 text-amber-400 border border-amber-800 rounded px-2 py-0.5">Experimental</span>
          <button onClick={load} className="p-2 theme-text-muted hover:theme-text-primary"><RefreshCw size={16} /></button>
        </div>
      </div>

      <div className="theme-bg-secondary rounded border theme-border p-4 mb-6 flex items-center justify-between">
        <div>
          <h3 className="text-sm font-semibold theme-text-primary">Offer this machine as a worker</h3>
          <p className="text-xs theme-text-muted mt-1">Runs CI jobs from the network in sandboxed Docker containers.</p>
          {error && <p className="text-xs text-red-400 mt-2">{error}</p>}
        </div>
        <button onClick={toggleWorker} disabled={workerBusy}
          className={`flex items-center gap-2 text-sm font-medium rounded px-4 py-2 disabled:opacity-50 ${workerActive ? 'bg-red-600 hover:bg-red-700 text-white' : 'bg-emerald-600 hover:bg-emerald-700 text-white'}`}>
          {workerActive ? <><Square size={14} /> Stop offering</> : <><Play size={14} /> Start offering</>}
        </button>
      </div>

      {credit && (
        <div className="theme-bg-secondary rounded border theme-border p-4 mb-6 flex items-center justify-between">
          <div>
            <h3 className="text-sm font-semibold theme-text-primary">Mutual credit</h3>
            <p className="text-xs theme-text-muted mt-1">
              Given {credit.given_secs}s · Taken {credit.taken_secs}s · Deficit {credit.own_deficit_secs}s / {credit.allowance_secs}s
            </p>
          </div>
          <span className={`text-xs px-2 py-0.5 rounded ${credit.may_spend ? 'bg-emerald-900/50 text-emerald-400' : 'bg-red-900/50 text-red-400'}`}>
            {credit.may_spend ? 'may submit' : 'exhausted'}
          </span>
        </div>
      )}

      <div className="theme-bg-secondary rounded border theme-border p-4 mb-6">
        <h3 className="text-sm font-semibold theme-text-primary mb-3">Submit a job</h3>
        {repos.length === 0 ? (
          <p className="text-xs theme-text-muted">Add a repo first to submit compute jobs.</p>
        ) : (
          <>
            <div className="grid grid-cols-2 md:grid-cols-4 gap-3 mb-3">
              <label className="block">
                <span className="text-xs theme-text-muted">Repo</span>
                <select value={repo} onChange={(e) => setRepo(e.target.value)}
                  className="mt-1 w-full theme-bg-tertiary border theme-border rounded px-3 py-2 text-sm theme-text-primary outline-none focus:border-emerald-500">
                  {repos.map((r: any) => <option key={r.name} value={r.name}>{r.name}</option>)}
                </select>
              </label>
              <label className="block">
                <span className="text-xs theme-text-muted">Commit</span>
                <input value={commit} onChange={(e) => setCommit(e.target.value)} spellCheck={false}
                  className="mt-1 w-full theme-bg-tertiary border theme-border rounded px-3 py-2 text-sm theme-text-primary outline-none focus:border-emerald-500" />
              </label>
              <label className="block">
                <span className="text-xs theme-text-muted">Max payment (sats)</span>
                <input value={sats} onChange={(e) => setSats(e.target.value)} type="number" min="0"
                  className="mt-1 w-full theme-bg-tertiary border theme-border rounded px-3 py-2 text-sm theme-text-primary outline-none focus:border-emerald-500" />
              </label>
              <label className="block">
                <span className="text-xs theme-text-muted">Chain</span>
                <select value={chain} onChange={(e) => setChain(e.target.value)}
                  className="mt-1 w-full theme-bg-tertiary border theme-border rounded px-3 py-2 text-sm theme-text-primary outline-none focus:border-emerald-500">
                  {CHAINS.map((c) => <option key={c} value={c}>{c}</option>)}
                </select>
              </label>
            </div>
            <span className="text-xs theme-text-muted">Job config (.gitforest-ci.yml)</span>
            <textarea value={yaml} onChange={(e) => setYaml(e.target.value)} spellCheck={false} rows={6}
              placeholder={yamlHint}
              className="mt-1 w-full theme-bg-tertiary border theme-border rounded px-3 py-2 text-xs font-mono theme-text-primary outline-none focus:border-emerald-500 mb-3" />
            <div className="flex items-center gap-3">
              <button onClick={submitJob} disabled={submitBusy || !repo}
                className="bg-emerald-600 hover:bg-emerald-700 disabled:opacity-50 text-white text-sm font-medium rounded px-4 py-2">
                {submitBusy ? 'Submitting…' : 'Submit job'}
              </button>
              {submitError && <p className="text-xs text-red-400">{submitError}</p>}
            </div>
            {submitResult && (
              <div className="mt-3 text-xs theme-text-muted">
                <p>Job submitted: <code className="theme-text-primary">{submitResult.id}</code> ({submitResult.status})</p>
                {submitResult.warning && <p className="text-amber-400 mt-1">Warning: {submitResult.warning}</p>}
              </div>
            )}
          </>
        )}
      </div>

      <div className="theme-bg-secondary rounded border theme-border mb-6">
        <h3 className="text-sm font-semibold theme-text-primary px-4 py-3 border-b theme-border">Jobs</h3>
        {jobs.length === 0 && <p className="text-xs theme-text-muted p-4">No compute jobs yet.</p>}
        {jobs.map((j: any, i: number) => (
          <div key={j.id || i} className="px-4 py-2.5 border-b theme-border last:border-0">
            <div className="flex items-center justify-between">
              <div className="min-w-0">
                <p className="text-sm theme-text-primary truncate">{j.name || j.repo || j.id}</p>
                <p className="text-xs theme-text-muted">{j.docker_image || j.image || ''}</p>
              </div>
              <div className="flex items-center gap-2">
                {['completed', 'failed'].includes((j.status || '').toLowerCase()) && (
                  <button onClick={() => toggleLogs(j.id)}
                    className="text-xs px-2 py-0.5 rounded border theme-border theme-text-muted hover:theme-text-primary">
                    {jobLogs[j.id] ? 'Hide logs' : 'Logs'}
                  </button>
                )}
                {['pending', 'matched', 'verifying', 'running'].includes((j.status || 'pending').toLowerCase()) && (
                  <button onClick={() => cancelJob(j.id)} disabled={cancelBusy === j.id}
                    className="text-xs px-2 py-0.5 rounded border border-red-500 text-red-400 hover:bg-red-500 hover:text-white disabled:opacity-50">
                    {cancelBusy === j.id ? 'Cancelling…' : 'Cancel'}
                  </button>
                )}
                <span className={`text-xs px-2 py-0.5 rounded ${jobBadge(j.status || 'pending')}`}>{j.status || 'pending'}</span>
              </div>
            </div>
            {jobLogs[j.id] && (
              <div className="mt-2">
                {jobLogs[j.id].loading && <p className="text-xs theme-text-muted">Loading logs…</p>}
                {jobLogs[j.id].error && <p className="text-xs text-red-400">{jobLogs[j.id].error}</p>}
                {jobLogs[j.id].exitCode !== undefined && (
                  <p className="text-xs theme-text-muted mb-1">
                    Exit code: {jobLogs[j.id].exitCode}
                    {jobLogs[j.id].usage && <> · Peak: {jobLogs[j.id].usage}</>}
                  </p>
                )}
                {jobLogs[j.id].stdout && (
                  <pre className="text-xs font-mono theme-text-primary theme-bg-tertiary border theme-border rounded p-2 max-h-48 overflow-auto whitespace-pre-wrap break-words">{jobLogs[j.id].stdout}</pre>
                )}
                {jobLogs[j.id].stderr && (
                  <pre className="text-xs font-mono text-red-300 theme-bg-tertiary border theme-border rounded p-2 mt-1 max-h-32 overflow-auto whitespace-pre-wrap break-words">{jobLogs[j.id].stderr}</pre>
                )}
              </div>
            )}
          </div>
        ))}
      </div>

      <div className="theme-bg-secondary rounded border theme-border">
        <h3 className="text-sm font-semibold theme-text-primary px-4 py-3 border-b theme-border">Workers on the network</h3>
        {offers.length === 0 && <p className="text-xs theme-text-muted p-4">No workers discovered.</p>}
        {offers.map((o: any, i: number) => (
          <div key={o.worker_id || i} className="px-4 py-2 border-b theme-border last:border-0 flex items-center justify-between">
            <code className="text-xs theme-text-primary truncate">{(o.worker_id || '').toString().slice(0, 24)}…</code>
            <span className="text-xs theme-text-muted">{o.cpu_cores ?? '?'} cores · {o.ram_mb ?? '?'} MB · {o.hourly_rate_sats ?? '?'} sats/h</span>
          </div>
        ))}
      </div>
    </div>
  );
}
