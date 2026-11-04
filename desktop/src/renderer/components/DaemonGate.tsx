import React, { useState } from 'react';
import { Play, RefreshCw, AlertTriangle, Trees } from 'lucide-react';

export default function DaemonGate({ onReady, needsIdentity }: { onReady: () => void; needsIdentity: boolean }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const start = async () => {
    setBusy(true);
    setError(null);
    if (needsIdentity) {
      const init = await window.gf.initIdentity();
      if (!init.ok) {
        setBusy(false);
        setError(
          init.error === 'binary-not-found'
            ? 'gitforest binary not found. Reinstall the app or build with cargo build --release.'
            : `Could not create identity: ${init.error || 'unknown error'}`
        );
        return;
      }
    }
    const res = await window.gf.daemonStart();
    setBusy(false);
    if (res.ok) {
      onReady();
    } else {
      setError(
        res.reason === 'binary-not-found'
          ? 'gitforest binary not found. Install it (install.sh) or build with cargo build --release.'
          : 'Daemon failed to start within the timeout window.'
      );
    }
  };

  return (
    <div className="flex-1 flex items-center justify-center">
      <div className="w-96 theme-bg-secondary rounded-lg border theme-border p-8 text-center">
        <Trees size={40} className="text-emerald-500 mx-auto mb-4" />
        <h2 className="text-lg font-semibold theme-text-primary mb-2">{needsIdentity ? 'Welcome to gitforest' : 'Daemon not running'}</h2>
        <p className="text-sm theme-text-muted mb-6">
          {needsIdentity
            ? 'First launch: create your Ed25519 identity and start the daemon to join the P2P network.'
            : 'gitforest needs its daemon to talk to the P2P network. Start it to continue.'}
        </p>
        {error && (
          <div className="flex items-start gap-2 text-left text-xs text-red-400 bg-red-950/40 border border-red-900 rounded p-3 mb-4">
            <AlertTriangle size={14} className="flex-shrink-0 mt-0.5" />
            <span>{error}</span>
          </div>
        )}
        <button
          onClick={start}
          disabled={busy}
          className="w-full flex items-center justify-center gap-2 bg-emerald-600 hover:bg-emerald-700 disabled:opacity-50 text-white text-sm font-medium rounded px-4 py-2.5 transition-colors"
        >
          {busy ? <RefreshCw size={16} className="animate-spin" /> : <Play size={16} />}
          {busy ? (needsIdentity ? 'Creating identity…' : 'Starting daemon…') : (needsIdentity ? 'Create identity & start' : 'Start daemon')}
        </button>
      </div>
    </div>
  );
}
