import React, { useState, useEffect, useCallback, useRef } from 'react';
import { BarChart3, RefreshCw } from 'lucide-react';

interface Sample { t: number; peers: number; up_bps?: number; down_bps?: number }

export default function AnalyticsPage() {
  const [summary, setSummary] = useState<any>(null);
  const [history, setHistory] = useState<Sample[]>([]);
  const [localHistory, setLocalHistory] = useState<Sample[]>([]);
  const canvasRef = useRef<HTMLCanvasElement>(null);

  const load = useCallback(async () => {
    // Server-side /api/analytics/* feeds the summary and chart; the client-side
    // rolling buffer stays as a fallback for older daemons and fresh histories.
    const [s, p, sum, hist] = await Promise.all([
      window.gf.api('/api/status'),
      window.gf.api('/api/peers'),
      window.gf.api('/api/analytics/summary'),
      window.gf.api('/api/analytics/history?window=24h'),
    ]);
    const peerCount = p.ok ? (Array.isArray(p.data) ? p.data.length : p.data?.peers?.length || 0) : 0;
    setLocalHistory((h) => [...h.slice(-287), { t: Date.now(), peers: peerCount }]);
    if (s.ok) setSummary((prev: any) => ({ ...prev, ...s.data }));
    if (sum.ok) setSummary((prev: any) => ({ ...prev, ...sum.data }));
    if (hist.ok && Array.isArray(hist.data)) setHistory(hist.data);
  }, []);

  useEffect(() => { load(); const t = setInterval(load, 5000); return () => clearInterval(t); }, [load]);

  // lightweight inline sparkline — swap for uPlot in WS-E (T4.1)
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;
    const data = history.length > 1 ? history : localHistory;
    const w = canvas.width = canvas.offsetWidth * devicePixelRatio;
    const h = canvas.height = canvas.offsetHeight * devicePixelRatio;
    ctx.clearRect(0, 0, w, h);
    if (data.length < 2) return;
    const values = data.map(d => d.peers);
    const max = Math.max(...values, 1);
    ctx.strokeStyle = '#10b981';
    ctx.lineWidth = 2 * devicePixelRatio;
    ctx.beginPath();
    values.forEach((v, i) => {
      const x = (i / (values.length - 1)) * w;
      const y = h - (v / max) * (h - 8 * devicePixelRatio) - 4 * devicePixelRatio;
      i === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y);
    });
    ctx.stroke();
  }, [history, localHistory]);

  const currentPeers = (history.length ? history[history.length - 1]?.peers : localHistory[localHistory.length - 1]?.peers) ?? 0;

  return (
    <div className="p-6">
      <div className="flex items-center justify-between mb-6">
        <h1 className="text-xl font-bold theme-text-primary flex items-center gap-2">
          <BarChart3 size={20} className="text-emerald-500" /> Analytics
        </h1>
        <button onClick={load} className="p-2 theme-text-muted hover:theme-text-primary"><RefreshCw size={16} /></button>
      </div>

      <div className="grid grid-cols-4 gap-4 mb-6">
        {[
          { label: 'Peers now', value: currentPeers },
          { label: 'Repos hosted', value: summary?.repos_seeding ?? summary?.repos ?? '–' },
          { label: 'Blobs stored', value: summary?.blob_count ?? '–' },
          { label: 'Compute jobs', value: summary?.job_count ?? '–' },
        ].map((s) => (
          <div key={s.label} className="theme-bg-secondary rounded border theme-border p-4">
            <p className="text-xs theme-text-muted mb-1">{s.label}</p>
            <p className="text-2xl font-semibold theme-text-primary">{s.value}</p>
          </div>
        ))}
      </div>

      <div className="theme-bg-secondary rounded border theme-border p-4 mb-6">
        <h3 className="text-sm font-semibold theme-text-primary mb-3">Peer count over time</h3>
        <canvas ref={canvasRef} className="w-full h-32" />
        <p className="text-[10px] theme-text-muted mt-2">
          {history.length > 1 ? 'Server-side metrics (24h window)' : 'Live session sampling — daemon-side metrics ring buffer lands with the analytics API (T2.5)'}
        </p>
      </div>

      <div className="theme-bg-secondary rounded border theme-border p-4">
        <h3 className="text-sm font-semibold theme-text-primary mb-3">Storage</h3>
        <p className="text-xs theme-text-muted">
          {summary?.blob_bytes != null
            ? `${summary.blob_count ?? 0} blobs · ${(summary.blob_bytes / 1024).toFixed(1)} KiB`
            : 'Blob store usage reports here once the daemon serves /api/analytics/summary.'}
        </p>
      </div>
    </div>
  );
}
