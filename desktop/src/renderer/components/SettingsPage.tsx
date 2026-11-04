import React, { useState, useEffect, useCallback } from 'react';
import { Settings, Power, RefreshCw, RotateCcw, FileText, ArrowUpCircle, ExternalLink, Download } from 'lucide-react';

export default function SettingsPage() {
  const [daemonRunning, setDaemonRunning] = useState(true);
  const [daemonVersion, setDaemonVersion] = useState<string | null>(null);
  const [daemonOwned, setDaemonOwned] = useState(true);
  const [daemonPid, setDaemonPid] = useState<number | null>(null);
  const [daemonError, setDaemonError] = useState<string | null>(null);
  const [log, setLog] = useState('');
  const [dataDir, setDataDir] = useState('');
  const [configDir, setConfigDir] = useState('');
  const [busy, setBusy] = useState(false);
  const [appVersion, setAppVersion] = useState('');
  const [update, setUpdate] = useState<any>(null);
  const [checking, setChecking] = useState(false);
  const [updateError, setUpdateError] = useState<string | null>(null);
  const [dlProgress, setDlProgress] = useState<{ received: number; total: number } | null>(null);
  const [installing, setInstalling] = useState(false);
  const [dlError, setDlError] = useState<string | null>(null);

  const load = useCallback(async () => {
    let ds: any; let l: any; let dd: any; let cd: any;
    try {
      [ds, l, dd, cd] = await Promise.all([
        window.gf.daemonState(),
        window.gf.readLog(30),
        window.gf.dataDir(),
        window.gf.configDir(),
      ]);
    } catch (e) {
      console.error('[gf:settings] load threw', e);
      return;
    }
    setDaemonRunning(ds.running);
    setDaemonVersion(typeof ds.daemonVersion === 'string' ? ds.daemonVersion : null);
    setDaemonOwned(ds.owned !== false);
    setDaemonPid(typeof ds.daemonPid === 'number' ? ds.daemonPid : null);
    console.log('[gf:settings] daemon state', JSON.stringify({ running: ds.running, daemon: ds.daemonVersion, daemonPid: ds.daemonPid, owned: ds.owned, app: ds.appVersion }));
    if (l.ok) setLog(l.data || '');
    else console.error('[gf:settings] read log failed', l);
    setDataDir(dd);
    setConfigDir(cd);
  }, []);

  const checkNow = useCallback(async () => {
    setChecking(true);
    setUpdateError(null);
    try {
      const r: any = await window.gf.checkForUpdates?.();
      if (r?.success) {
        console.log('[gf:update] check ok', JSON.stringify({ current: r.currentVersion, latest: r.latestVersion, hasUpdate: r.hasUpdate, asset: r.asset?.kind || null, devMode: r.devMode }));
        setUpdate(r);
      } else {
        console.error('[gf:update] check failed', r?.error);
        setUpdateError(r?.error || 'Check failed');
      }
    } catch (e) {
      console.error('[gf:update] check threw', e);
      setUpdateError((e as Error).message);
    }
    setChecking(false);
  }, []);

  const downloadAndInstall = async () => {
    const asset = update?.asset;
    if (!asset) return;
    setDlError(null);
    setDlProgress({ received: 0, total: 0 });
    console.log('[gf:update] download started', { url: asset.url, file: asset.file });
    const unsub = window.gf.onUpdateDownloadProgress((p) => setDlProgress(p));
    let dl: any;
    try {
      dl = await window.gf.downloadUpdate(asset.url, asset.file);
    } catch (e) {
      unsub();
      setDlProgress(null);
      console.error('[gf:update] download threw', e);
      setDlError(`Download failed: ${(e as Error).message}`);
      return;
    }
    unsub();
    if (!dl?.success) {
      setDlProgress(null);
      console.error('[gf:update] download failed', dl?.error);
      setDlError(dl?.error || 'Download failed');
      return;
    }
    console.log('[gf:update] download ok, installing', { path: dl.path, kind: asset.kind });
    setInstalling(true);
    let inst: any;
    try {
      inst = await window.gf.installUpdate(dl.path, asset.kind);
    } catch (e) {
      setInstalling(false);
      setDlProgress(null);
      console.error('[gf:update] install threw', e);
      setDlError(`Install failed: ${(e as Error).message}`);
      return;
    }
    setInstalling(false);
    setDlProgress(null);
    if (!inst?.success) {
      console.error('[gf:update] install failed', inst?.error);
      setDlError(inst?.error || 'Install failed');
    }
  };

  useEffect(() => {
    load();
    window.gf.getAppVersion?.().then(setAppVersion);
    checkNow();
    const unsub = window.gf.onDaemonStateChange?.((s) => setDaemonRunning(s.running));
    return () => { unsub?.(); };
  }, [load, checkNow]);

  const act = async (fn: () => Promise<any>) => {
    setBusy(true);
    setDaemonError(null);
    let r: any;
    try {
      r = await fn();
    } catch (e) {
      console.error('[gf:settings] daemon action threw', e);
      setDaemonError((e as Error).message);
      setBusy(false);
      load();
      return;
    }
    console.log('[gf:settings] daemon action result', JSON.stringify(r));
    if (r && r.ok === false) setDaemonError(r.error || r.reason || 'Daemon action failed');
    setBusy(false);
    load();
  };

  return (
    <div className="p-6 max-w-3xl">
      <h1 className="text-xl font-bold theme-text-primary flex items-center gap-2 mb-6">
        <Settings size={20} className="text-emerald-500" /> Machine & daemon
      </h1>

      <div className="theme-bg-secondary rounded border theme-border p-6 mb-6">
        <div className="flex items-center justify-between mb-4">
          <h3 className="text-sm font-semibold theme-text-primary flex items-center gap-2">
            <ArrowUpCircle size={14} className="text-emerald-500" /> App updates
          </h3>
          <button disabled={checking || !!dlProgress || installing} onClick={checkNow}
            className="flex items-center gap-1.5 text-xs theme-bg-tertiary hover:theme-bg-hover disabled:opacity-40 theme-text-secondary rounded px-3 py-1.5">
            <RefreshCw size={12} /> {checking ? 'Checking…' : 'Check for updates'}
          </button>
        </div>
        <div className="space-y-1.5 text-xs theme-text-muted">
          <p>Installed: <code>v{appVersion || '?'}</code>
            {update?.success && update.hasUpdate && <> · Latest: <code>v{update.latestVersion}</code></>}</p>
          {updateError && <p className="text-red-500">Check failed: {updateError}</p>}
          {update?.success && !update.hasUpdate && <p className="text-emerald-500">You're up to date.</p>}
          {update?.devMode && <p>Dev mode: in-app install is disabled.</p>}
        </div>
        {update?.success && update.hasUpdate && !update.devMode && update.asset && !installing && (
          <button disabled={!!dlProgress} onClick={downloadAndInstall}
            className="mt-4 flex items-center gap-1.5 text-xs bg-emerald-600 hover:bg-emerald-700 disabled:opacity-40 text-white rounded px-3 py-1.5">
            <Download size={12} /> {dlProgress ? 'Downloading…' : `Download & install v${update.latestVersion}`}
          </button>
        )}
        {update?.success && update.hasUpdate && !update.asset && (
          <button onClick={() => window.gf.openExternal(update.releaseUrl)}
            className="mt-4 flex items-center gap-1.5 text-xs theme-bg-tertiary hover:theme-bg-hover theme-text-secondary rounded px-3 py-1.5">
            View release <ExternalLink size={12} />
          </button>
        )}
        {dlProgress && (
          <div className="mt-3">
            <div className="h-2 theme-bg-tertiary rounded overflow-hidden">
              <div className="h-full bg-emerald-500 transition-all"
                style={{ width: `${dlProgress.total ? Math.round(100 * dlProgress.received / dlProgress.total) : 0}%` }} />
            </div>
            <p className="text-xs theme-text-muted mt-1">
              {(dlProgress.received / 1048576).toFixed(1)} / {(dlProgress.total / 1048576).toFixed(1)} MB
            </p>
          </div>
        )}
        {installing && <p className="text-xs theme-text-muted mt-3">Installing… the app will relaunch.</p>}
        {dlError && <p className="text-xs text-red-500 mt-3">{dlError}</p>}
      </div>

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
          <p>
            Daemon: <code>v{daemonVersion || 'unknown'}</code>
            {daemonRunning && <> · {daemonOwned ? 'started by this app' : 'started outside the app'}{daemonPid ? <> · pid <code>{daemonPid}</code></> : null}</>}
            {!daemonOwned && daemonVersion !== appVersion && <> · Restart replaces it with the bundled v{appVersion} daemon.</>}
          </p>
          <p>Config: <code>{configDir}</code></p>
          <p>Data: <code>{dataDir}</code></p>
          <p>API: <code>http://127.0.0.1:7878</code> · P2P: <code>9418</code> · App: v{appVersion}</p>
          {daemonError && <p className="text-red-500">{daemonError}</p>}
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
