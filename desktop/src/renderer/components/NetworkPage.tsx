import React, { useState, useEffect, useCallback, useRef } from 'react';
import { Globe, RefreshCw } from 'lucide-react';

function stripMeta(s: string): string {
  const cuts = ['/p2p/', '/api/', '/gf/']
    .map((m) => s.indexOf(m))
    .filter((i) => i >= 0);
  return cuts.length ? s.slice(0, Math.min(...cuts)) : s;
}

function extractP2pId(addr: string): string {
  const m = addr.match(/\/p2p\/([^/]+)/);
  return m ? m[1] : '';
}

function addrPort(addr: string): number {
  return parseInt(addr.split('/')[4] || '0', 10) || 0;
}

function stableAddr(addr: string): boolean {
  const port = addrPort(addr);
  return port > 0 && port < 32768;
}

function entryKey(p: any): string {
  if (p.p2p_id) return String(p.p2p_id);
  const addr = String(p.addr || p.address || '');
  const embedded = extractP2pId(addr);
  if (embedded) return embedded;
  if (p.peer_id || p.id) return stripMeta(String(p.peer_id || p.id));
  const host = addr.split('/')[2] || addr;
  return `addr:${host}`;
}

function addrRank(addr: string): [number, number, number] {
  const segs = addr.split('/');
  const host = (segs[2] || '').toLowerCase();
  const port = parseInt(segs[4] || '0', 10) || 0;
  const loopback = host === '127.0.0.1' || host === '::1' || host === 'localhost' ? 1 : 0;
  const ephemeral = port >= 32768 ? 1 : 0;
  return [loopback, ephemeral, port];
}

function betterAddr(a: string, b: string): string {
  const [al, ae, ap] = addrRank(a);
  const [bl, be, bp] = addrRank(b);
  if (al !== bl) return al < bl ? a : b;
  if (ae !== be) return ae < be ? a : b;
  return ap <= bp ? a : b;
}

function endpointKey(addr: string): string {
  const segs = addr.split('/');
  const host = (segs[2] || '').toLowerCase();
  const port = parseInt(segs[4] || '0', 10) || 0;
  if (!host || !port) return '';
  return `${host}:${port}`;
}

function preferId(a: string, b: string): string {
  const hex = /^[0-9a-f]{64}$/;
  if (hex.test(a)) return a;
  if (hex.test(b)) return b;
  return a || b;
}

export default function NetworkPage() {
  const [status, setStatus] = useState<any>(null);
  const [peers, setPeers] = useState<any[]>([]);
  const [connected, setConnected] = useState<number | null>(null);
  const [transfers, setTransfers] = useState<any[]>([]);
  const lastPeerSig = useRef('');

  const load = useCallback(async () => {
    let s: any; let p: any; let t: any;
    try {
      [s, p, t] = await Promise.all([
        window.gf.api('/api/status'),
        window.gf.api('/api/peers'),
        window.gf.api('/api/transfers'),
      ]);
    } catch (e) {
      console.error('[gf:net] load threw', e);
      return;
    }
    if (s.ok) setStatus(s.data);
    else console.error('[gf:net] /api/status failed', { status: s.status, error: s.error });
    if (p.ok) {
      const list = Array.isArray(p.data) ? p.data : p.data?.peers || [];
      setPeers(list);
      setConnected(typeof p.data?.connected === 'number' ? p.data.connected : null);
      if (typeof p.data?.connected === 'number' && p.data.connected === 0) {
        console.warn('[gf:net] connected=0', {
          bootstraps: Array.isArray(p.data?.bootstrap_peers) ? p.data.bootstrap_peers.length : '?',
          discovered: Array.isArray(p.data?.discovered_peers) ? p.data.discovered_peers.length : '?',
        });
      } else {
        const sig = `${list.length}/${p.data?.connected ?? '?'}`;
        if (sig !== lastPeerSig.current) {
          lastPeerSig.current = sig;
          console.log('[gf:net] peers ok', {
            entries: list.length,
            connected: p.data?.connected ?? '?',
            bootstraps: Array.isArray(p.data?.bootstrap_peers) ? p.data.bootstrap_peers.length : '?',
            discovered: Array.isArray(p.data?.discovered_peers) ? p.data.discovered_peers.length : '?',
          });
        }
      }
    } else {
      console.error('[gf:net] /api/peers failed', { status: p.status, error: p.error });
    }
    if (t.ok) setTransfers(Array.isArray(t.data) ? t.data : t.data?.transfers || []);
    else console.error('[gf:net] /api/transfers failed', { status: t.status, error: t.error });
  }, []);

  useEffect(() => { load(); const t = setInterval(load, 5000); return () => clearInterval(t); }, [load]);

  const grouped = new Map<string, { id: string; addr: string; stable: number; total: number }>();
  for (const p of peers) {
    const key = entryKey(p);
    const addr = stripMeta(String(p.addr || p.address || ''));
    const label = stripMeta(String(p.peer_id || p.id || ''));
    const cur = grouped.get(key);
    if (cur) {
      cur.total += 1;
      if (stableAddr(addr)) {
        cur.stable += 1;
        cur.addr = cur.stable === 1 ? addr : betterAddr(cur.addr, addr);
      } else if (cur.stable === 0) {
        cur.addr = betterAddr(cur.addr, addr);
      }
      if (!cur.id && label) cur.id = label;
    } else {
      grouped.set(key, {
        id: label || (key.startsWith('addr:') ? '' : key),
        addr,
        stable: stableAddr(addr) ? 1 : 0,
        total: 1,
      });
    }
  }
  const merged = new Map<string, { id: string; addr: string; stable: number; total: number }>();
  for (const r of grouped.values()) {
    const ek = endpointKey(r.addr);
    if (!ek) {
      merged.set(`row:${merged.size}`, { ...r });
      continue;
    }
    const cur = merged.get(ek);
    if (cur) {
      cur.stable += r.stable;
      cur.total += r.total;
      cur.addr = betterAddr(cur.addr, r.addr);
      cur.id = preferId(cur.id, r.id);
    } else {
      merged.set(ek, { ...r });
    }
  }
  const unique = [...merged.values()];
  const shown = connected ?? unique.length;

  return (
    <div className="p-6">
      <div className="flex items-center justify-between mb-6">
        <h1 className="text-xl font-bold theme-text-primary flex items-center gap-2">
          <Globe size={20} className="text-emerald-500" /> Network
        </h1>
        <button onClick={load} className="p-2 theme-text-muted hover:theme-text-primary"><RefreshCw size={16} /></button>
      </div>

      <div className="grid grid-cols-3 gap-4 mb-6">
        <div className="theme-bg-secondary rounded border theme-border p-4">
          <p className="text-xs theme-text-muted mb-1">Connected peers</p>
          <p className="text-2xl font-semibold theme-text-primary">{shown}</p>
        </div>
        <div className="theme-bg-secondary rounded border theme-border p-4">
          <p className="text-xs theme-text-muted mb-1">Repos seeding</p>
          <p className="text-2xl font-semibold theme-text-primary">{status?.repos_seeding ?? status?.repos ?? '–'}</p>
        </div>
        <div className="theme-bg-secondary rounded border theme-border p-4">
          <p className="text-xs theme-text-muted mb-1">Active transfers</p>
          <p className="text-2xl font-semibold theme-text-primary">{transfers.length}</p>
        </div>
      </div>

      <div className="theme-bg-secondary rounded border theme-border mb-6">
        <h3 className="text-sm font-semibold theme-text-primary px-4 py-3 border-b theme-border">Peers ({unique.length} unique, {unique.reduce((n, p) => n + (p.stable || p.total), 0)} addresses)</h3>
        {unique.length === 0 && <p className="text-xs theme-text-muted p-4">{connected ? 'Discovering peer addresses…' : 'No peers connected. Bootstrap nodes unreachable?'}</p>}
        {unique.map((p, i) => (
          <div key={p.id || p.addr || i} className="px-4 py-2 border-b theme-border last:border-0 flex items-center justify-between">
            <code className="text-xs theme-text-primary truncate">{p.id || p.addr}{(p.stable || p.total) > 1 ? ` · ${p.stable || p.total} addrs` : ''}</code>
            <span className="text-xs theme-text-muted">{p.id ? p.addr : ''}</span>
          </div>
        ))}
      </div>

      <div className="theme-bg-secondary rounded border theme-border">
        <h3 className="text-sm font-semibold theme-text-primary px-4 py-3 border-b theme-border">Active transfers</h3>
        {transfers.length === 0 && <p className="text-xs theme-text-muted p-4">No transfers in flight.</p>}
        {transfers.map((t: any, i: number) => (
          <div key={t.blob_id || i} className="px-4 py-2 border-b theme-border last:border-0">
            <div className="flex justify-between text-xs mb-1">
              <code className="theme-text-primary">{(t.blob_id || '').slice(0, 16)}…</code>
              <span className="theme-text-muted">{t.progress != null ? `${Math.round(t.progress * 100)}%` : ''}</span>
            </div>
            <div className="h-1.5 theme-bg-tertiary rounded-full overflow-hidden">
              <div className="h-full bg-emerald-500" style={{ width: `${Math.round((t.progress || 0) * 100)}%` }} />
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
