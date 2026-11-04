import React, { useState, useEffect, useCallback } from 'react';
import { GitBranch, Plus, RefreshCw, Copy, CheckCircle2, FolderOpen, Trash2, GitPullRequest, FileDiff, Check, X, Download } from 'lucide-react';

interface Repo { name: string; path: string; description?: string; seeding?: boolean }

function networkRepoKey(r: any): string {
  return r?.history_id ? `history:${r.name}:${r.history_id}` : `${r?.owner || ''}/${r?.name || ''}`;
}

function shortHash(v: unknown, chars = 12): string {
  const s = String(v || '');
  return s ? `${s.slice(0, chars)}…` : '–';
}

export default function ReposPage() {
  const [repos, setRepos] = useState<Repo[]>([]);
  const [selected, setSelected] = useState<Repo | null>(null);
  const [detail, setDetail] = useState<any>(null);
  const [branches, setBranches] = useState<string[]>([]);
  const [log, setLog] = useState<any[]>([]);
  const [mrs, setMrs] = useState<any[]>([]);
  const [peerId, setPeerId] = useState('');
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState(false);
  const [mrMsg, setMrMsg] = useState<string | null>(null);
  const [showClone, setShowClone] = useState(false);
  const [cloneInputUrl, setCloneInputUrl] = useState('');
  const [cloneDest, setCloneDest] = useState('');
  const [cloneMsg, setCloneMsg] = useState<string | null>(null);
  const [tab, setTab] = useState<'hosted' | 'network'>('hosted');
  const [netRepos, setNetRepos] = useState<any[]>([]);
  const [netMeta, setNetMeta] = useState<any>(null);
  const [netLoading, setNetLoading] = useState(false);
  const [selectedNet, setSelectedNet] = useState<any | null>(null);
  const [cloningNet, setCloningNet] = useState(false);
  const [cloneNetErr, setCloneNetErr] = useState<string | null>(null);

  const cloneFromNetwork = async (repo: any) => {
    console.log('[gf:repos] network clone started', { name: repo.name, owner: repo.owner, url: repo.clone_url });
    try {
      const pick = await window.gf.pickDirectory(`Choose where to clone ${repo.name}`);
      if (pick.canceled || !pick.filePaths?.[0]) {
        console.log('[gf:repos] network clone canceled at folder picker');
        return;
      }
      console.log('[gf:repos] network clone dest picked', { dest: pick.filePaths[0] });
      setCloningNet(true);
      setCloneNetErr(null);
      const res = await window.gf.api('/api/repos/clone', {
        method: 'POST',
        body: { url: repo.clone_url, dest: pick.filePaths[0], description: repo.description || undefined },
      });
      setCloningNet(false);
      if (!res.ok) {
        const msg = String(res.error || (res.data as any)?.error || `Clone failed (HTTP ${res.status})`);
        console.error('[gf:repos] network clone failed', { status: res.status, error: res.error, data: res.data });
        setCloneNetErr(msg);
        return;
      }
      console.log('[gf:repos] network clone ok', res.data);
      const name = (res.data as any)?.name || repo.name;
      setSelectedNet(null);
      setTab('hosted');
      const list = await load();
      const added = list.find((r) => r.name === name);
      if (added) {
        console.log('[gf:repos] network clone registered, opening', { name });
        openRepo(added);
      } else {
        console.error('[gf:repos] network clone missing from hosted list after reload', { name, hosted: list.map((r) => r.name) });
        setCloneNetErr(`Cloned but ${name} is not in the hosted list — check daemon log in Settings.`);
      }
    } catch (e) {
      setCloningNet(false);
      console.error('[gf:repos] network clone threw', e);
      setCloneNetErr(`Clone failed: ${(e as Error).message}`);
    }
  };

  const load = useCallback(async () => {
    let r: any; let id: any;
    try {
      [r, id] = await Promise.all([window.gf.api('/api/repos'), window.gf.api('/api/id')]);
    } catch (e) {
      console.error('[gf:repos] load threw', e);
      return [];
    }
    let list: Repo[] = [];
    if (r.ok) {
      list = Array.isArray(r.data) ? r.data : r.data?.repos || [];
      setRepos(list);
    } else {
      console.error('[gf:repos] /api/repos failed', { status: r.status, error: r.error });
    }
    if (id.ok) setPeerId(id.data?.peer_id || id.data || '');
    else console.error('[gf:repos] /api/id failed', { status: id.status, error: id.error });
    return list;
  }, []);

  const loadNetwork = useCallback(async () => {
    setNetLoading(true);
    const n = await window.gf.api('/api/network/repos');
    setNetLoading(false);
    if (n.ok) {
      const repos = Array.isArray(n.data?.repos) ? n.data.repos : [];
      console.log('[gf:repos] network scan ok', {
        repos: repos.length,
        copies: repos.reduce((n: number, r: any) => n + (r.copies || 1), 0),
        bootstraps: Array.isArray(n.data?.bootstraps)
          ? n.data.bootstraps.map((b: any) => `${b.ip}:${b.ok ? 'up' : 'down'}`).join(',')
          : '?',
      });
      setNetRepos(repos);
      setNetMeta(n.data || null);
    } else {
      console.error('[gf:repos] network scan failed', { status: n.status, error: n.error });
    }
  }, []);

  useEffect(() => { load(); const t = setInterval(load, 15000); return () => clearInterval(t); }, [load]);
  useEffect(() => { if (tab === 'network') loadNetwork(); }, [tab, loadNetwork]);

  const openRepo = async (repo: Repo) => {
    setSelected(repo);
    setSelectedNet(null);
    setBusy(true);
    const [d, b, l, m] = await Promise.all([
      window.gf.api(`/api/repos/${encodeURIComponent(repo.name)}`),
      window.gf.api(`/api/repos/${encodeURIComponent(repo.name)}/branches`),
      window.gf.api(`/api/repos/${encodeURIComponent(repo.name)}/log`),
      window.gf.api(`/api/repos/${encodeURIComponent(repo.name)}/merge-requests`),
    ]);
    setDetail(d.ok ? d.data : null);
    setBranches(b.ok ? (Array.isArray(b.data) ? b.data : b.data?.branches || []) : []);
    setLog(l.ok ? (Array.isArray(l.data) ? l.data : l.data?.commits || []) : []);
    setMrs(m.ok ? (Array.isArray(m.data) ? m.data : m.data?.merge_requests || []) : []);
    setBusy(false);
  };

  const addRepo = async () => {
    try {
      const pick = await window.gf.pickDirectory();
      if (pick.canceled || !pick.filePaths?.[0]) return;
      const p = pick.filePaths[0];
      const name = p.split('/').pop() || 'repo';
      console.log('[gf:repos] add repo', { path: p, name });
      const res = await window.gf.api('/api/repos', { method: 'POST', body: { path: p, name } });
      if (res.ok) {
        console.log('[gf:repos] add repo ok', res.data);
        load();
      } else {
        console.error('[gf:repos] add repo failed', { status: res.status, error: res.error, data: res.data });
      }
    } catch (e) {
      console.error('[gf:repos] add repo threw', e);
    }
  };

  const pickCloneDest = async () => {
    const pick = await window.gf.pickDirectory();
    if (!pick.canceled && pick.filePaths?.[0]) setCloneDest(pick.filePaths[0]);
  };

  const cloneRepo = async () => {
    if (!cloneInputUrl.trim() || !cloneDest.trim()) {
      setCloneMsg('Paste a gitforest:// URL and pick a destination folder.');
      return;
    }
    setBusy(true);
    setCloneMsg(null);
    console.log('[gf:repos] manual clone started', { url: cloneInputUrl.trim(), dest: cloneDest.trim() });
    try {
      const res = await window.gf.api('/api/repos/clone', { method: 'POST', body: { url: cloneInputUrl.trim(), dest: cloneDest.trim() } });
      setBusy(false);
      if (!res.ok) {
        console.error('[gf:repos] manual clone failed', { status: res.status, error: res.error, data: res.data });
        setCloneMsg(String(res.error || (res.data as any)?.error || `Clone failed (HTTP ${res.status})`));
        return;
      }
      console.log('[gf:repos] manual clone ok', res.data);
    } catch (e) {
      setBusy(false);
      console.error('[gf:repos] manual clone threw', e);
      setCloneMsg(`Clone failed: ${(e as Error).message}`);
      return;
    }
    setShowClone(false);
    setCloneInputUrl('');
    setCloneMsg(null);
    setSelectedNet(null);
    setTab('hosted');
    load();
  };

  const removeRepo = async (name: string) => {
    if (!confirm(`Stop hosting "${name}"? (repo files stay on disk)`)) return;
    console.log('[gf:repos] stop hosting', { name });
    const res = await window.gf.api(`/api/repos/${encodeURIComponent(name)}`, { method: 'DELETE' });
    if (!res.ok) console.error('[gf:repos] stop hosting failed', { name, status: res.status, error: res.error });
    setSelected(null);
    load();
  };

  const actOnMr = async (id: string, action: 'accept' | 'reject') => {
    if (!selected) return;
    setMrMsg(null);
    const res = await window.gf.api(`/api/repos/${encodeURIComponent(selected.name)}/merge-requests/${encodeURIComponent(id)}/${action}`, { method: 'POST' });
    if (!res.ok) {
      setMrMsg(res.error || res.data?.error || `${action} failed`);
      return;
    }
    openRepo(selected);
  };

  const cloneUrl = selected ? `gitforest://${peerId}/${selected.name}` : '';
  const netCopyCount = netRepos.reduce((n: number, r: any) => n + (r.copies || (Array.isArray(r.owners) ? r.owners.length : 1) || 1), 0);

  return (
    <div className="flex h-full">
      <div className="w-72 flex-shrink-0 border-r theme-border overflow-auto">
        <div className="flex items-center justify-between px-4 py-3 border-b theme-border">
          <div className="flex gap-3">
            <button onClick={() => setTab('hosted')}
              className={`text-sm font-semibold ${tab === 'hosted' ? 'theme-text-primary' : 'theme-text-muted hover:theme-text-primary'}`}>
              Hosted
            </button>
            <button onClick={() => setTab('network')}
              className={`text-sm font-semibold ${tab === 'network' ? 'theme-text-primary' : 'theme-text-muted hover:theme-text-primary'}`}>
              Network
            </button>
          </div>
          <div className="flex gap-1">
            <button onClick={tab === 'hosted' ? load : loadNetwork} className="p-1.5 theme-text-muted hover:theme-text-primary"><RefreshCw size={14} /></button>
            <button onClick={() => { setShowClone(!showClone); setCloneMsg(null); }} className="p-1.5 theme-text-muted hover:theme-text-primary" title="Clone from URL"><Download size={14} /></button>
            {tab === 'hosted' && (
              <button onClick={addRepo} className="p-1.5 theme-text-muted hover:theme-text-primary" title="Add repo"><Plus size={14} /></button>
            )}
          </div>
        </div>
        {showClone && (
          <div className="px-4 py-3 border-b theme-border space-y-2">
            <input value={cloneInputUrl} onChange={(e) => setCloneInputUrl(e.target.value)} spellCheck={false}
              placeholder="gitforest://peer-id/repo"
              className="w-full text-xs theme-bg-tertiary theme-text-primary rounded px-2.5 py-1.5 outline-none" />
            <div className="flex gap-2">
              <input value={cloneDest} onChange={(e) => setCloneDest(e.target.value)} spellCheck={false}
                placeholder="Destination folder"
                className="flex-1 min-w-0 text-xs theme-bg-tertiary theme-text-primary rounded px-2.5 py-1.5 outline-none" />
              <button onClick={pickCloneDest} className="p-1.5 theme-text-muted hover:theme-text-primary" title="Pick folder"><FolderOpen size={14} /></button>
            </div>
            {cloneMsg && <p className="text-xs text-red-500">{cloneMsg}</p>}
            <div className="flex gap-2">
              <button onClick={cloneRepo} disabled={busy} className="text-xs bg-emerald-600 hover:bg-emerald-700 disabled:opacity-50 text-white rounded px-2.5 py-1">Clone</button>
              <button onClick={() => setShowClone(false)} className="text-xs theme-text-muted hover:theme-text-primary rounded px-2.5 py-1">Cancel</button>
            </div>
          </div>
        )}
        {tab === 'hosted' && repos.length === 0 && <p className="text-xs theme-text-muted p-4">No repos hosted. Click + to add one.</p>}
        {tab === 'hosted' && repos.map((r) => (
          <button key={r.name} onClick={() => openRepo(r)}
            className={`w-full text-left px-4 py-3 border-b theme-border ${selected?.name === r.name ? 'theme-bg-tertiary' : 'hover:theme-bg-tertiary'}`}>
            <div className="flex items-center gap-2">
              <GitBranch size={14} className="text-emerald-500 flex-shrink-0" />
              <span className="text-sm theme-text-primary truncate">{r.name}</span>
            </div>
            {r.description && <p className="text-xs theme-text-muted truncate mt-0.5 pl-6">{r.description}</p>}
          </button>
        ))}
        {tab === 'network' && netMeta && (
          <p className="text-xs theme-text-muted px-4 pt-3">
            {netRepos.length} repos{netCopyCount > netRepos.length ? ` · ${netCopyCount} copies` : ''} · {Array.isArray(netMeta.bootstraps) ? netMeta.bootstraps.filter((b: any) => b.ok).length : 0}/{Array.isArray(netMeta.bootstraps) ? netMeta.bootstraps.length : 0} bootstraps up
          </p>
        )}
        {tab === 'network' && netLoading && netRepos.length === 0 && <p className="text-xs theme-text-muted p-4">Scanning bootstrap nodes…</p>}
        {tab === 'network' && !netLoading && netRepos.length === 0 && <p className="text-xs theme-text-muted p-4">No network repos found. Are the bootstraps reachable?</p>}
        {tab === 'network' && netRepos.map((r: any) => {
          const copies = r.copies || (Array.isArray(r.owners) ? r.owners.length : 1);
          return (
            <button key={networkRepoKey(r)} onClick={() => { setSelected(null); setSelectedNet(r); setCloneNetErr(null); }}
              className={`w-full text-left px-4 py-3 border-b theme-border ${selectedNet && networkRepoKey(selectedNet) === networkRepoKey(r) ? 'theme-bg-tertiary' : 'hover:theme-bg-tertiary'}`}>
              <div className="flex items-center justify-between gap-2">
                <div className="flex items-center gap-2 min-w-0">
                  <GitBranch size={14} className="text-emerald-500 flex-shrink-0" />
                  <span className="text-sm theme-text-primary truncate">{r.name}</span>
                </div>
                <span className="text-xs theme-text-muted flex-shrink-0">{r.seeders ?? 0} seeder{(r.seeders ?? 0) === 1 ? '' : 's'}</span>
              </div>
              <p className="text-xs theme-text-muted truncate mt-0.5 pl-6">
                {String(r.owner || '').slice(0, 12)}…{copies > 1 ? ` · ${copies} copies` : ''}{r.head_sha ? ` · HEAD ${shortHash(r.head_sha, 8)}` : ''}{r.description ? ` · ${r.description}` : ''}
              </p>
            </button>
          );
        })}
      </div>

      <div className="flex-1 overflow-auto p-6">
        {!selected && !selectedNet && (
          <div className="h-full flex items-center justify-center theme-text-muted text-sm">Select a repo, or add one with the + button.</div>
        )}
        {!selected && selectedNet && (
          <>
            <div className="flex items-center justify-between mb-6">
              <div>
                <h1 className="text-xl font-bold theme-text-primary">{selectedNet.name}</h1>
                <div className="flex items-center gap-2 mt-1">
                  <code className="text-xs theme-text-muted theme-bg-tertiary rounded px-2 py-0.5 truncate max-w-md">{selectedNet.clone_url}</code>
                  <button onClick={() => { navigator.clipboard.writeText(selectedNet.clone_url || ''); setCopied(true); setTimeout(() => setCopied(false), 1500); }}
                    className="theme-text-muted hover:theme-text-primary">
                    {copied ? <CheckCircle2 size={14} className="text-emerald-500" /> : <Copy size={14} />}
                  </button>
                </div>
              </div>
              <div className="flex flex-col items-end gap-2">
                <button onClick={() => cloneFromNetwork(selectedNet)} disabled={cloningNet}
                  className="flex items-center gap-1.5 text-xs bg-emerald-600 hover:bg-emerald-700 disabled:opacity-50 text-white rounded px-3 py-1.5">
                  <Download size={12} /> {cloningNet ? 'Cloning…' : 'Clone'}
                </button>
                {cloneNetErr && <p className="text-xs text-red-500 max-w-xs text-right">{cloneNetErr}</p>}
              </div>
            </div>

            <div className="grid grid-cols-2 lg:grid-cols-4 gap-4 mb-6">
              <div className="theme-bg-secondary rounded border theme-border p-4">
                <p className="text-xs theme-text-muted mb-1">Seeders</p>
                <p className="text-2xl font-semibold theme-text-primary">{selectedNet.seeders ?? 0}</p>
              </div>
              <div className="theme-bg-secondary rounded border theme-border p-4">
                <p className="text-xs theme-text-muted mb-1">Copies</p>
                <p className="text-2xl font-semibold theme-text-primary">{selectedNet.copies || (Array.isArray(selectedNet.owners) ? selectedNet.owners.length : 1)}</p>
              </div>
              <div className="theme-bg-secondary rounded border theme-border p-4">
                <p className="text-xs theme-text-muted mb-1">Owner</p>
                <p className="text-sm font-mono theme-text-primary truncate">{String(selectedNet.owner || '')}</p>
              </div>
              <div className="theme-bg-secondary rounded border theme-border p-4">
                <p className="text-xs theme-text-muted mb-1">Seen on</p>
                <p className="text-sm theme-text-primary">{Array.isArray(selectedNet.seen_on) ? selectedNet.seen_on.join(', ') : '–'}</p>
              </div>
            </div>

            {(selectedNet.head_sha || selectedNet.history_id) && (
              <div className="theme-bg-secondary rounded border theme-border mb-6">
                <h3 className="text-sm font-semibold theme-text-primary px-4 py-3 border-b theme-border">Shared git history</h3>
                <div className="p-4 space-y-1.5 text-xs theme-text-muted">
                  {selectedNet.head_sha && <p>HEAD: <code className="theme-text-primary">{String(selectedNet.head_sha)}</code></p>}
                  {selectedNet.history_id && <p>History: <code className="theme-text-primary">{shortHash(selectedNet.history_id, 16)}</code></p>}
                  <p>Copies are grouped when they point at the same HEAD commit; without a HEAD, their branch and tag refs must match exactly.</p>
                </div>
              </div>
            )}

            {Array.isArray(selectedNet.clone_urls) && selectedNet.clone_urls.length > 1 && (
              <div className="theme-bg-secondary rounded border theme-border mb-6">
                <h3 className="text-sm font-semibold theme-text-primary px-4 py-3 border-b theme-border">Clone from a copy</h3>
                {selectedNet.clone_urls.map((url: string) => (
                  <div key={url} className="flex items-center justify-between gap-3 px-4 py-2.5 border-b theme-border last:border-0">
                    <code className="text-xs theme-text-primary truncate">{url}</code>
                    <button onClick={() => cloneFromNetwork({ ...selectedNet, clone_url: url })} disabled={cloningNet}
                      className="flex-shrink-0 flex items-center gap-1.5 text-xs theme-bg-tertiary hover:theme-bg-hover disabled:opacity-50 theme-text-secondary rounded px-2.5 py-1">
                      <Download size={12} /> Clone
                    </button>
                  </div>
                ))}
                {Array.isArray(selectedNet.owners) && selectedNet.owners.length > 1 && (
                  <p className="text-xs theme-text-muted px-4 py-3 border-t theme-border">
                    Owners: {selectedNet.owners.map((owner: string) => owner.slice(0, 12) + '…').join(', ')}
                  </p>
                )}
              </div>
            )}

            {selectedNet.description && (
              <div className="theme-bg-secondary rounded border theme-border">
                <h3 className="text-sm font-semibold theme-text-primary px-4 py-3 border-b theme-border">About</h3>
                <p className="text-sm theme-text-secondary p-4">{selectedNet.description}</p>
              </div>
            )}
          </>
        )}
        {selected && (
          <>
            <div className="flex items-center justify-between mb-6">
              <div>
                <h1 className="text-xl font-bold theme-text-primary">{selected.name}</h1>
                <div className="flex items-center gap-2 mt-1">
                  <code className="text-xs theme-text-muted theme-bg-tertiary rounded px-2 py-0.5 truncate max-w-md">{cloneUrl}</code>
                  <button onClick={() => { navigator.clipboard.writeText(cloneUrl); setCopied(true); setTimeout(() => setCopied(false), 1500); }}
                    className="theme-text-muted hover:theme-text-primary">
                    {copied ? <CheckCircle2 size={14} className="text-emerald-500" /> : <Copy size={14} />}
                  </button>
                </div>
              </div>
              <button onClick={() => removeRepo(selected.name)} className="flex items-center gap-1.5 text-xs text-red-400 hover:text-red-300 border border-red-900 rounded px-3 py-1.5">
                <Trash2 size={12} /> Stop hosting
              </button>
            </div>

            <div className="grid grid-cols-3 gap-4 mb-6">
              <div className="theme-bg-secondary rounded border theme-border p-4">
                <p className="text-xs theme-text-muted mb-1">Branches</p>
                <p className="text-2xl font-semibold theme-text-primary">{branches.length}</p>
              </div>
              <div className="theme-bg-secondary rounded border theme-border p-4">
                <p className="text-xs theme-text-muted mb-1">Commits</p>
                <p className="text-2xl font-semibold theme-text-primary">{log.length}</p>
              </div>
              <div className="theme-bg-secondary rounded border theme-border p-4">
                <p className="text-xs theme-text-muted mb-1">Merge requests</p>
                <p className="text-2xl font-semibold theme-text-primary">{mrs.length}</p>
              </div>
            </div>

            {mrs.length > 0 && (
              <div className="theme-bg-secondary rounded border theme-border mb-6">
                <h3 className="text-sm font-semibold theme-text-primary px-4 py-3 border-b theme-border flex items-center gap-2">
                  <GitPullRequest size={14} className="text-emerald-500" /> Merge requests
                </h3>
                {mrMsg && <p className="text-xs text-red-500 px-4 py-2 border-b theme-border">{mrMsg}</p>}
                {mrs.map((mr: any, i: number) => (
                  <div key={mr.id || i} className="flex items-center justify-between px-4 py-2.5 border-b theme-border last:border-0">
                    <div className="min-w-0">
                      <p className="text-sm theme-text-primary truncate">{mr.title || mr.message || mr.id}</p>
                      <p className="text-xs theme-text-muted truncate">from {(mr.author || mr.from_peer || '').slice(0, 20)}…{mr.status && mr.status !== 'open' ? ` · ${mr.status}` : ''}</p>
                    </div>
                    <div className="flex gap-2 flex-shrink-0">
                      <button onClick={() => actOnMr(mr.id || i, 'accept')} className="flex items-center gap-1 text-xs bg-emerald-600 hover:bg-emerald-700 text-white rounded px-2.5 py-1"><Check size={12} /> Accept</button>
                      <button onClick={() => actOnMr(mr.id || i, 'reject')} className="flex items-center gap-1 text-xs theme-bg-tertiary hover:theme-bg-hover theme-text-secondary rounded px-2.5 py-1"><X size={12} /> Reject</button>
                    </div>
                  </div>
                ))}
              </div>
            )}

            <div className="theme-bg-secondary rounded border theme-border">
              <h3 className="text-sm font-semibold theme-text-primary px-4 py-3 border-b theme-border flex items-center gap-2">
                <FileDiff size={14} className="text-emerald-500" /> Recent commits
              </h3>
              {busy && <p className="text-xs theme-text-muted p-4">Loading…</p>}
              {!busy && log.length === 0 && <p className="text-xs theme-text-muted p-4">No commits found.</p>}
              {log.slice(0, 20).map((c: any, i: number) => (
                <div key={c.sha || c.id || i} className="px-4 py-2 border-b theme-border last:border-0">
                  <p className="text-sm theme-text-primary truncate">{c.message || '(no message)'}</p>
                  <p className="text-xs theme-text-muted">{(c.sha || c.id || '').slice(0, 10)} · {c.author || ''}</p>
                </div>
              ))}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
