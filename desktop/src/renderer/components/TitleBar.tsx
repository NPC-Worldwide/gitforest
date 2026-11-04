import React, { useState, useEffect } from 'react';
import { Minus, Square, X, Maximize2, Trees } from 'lucide-react';

export default function TitleBar() {
  const [isMaximized, setIsMaximized] = useState(false);
  const [isMac, setIsMac] = useState(false);
  const [daemonRunning, setDaemonRunning] = useState(true);

  useEffect(() => {
    setIsMac(navigator.platform.startsWith('Mac'));
    window.gf?.windowState?.isMaximized?.().then(setIsMaximized);
    const u1 = window.gf?.onWindowStateChange?.((s) => setIsMaximized(s.isMaximized));
    window.gf?.daemonState?.().then((s) => setDaemonRunning(s.running));
    const u2 = window.gf?.onDaemonStateChange?.((s) => setDaemonRunning(s.running));
    return () => { u1?.(); u2?.(); };
  }, []);

  const statusDot = (
    <div className="flex items-center gap-1.5" title={daemonRunning ? 'Daemon running' : 'Daemon stopped'}>
      <div className={`w-2 h-2 rounded-full ${daemonRunning ? 'bg-emerald-500' : 'bg-red-500'}`} />
      <span className="text-[10px] theme-text-muted">{daemonRunning ? 'online' : 'offline'}</span>
    </div>
  );

  if (isMac) {
    return (
      <div className="h-8 flex-shrink-0 flex items-center justify-between select-none pl-20 theme-bg-secondary border-b theme-border" style={{ WebkitAppRegion: 'drag' } as React.CSSProperties}>
        <span className="text-xs font-medium theme-text-muted">gitforest</span>
        <div className="flex items-center pr-3" style={{ WebkitAppRegion: 'no-drag' } as React.CSSProperties}>{statusDot}</div>
      </div>
    );
  }

  return (
    <div className="h-9 flex-shrink-0 flex items-center justify-between theme-bg-secondary border-b theme-border select-none" style={{ WebkitAppRegion: 'drag' } as React.CSSProperties}>
      <div className="flex items-center gap-2 px-3" style={{ WebkitAppRegion: 'no-drag' } as React.CSSProperties}>
        <Trees size={16} className="text-emerald-500" />
        <span className="text-xs font-medium theme-text-primary">gitforest</span>
      </div>
      <div className="flex items-center" style={{ WebkitAppRegion: 'no-drag' } as React.CSSProperties}>
        <div className="px-2">{statusDot}</div>
        <button onClick={() => window.gf?.windowControls?.minimize?.()} className="w-12 h-9 flex items-center justify-center theme-text-muted hover:theme-bg-tertiary hover:theme-text-primary transition-colors" title="Minimize">
          <Minus size={14} />
        </button>
        <button onClick={() => window.gf?.windowControls?.maximize?.()} className="w-12 h-9 flex items-center justify-center theme-text-muted hover:theme-bg-tertiary hover:theme-text-primary transition-colors" title={isMaximized ? 'Restore' : 'Maximize'}>
          {isMaximized ? <Square size={12} /> : <Maximize2 size={12} />}
        </button>
        <button onClick={() => window.gf?.windowControls?.close?.()} className="w-12 h-9 flex items-center justify-center theme-text-muted hover:bg-red-600 hover:text-white transition-colors" title="Close">
          <X size={14} />
        </button>
      </div>
    </div>
  );
}
