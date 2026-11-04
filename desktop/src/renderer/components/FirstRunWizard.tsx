import React, { useState, useEffect } from 'react';
import {
  Trees, KeyRound, Download, FolderGit2, CheckCircle2, Copy, AlertTriangle,
  ArrowRight, ArrowLeft, RefreshCw, ShieldCheck
} from 'lucide-react';

type Step = 'welcome' | 'identity' | 'backup' | 'clone-demo' | 'done';

const DEMO_REPO = (import.meta as unknown as { env?: Record<string, string> }).env?.GITFOREST_DEMO_REPO || '';

export default function FirstRunWizard({ onComplete }: { onComplete: () => void }) {
  const [step, setStep] = useState<Step>('welcome');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [peerId, setPeerId] = useState<string>('');
  const [copied, setCopied] = useState(false);
  const [backupDone, setBackupDone] = useState(false);
  const [cloneUrl, setCloneUrl] = useState(DEMO_REPO);
  const [cloneStatus, setCloneStatus] = useState<'idle' | 'cloning' | 'ok' | 'fail'>('idle');

  const genIdentity = async () => {
    setBusy(true);
    setError(null);
    const res = await window.gf.api('/api/id');
    setBusy(false);
    if (res.ok && res.data?.peer_id) {
      setPeerId(res.data.peer_id);
      setStep('backup');
    } else if (res.ok && typeof res.data === 'string') {
      setPeerId(res.data);
      setStep('backup');
    } else {
      setError(res.error || 'Could not read identity from daemon.');
    }
  };

  useEffect(() => {
    // prefetch peer id if daemon already has one
    window.gf.api('/api/id').then((res: any) => {
      if (res?.ok) setPeerId(res.data?.peer_id || res.data || '');
    });
  }, []);

  const doBackup = async () => {
    setBusy(true);
    setError(null);
    const pick = await window.gf.pickSavePath('gitforest-identity-backup.enc');
    if (pick.canceled || !pick.filePath) { setBusy(false); return; }
    // Daemon writes the key backup through /api/identity/export.
    // The wizard records done-ness so the flag can drive the "backup done" UI state.
    const res = await window.gf.api('/api/identity/export', { method: 'POST', body: { path: pick.filePath } });
    setBusy(false);
    if (res.ok) {
      setBackupDone(true);
    } else {
      setError(res.error || 'Identity export failed.');
    }
  };

  const doClone = async () => {
    if (!cloneUrl) return;
    setCloneStatus('cloning');
    setError(null);
    // clone goes through the daemon (needs git + remote helper on the host)
    const pick = await window.gf.pickDirectory();
    if (pick.canceled || !pick.filePaths?.[0]) { setCloneStatus('idle'); return; }
    const res = await window.gf.api('/api/repos/clone', {
      method: 'POST',
      body: { url: cloneUrl, dest: pick.filePaths[0] },
    });
    setCloneStatus(res.ok ? 'ok' : 'fail');
    if (!res.ok) setError(res.error || res.data?.error || 'Clone failed');
  };

  const steps: { id: Step; label: string }[] = [
    { id: 'welcome', label: 'Welcome' },
    { id: 'identity', label: 'Identity' },
    { id: 'backup', label: 'Key backup' },
    { id: 'clone-demo', label: 'First repo' },
    { id: 'done', label: 'Done' },
  ];
  const stepIdx = steps.findIndex(s => s.id === step);

  return (
    <div className="flex-1 flex flex-col items-center justify-center p-8">
      <div className="w-full max-w-xl">
        {/* progress */}
        <div className="flex items-center justify-between mb-8">
          {steps.map((s, i) => (
            <React.Fragment key={s.id}>
              <div className="flex flex-col items-center">
                <div className={`w-8 h-8 rounded-full flex items-center justify-center text-xs font-medium ${
                  i < stepIdx ? 'bg-emerald-600 text-white' :
                  i === stepIdx ? 'bg-emerald-600 text-white ring-2 ring-emerald-400' :
                  'theme-bg-tertiary theme-text-muted'
                }`}>
                  {i < stepIdx ? <CheckCircle2 size={16} /> : i + 1}
                </div>
                <span className="text-[10px] mt-1 theme-text-muted">{s.label}</span>
              </div>
              {i < steps.length - 1 && <div className={`flex-1 h-0.5 mx-2 mb-4 ${i < stepIdx ? 'bg-emerald-600' : 'theme-bg-tertiary'}`} />}
            </React.Fragment>
          ))}
        </div>

        <div className="theme-bg-secondary rounded-lg border theme-border p-8">
          {error && (
            <div className="flex items-start gap-2 text-xs text-red-400 bg-red-950/40 border border-red-900 rounded p-3 mb-4">
              <AlertTriangle size={14} className="flex-shrink-0 mt-0.5" />
              <span>{error}</span>
            </div>
          )}

          {step === 'welcome' && (
            <>
              <Trees size={48} className="text-emerald-500 mb-4" />
              <h1 className="text-2xl font-bold theme-text-primary mb-2">Welcome to GitForest</h1>
              <p className="text-sm theme-text-muted mb-6">
                Decentralized P2P git hosting. Your repos, hosted by you, discovered on a DHT,
                signed with your own Ed25519 key. No company controls the network.
              </p>
              <button onClick={() => {
                window.gf.identityExists().then((exists: boolean) => {
                  setStep(exists ? 'clone-demo' : 'identity');
                });
              }} className="flex items-center gap-2 bg-emerald-600 hover:bg-emerald-700 text-white text-sm font-medium rounded px-5 py-2.5">
                Get started <ArrowRight size={16} />
              </button>
            </>
          )}

          {step === 'identity' && (
            <>
              <KeyRound size={40} className="text-emerald-500 mb-4" />
              <h2 className="text-xl font-semibold theme-text-primary mb-2">Your identity</h2>
              <p className="text-sm theme-text-muted mb-4">
                GitForest generates an Ed25519 keypair. Your public key <em>is</em> your account —
                no sign-ups, no passwords, no email.
              </p>
              <button onClick={genIdentity} disabled={busy}
                className="flex items-center gap-2 bg-emerald-600 hover:bg-emerald-700 disabled:opacity-50 text-white text-sm font-medium rounded px-5 py-2.5">
                {busy ? <RefreshCw size={16} className="animate-spin" /> : <KeyRound size={16} />}
                Generate identity
              </button>
            </>
          )}

          {step === 'backup' && (
            <>
              <ShieldCheck size={40} className="text-emerald-500 mb-4" />
              <h2 className="text-xl font-semibold theme-text-primary mb-2">Back up your key</h2>
              {peerId && (
                <div className="flex items-center gap-2 theme-bg-tertiary rounded p-2 mb-4">
                  <code className="text-xs theme-text-secondary truncate flex-1">{peerId}</code>
                  <button onClick={() => { navigator.clipboard.writeText(peerId); setCopied(true); setTimeout(() => setCopied(false), 1500); }}
                    className="theme-text-muted hover:theme-text-primary">
                    {copied ? <CheckCircle2 size={14} className="text-emerald-500" /> : <Copy size={14} />}
                  </button>
                </div>
              )}
              <p className="text-sm theme-text-muted mb-4">
                Anyone with this key controls your identity. Store an encrypted backup somewhere safe
                (password manager, encrypted drive). Without it, losing this machine means losing your
                identity and any repos only you host.
              </p>
              <div className="flex gap-3">
                <button onClick={doBackup} disabled={busy || backupDone}
                  className="flex items-center gap-2 bg-emerald-600 hover:bg-emerald-700 disabled:opacity-50 text-white text-sm font-medium rounded px-5 py-2.5">
                  {backupDone ? <CheckCircle2 size={16} /> : busy ? <RefreshCw size={16} className="animate-spin" /> : <Download size={16} />}
                  {backupDone ? 'Backed up' : 'Export encrypted backup'}
                </button>
                <button onClick={() => setStep('clone-demo')} className="flex items-center gap-2 theme-bg-tertiary hover:theme-bg-hover theme-text-secondary text-sm rounded px-5 py-2.5">
                  {backupDone ? 'Continue' : 'Skip for now'} <ArrowRight size={16} />
                </button>
              </div>
            </>
          )}

          {step === 'clone-demo' && (
            <>
              <FolderGit2 size={40} className="text-emerald-500 mb-4" />
              <h2 className="text-xl font-semibold theme-text-primary mb-2">Clone your first repo</h2>
              <p className="text-sm theme-text-muted mb-4">
                Paste a <code className="text-xs">gitforest://&lt;peer&gt;/&lt;repo&gt;</code> URL to clone from the network.
                If the public bootstrap network is up, try the canonical demo repo.
              </p>
              <input
                value={cloneUrl}
                onChange={(e) => setCloneUrl(e.target.value)}
                placeholder="gitforest://12D3KooW…/myrepo"
                className="w-full theme-bg-tertiary border theme-border rounded px-3 py-2 text-sm theme-text-primary mb-4 outline-none focus:border-emerald-500"
              />
              <div className="flex gap-3">
                <button onClick={doClone} disabled={!cloneUrl || cloneStatus === 'cloning'}
                  className="flex items-center gap-2 bg-emerald-600 hover:bg-emerald-700 disabled:opacity-50 text-white text-sm font-medium rounded px-5 py-2.5">
                  {cloneStatus === 'cloning' && <RefreshCw size={16} className="animate-spin" />}
                  {cloneStatus === 'ok' && <CheckCircle2 size={16} />}
                  {cloneStatus === 'ok' ? 'Cloned' : cloneStatus === 'cloning' ? 'Cloning…' : 'Clone'}
                </button>
                <button onClick={() => setStep('done')} className="theme-bg-tertiary hover:theme-bg-hover theme-text-secondary text-sm rounded px-5 py-2.5">
                  {cloneStatus === 'ok' ? 'Continue' : 'Skip'}
                </button>
              </div>
              {cloneStatus === 'fail' && (
                <p className="text-xs text-red-400 mt-3">Clone failed — check the URL and that bootstrap peers are reachable.</p>
              )}
            </>
          )}

          {step === 'done' && (
            <>
              <CheckCircle2 size={48} className="text-emerald-500 mb-4" />
              <h2 className="text-xl font-semibold theme-text-primary mb-2">You're in the forest</h2>
              <p className="text-sm theme-text-muted mb-2">Identity: <code className="text-xs">{peerId || 'created'}</code></p>
              <ul className="text-sm theme-text-muted space-y-1.5 mb-6 mt-4">
                <li className="flex items-center gap-2"><CheckCircle2 size={14} className="text-emerald-500" /> Identity generated</li>
                <li className="flex items-center gap-2">{backupDone ? <CheckCircle2 size={14} className="text-emerald-500" /> : <AlertTriangle size={14} className="text-amber-500" />} Key backup {backupDone ? 'exported' : 'skipped (do it from Profile)'}</li>
                <li className="flex items-center gap-2">{cloneStatus === 'ok' ? <CheckCircle2 size={14} className="text-emerald-500" /> : <AlertTriangle size={14} className="text-amber-500" />} First clone {cloneStatus === 'ok' ? 'complete' : 'skipped'}</li>
              </ul>
              <button onClick={onComplete} className="flex items-center gap-2 bg-emerald-600 hover:bg-emerald-700 text-white text-sm font-medium rounded px-5 py-2.5">
                Open GitForest <ArrowRight size={16} />
              </button>
            </>
          )}
        </div>

        {step !== 'welcome' && step !== 'done' && (
          <button onClick={() => setStep(steps[Math.max(0, stepIdx - 1)].id)} className="flex items-center gap-1 text-xs theme-text-muted hover:theme-text-primary mt-4">
            <ArrowLeft size={12} /> Back
          </button>
        )}
      </div>
    </div>
  );
}
