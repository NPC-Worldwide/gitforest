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
  const [appVersion, setAppVersion] = useState('');
  const [daemonVersion, setDaemonVersion] = useState<string | null>(null);
  const [daemonOwned, setDaemonOwned] = useState(true);

  const check = useCallback(async () => {
    if (!window.gf) { setState('loading'); return; }
    try {
      const ds = await window.gf.daemonState();
      console.log('[gf:app] daemon state', { running: ds.running, app: ds.appVersion, daemon: ds.daemonVersion, owned: ds.owned });
      setAppVersion(ds.appVersion || '');
      setDaemonVersion(typeof ds.daemonVersion === 'string' ? ds.daemonVersion : null);
      setDaemonOwned(ds.owned !== false);
      if (!ds.running) {
        setIdentityExists(await window.gf.identityExists());
        setState('daemon-down');
        return;
      }
      const onboarded = await window.gf.onboardedGet();
      setState(onboarded ? 'ready' : 'first-run');
    } catch (e) {
      console.error('[gf:app] daemon state check threw', e);
      setState('daemon-down');
    }
  }, []);

  const versionMismatch = state === 'ready' && !!appVersion && daemonVersion !== appVersion;

  useEffect(() => {
    check();
    const unsub = window.gf?.onDaemonStateChange?.((s) => {
      console.log('[gf:app] daemon state changed', s);
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
            <Sidebar page={page} onNavigate={setPage} appVersion={appVersion} daemonVersion={daemonVersion} />
            <div className="flex-1 overflow-auto theme-bg-primary flex flex-col">
              {versionMismatch && (
                <div className="flex items-center justify-between gap-3 px-4 py-2 bg-amber-900/40 border-b border-amber-700 text-xs">
                  <span className="text-amber-200">
                    Daemon v{daemonVersion || '?'} doesn't match app v{appVersion}.
                    {daemonOwned ? ' Restart it to pick up the bundled daemon.' : ' It was started outside the app — restart it to match.'}
                  </span>
                  {daemonOwned && (
                    <button
                      onClick={async () => {
                        console.log('[gf:app] daemon restart requested (version mismatch)', { app: appVersion, daemon: daemonVersion });
                        try {
                          const r = await window.gf.daemonRestart();
                          console.log('[gf:app] daemon restart result', r);
                        } catch (e) {
                          console.error('[gf:app] daemon restart threw', e);
                        }
                        check();
                      }}
                      className="flex-shrink-0 bg-amber-600 hover:bg-amber-700 text-white rounded px-3 py-1"
                    >
                      Restart daemon
                    </button>
                  )}
                </div>
              )}
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
