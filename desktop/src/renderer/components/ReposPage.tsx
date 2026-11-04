import React, { useState, useEffect, useCallback } from 'react';
import { GitBranch, Plus, RefreshCw, Copy, CheckCircle2, FolderOpen, Trash2, GitPullRequest, FileDiff, Check, X, Download } from 'lucide-react';

interface Repo { name: string; path: string; description?: string; seeding?: boolean }

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

  const load = useCallback(async () => {
    const [r, id] = await Promise.all([window.gf.api('/api/repos'), window.gf.api('/api/id')]);
    if (r.ok) setRepos(Array.isArray(r.data) ? r.data : r.data?.repos || []);
    if (id.ok) setPeerId(id.data?.peer_id || id.data || '');
  }, []);

  useEffect(() => { load(); const t = setInterval(load, 15000); return () => clearInterval(t); }, [load]);

  const openRepo = async (repo: Repo) => {
    setSelected(repo);
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
    const pick = await window.gf.pickDirectory();
    if (pick.canceled || !pick.filePaths?.[0]) return;
    const p = pick.filePaths[0];
    const name = p.split('/').pop() || 'repo';
    const res = await window.gf.api('/api/repos', { method: 'POST', body: { path: p, name } });
    if (res.ok) load();
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
    const res = await window.gf.api('/api/repos/clone', { method: 'POST', body: { url: cloneInputUrl.trim(), dest: cloneDest.trim() } });
    setBusy(false);
    if (!res.ok) {
      setCloneMsg(res.error || res.data?.error || 'Clone failed');
      return;
    }
    setShowClone(false);
    setCloneInputUrl('');
    setCloneMsg(null);
    load();
  };

  const removeRepo = async (name: string) => {
    if (!confirm(`Stop hosting "${name}"? (repo files stay on disk)`)) return;
    await window.gf.api(`/api/repos/${encodeURIComponent(name)}`, { method: 'DELETE' });
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

  return (
    <div className="flex h-full">
      <div className="w-72 flex-shrink-0 border-r theme-border overflow-auto">
        <div className="flex items-center justify-between px-4 py-3 border-b theme-border">
          <h2 className="text-sm font-semibold theme-text-primary">Hosted repos</h2>
          <div className="flex gap-1">
            <button onClick={load} className="p-1.5 theme-text-muted hover:theme-text-primary"><RefreshCw size={14} /></button>
            <button onClick={() => { setShowClone(!showClone); setCloneMsg(null); }} className="p-1.5 theme-text-muted hover:theme-text-primary" title="Clone from URL"><Download size={14} /></button>
            <button onClick={addRepo} className="p-1.5 theme-text-muted hover:theme-text-primary" title="Add repo"><Plus size={14} /></button>
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
        {repos.length === 0 && <p className="text-xs theme-text-muted p-4">No repos hosted. Click + to add one.</p>}
        {repos.map((r) => (
          <button key={r.name} onClick={() => openRepo(r)}
            className={`w-full text-left px-4 py-3 border-b theme-border ${selected?.name === r.name ? 'theme-bg-tertiary' : 'hover:theme-bg-tertiary'}`}>
            <div className="flex items-center gap-2">
              <GitBranch size={14} className="text-emerald-500 flex-shrink-0" />
              <span className="text-sm theme-text-primary truncate">{r.name}</span>
            </div>
            {r.description && <p className="text-xs theme-text-muted truncate mt-0.5 pl-6">{r.description}</p>}
          </button>
        ))}
      </div>

      <div className="flex-1 overflow-auto p-6">
        {!selected && (
          <div className="h-full flex items-center justify-center theme-text-muted text-sm">Select a repo, or add one with the + button.</div>
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
