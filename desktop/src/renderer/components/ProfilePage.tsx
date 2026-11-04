import React, { useState, useEffect, useCallback } from 'react';
import { User, Copy, CheckCircle2, Download, Upload, RefreshCw, Fingerprint } from 'lucide-react';

export default function ProfilePage() {
  const [peerId, setPeerId] = useState('');
  const [profile, setProfile] = useState<any>({});
  const [displayName, setDisplayName] = useState('');
  const [copied, setCopied] = useState(false);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);

  const load = useCallback(async () => {
    let id: any; let p: any;
    try {
      [id, p] = await Promise.all([
        window.gf.api('/api/id'),
        window.gf.api('/api/profile'),
      ]);
    } catch (e) {
      console.error('[gf:profile] load threw', e);
      return;
    }
    if (id.ok) setPeerId(id.data?.peer_id || id.data || '');
    else console.error('[gf:profile] /api/id failed', { status: id.status, error: id.error });
    if (p.ok && p.data) {
      setProfile(p.data);
      if (p.data.display_name) setDisplayName(p.data.display_name);
    } else if (!p.ok) {
      console.error('[gf:profile] /api/profile failed', { status: p.status, error: p.error });
    }
  }, []);

  useEffect(() => { load(); }, [load]);

  const publishName = async () => {
    setBusy(true);
    setMsg(null);
    console.log('[gf:profile] publish display name', { displayName });
    try {
      const res = await window.gf.api('/api/profile', { method: 'POST', body: { display_name: displayName } });
      setBusy(false);
      if (res.ok) {
        console.log('[gf:profile] display name saved');
        setMsg('Display name saved on this machine.');
      } else {
        console.error('[gf:profile] display name save failed', { status: res.status, error: res.error, data: res.data });
        setMsg(String(res.error || (res.data as any)?.error || 'Could not save display name.'));
      }
    } catch (e) {
      setBusy(false);
      console.error('[gf:profile] display name save threw', e);
      setMsg(`Could not save display name: ${(e as Error).message}`);
    }
  };

  const exportBackup = async () => {
    setBusy(true);
    try {
      const pick = await window.gf.pickSavePath('gitforest-identity-backup.enc');
      if (pick.canceled || !pick.filePath) { setBusy(false); return; }
      console.log('[gf:profile] identity export started', { path: pick.filePath });
      const res = await window.gf.api('/api/identity/export', { method: 'POST', body: { path: pick.filePath } });
      setBusy(false);
      if (res.ok) {
        console.log('[gf:profile] identity export ok');
        setMsg(`Backup written to ${pick.filePath}`);
      } else {
        console.error('[gf:profile] identity export failed', { status: res.status, error: res.error, data: res.data });
        setMsg(String(res.error || (res.data as any)?.error || 'Identity export failed.'));
      }
    } catch (e) {
      setBusy(false);
      console.error('[gf:profile] identity export threw', e);
      setMsg(`Identity export failed: ${(e as Error).message}`);
    }
  };

  const restoreBackup = async () => {
    if (!confirm('Restoring replaces your current identity on this machine. You will lose access to it unless you exported a backup first. Continue?')) return;
    try {
      const pick = await window.gf.pickFile();
      if (pick.canceled || !pick.filePaths?.[0]) return;
      setBusy(true);
      console.log('[gf:profile] identity import started', { path: pick.filePaths[0] });
      const res = await window.gf.api('/api/identity/import', { method: 'POST', body: { path: pick.filePaths[0] } });
      if (res.ok) {
        console.log('[gf:profile] identity import ok, restarting daemon', { peer_id: (res.data as any)?.peer_id });
        setMsg(`Identity restored (now ${res.data?.peer_id || 'unknown'}). Restarting daemon…`);
        await window.gf.daemonRestart();
        setMsg(`Identity restored (now ${res.data?.peer_id || 'unknown'}). Daemon restarted.`);
      } else {
        console.error('[gf:profile] identity import failed', { status: res.status, error: res.error, data: res.data });
        setMsg(String(res.error || (res.data as any)?.error || 'Identity import failed.'));
      }
      setBusy(false);
    } catch (e) {
      setBusy(false);
      console.error('[gf:profile] identity import threw', e);
      setMsg(`Identity import failed: ${(e as Error).message}`);
    }
  };

  return (
    <div className="p-6 max-w-3xl">
      <h1 className="text-xl font-bold theme-text-primary flex items-center gap-2 mb-6">
        <User size={20} className="text-emerald-500" /> Profile & identity
      </h1>

      {msg && <div className="text-xs theme-text-secondary theme-bg-tertiary border theme-border rounded p-3 mb-4">{msg}</div>}

      <div className="theme-bg-secondary rounded border theme-border p-6 mb-6">
        <h3 className="text-sm font-semibold theme-text-primary mb-3 flex items-center gap-2">
          <Fingerprint size={14} className="text-emerald-500" /> Peer identity
        </h3>
        <div className="flex items-center gap-2 theme-bg-tertiary rounded p-3">
          <code className="text-xs theme-text-primary truncate flex-1">{peerId || 'loading…'}</code>
          <button onClick={() => { navigator.clipboard.writeText(peerId); setCopied(true); setTimeout(() => setCopied(false), 1500); }}
            className="theme-text-muted hover:theme-text-primary">
            {copied ? <CheckCircle2 size={14} className="text-emerald-500" /> : <Copy size={14} />}
          </button>
        </div>
        <p className="text-xs theme-text-muted mt-2">This Ed25519 public key is your account. Anyone can verify your signatures against it.</p>
      </div>

      <div className="theme-bg-secondary rounded border theme-border p-6 mb-6">
        <h3 className="text-sm font-semibold theme-text-primary mb-3">Display name</h3>
        <div className="flex gap-2">
          <input
            value={displayName}
            onChange={(e) => setDisplayName(e.target.value)}
            placeholder="How others see you on the network"
            className="flex-1 theme-bg-tertiary border theme-border rounded px-3 py-2 text-sm theme-text-primary outline-none focus:border-emerald-500"
          />
          <button onClick={publishName} disabled={busy}
            className="bg-emerald-600 hover:bg-emerald-700 disabled:opacity-50 text-white text-sm font-medium rounded px-4 py-2">
            {busy ? '…' : 'Publish'}
          </button>
        </div>
        <p className="text-xs theme-text-muted mt-2">Signed with your key and published to the DHT at <code>profile/&lt;peer_id&gt;</code>.</p>
      </div>

      <div className="theme-bg-secondary rounded border theme-border p-6">
        <h3 className="text-sm font-semibold theme-text-primary mb-3">Backup & sign-in</h3>
        <p className="text-xs theme-text-muted mb-4">
          "Signing in" on a gitforest network = restoring your identity key on a machine. Keep an encrypted export safe.
        </p>
        <div className="flex gap-3">
          <button onClick={exportBackup} disabled={busy}
            className="flex items-center gap-2 bg-emerald-600 hover:bg-emerald-700 disabled:opacity-50 text-white text-sm font-medium rounded px-4 py-2">
            <Download size={14} /> Export encrypted backup
          </button>
          <button onClick={restoreBackup} disabled={busy}
            className="flex items-center gap-2 theme-bg-tertiary hover:theme-bg-hover theme-text-secondary text-sm rounded px-4 py-2">
            <Upload size={14} /> Restore from backup
          </button>
        </div>
      </div>
    </div>
  );
}
