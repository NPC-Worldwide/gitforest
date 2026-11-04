import React, { useState, useEffect, useCallback } from 'react';
import TitleBar from './components/TitleBar';
import Sidebar, { Page } from './components/Sidebar';
import DaemonGate from './components/DaemonGate';
import FirstRunWizard from './components/FirstRunWizard';
import ReposPage from './components/ReposPage';
import NetworkPage from './components/NetworkPage';
import AnalyticsPage from './components/AnalyticsPage';
import ComputePage from './components/ComputePage';
import ProfilePage from './components/ProfilePage';
import SettingsPage from './components/SettingsPage';

type AppState = 'loading' | 'daemon-down' | 'first-run' | 'ready';

export default function App() {
  const [state, setState] = useState<AppState>('loading');
  const [page, setPage] = useState<Page>('repos');
  const [identityExists, setIdentityExists] = useState(false);

  const check = useCallback(async () => {
    if (!window.gf) { setState('loading'); return; }
    const ds = await window.gf.daemonState();
    if (!ds.running) {
      setIdentityExists(await window.gf.identityExists());
      setState('daemon-down');
      return;
    }
    const onboarded = await window.gf.onboardedGet();
    setState(onboarded ? 'ready' : 'first-run');
  }, []);

  useEffect(() => {
    check();
    const unsub = window.gf?.onDaemonStateChange?.((s) => {
      if (!s.running) setState('daemon-down'); else check();
    });
    return () => { unsub?.(); };
  }, [check]);

  return (
    <div className="h-screen w-screen theme-bg-primary overflow-hidden flex flex-col">
      <TitleBar />
      <div className="flex-1 flex overflow-hidden">
        {state === 'loading' && (
          <div className="flex-1 flex items-center justify-center">
            <div className="theme-text-muted">Loading gitforest…</div>
          </div>
        )}
        {state === 'daemon-down' && <DaemonGate onReady={check} needsIdentity={!identityExists} />}
        {state === 'first-run' && (
          <FirstRunWizard onComplete={() => { window.gf.onboardedSet().finally(() => setState('ready')); }} />
        )}
        {state === 'ready' && (
          <>
            <Sidebar page={page} onNavigate={setPage} />
            <div className="flex-1 overflow-auto theme-bg-primary">
              {page === 'repos' && <ReposPage />}
              {page === 'network' && <NetworkPage />}
              {page === 'analytics' && <AnalyticsPage />}
              {page === 'compute' && <ComputePage />}
              {page === 'profile' && <ProfilePage />}
              {page === 'settings' && <SettingsPage />}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
