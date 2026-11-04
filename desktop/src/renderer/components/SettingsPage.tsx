import React, { useState, useEffect, useCallback } from 'react';
import { Settings, Power, RefreshCw, RotateCcw, FileText, ArrowUpCircle, ExternalLink } from 'lucide-react';

export default function SettingsPage() {
  const [daemonRunning, setDaemonRunning] = useState(true);
  const [log, setLog] = useState('');
  const [dataDir, setDataDir] = useState('');
  const [configDir, setConfigDir] = useState('');
  const [busy, setBusy] = useState(false);
  const [appVersion, setAppVersion] = useState('');
  const [update, setUpdate] = useState<any>(null);

  const load = useCallback(async () => {
    const [ds, l, dd, cd] = await Promise.all([
      window.gf.daemonState(),
      window.gf.readLog(30),
      window.gf.dataDir(),
      window.gf.configDir(),
    ]);
    setDaemonRunning(ds.running);
    if (l.ok) setLog(l.data || '');
    setDataDir(dd);
    setConfigDir(cd);
  }, []);

  useEffect(() => {
    load();
    window.gf.getAppVersion?.().then(setAppVersion);
    window.gf.checkForUpdates?.().then((r: any) => { if (r?.success) setUpdate(r); });
    const unsub = window.gf.onDaemonStateChange?.((s) => setDaemonRunning(s.running));
    return () => { unsub?.(); };
  }, [load]);

  const act = async (fn: () => Promise<any>) => {
    setBusy(true);
    await fn();
    setBusy(false);
    load();
  };

  return (
    <div className="p-6 max-w-3xl">
      <h1 className="text-xl font-bold theme-text-primary flex items-center gap-2 mb-6">
        <Settings size={20} className="text-emerald-500" /> Machine & daemon
      </h1>

      {update?.hasUpdate && (
        <button onClick={() => window.gf.openExternal(update.releaseUrl)}
          className="w-full flex items-center justify-between theme-bg-tertiary border border-emerald-700 rounded p-3 mb-6 text-left">
          <span className="text-sm theme-text-primary flex items-center gap-2">
            <ArrowUpCircle size={16} className="text-emerald-500" />
            gitforest {update.latestVersion} available (you have {update.currentVersion})
          </span>
          <ExternalLink size={14} className="theme-text-muted" />
        </button>
      )}

      <div className="theme-bg-secondary rounded border theme-border p-6 mb-6">
        <div className="flex items-center justify-between mb-4">
          <h3 className="text-sm font-semibold theme-text-primary flex items-center gap-2">
            <Power size={14} className="text-emerald-500" /> Daemon
            <span className={`text-[10px] px-2 py-0.5 rounded ${daemonRunning ? 'bg-emerald-900/50 text-emerald-400' : 'bg-red-900/50 text-red-400'}`}>
              {daemonRunning ? 'running' : 'stopped'}
            </span>
          </h3>
          <div className="flex gap-2">
            <button disabled={busy || daemonRunning} onClick={() => act(() => window.gf.daemonStart())}
              className="flex items-center gap-1.5 text-xs bg-emerald-600 hover:bg-emerald-700 disabled:opacity-40 text-white rounded px-3 py-1.5">
              <Power size={12} /> Start
            </button>
            <button disabled={busy || !daemonRunning} onClick={() => act(() => window.gf.daemonStop())}
              className="flex items-center gap-1.5 text-xs theme-bg-tertiary hover:theme-bg-hover disabled:opacity-40 theme-text-secondary rounded px-3 py-1.5">
              <Power size={12} /> Stop
            </button>
            <button disabled={busy || !daemonRunning} onClick={() => act(() => window.gf.daemonRestart())}
              className="flex items-center gap-1.5 text-xs theme-bg-tertiary hover:theme-bg-hover disabled:opacity-40 theme-text-secondary rounded px-3 py-1.5">
              <RotateCcw size={12} /> Restart
            </button>
            <button onClick={() => load()} className="p-1.5 theme-text-muted hover:theme-text-primary"><RefreshCw size={14} /></button>
          </div>
        </div>
        <div className="space-y-1.5 text-xs theme-text-muted">
          <p>Config: <code>{configDir}</code></p>
          <p>Data: <code>{dataDir}</code></p>
          <p>API: <code>http://127.0.0.1:7878</code> · P2P: <code>9418</code> · App: v{appVersion}</p>
        </div>
      </div>

      <div className="theme-bg-secondary rounded border theme-border p-6">
        <h3 className="text-sm font-semibold theme-text-primary mb-3 flex items-center gap-2">
          <FileText size={14} className="text-emerald-500" /> Daemon log (last 30 lines)
        </h3>
        <pre className="text-[11px] theme-text-secondary theme-bg-tertiary rounded p-3 overflow-auto max-h-64 whitespace-pre-wrap break-all">
          {log || 'No log output captured yet. Logs are written when the app starts the daemon.'}
        </pre>
      </div>
    </div>
  );
}
