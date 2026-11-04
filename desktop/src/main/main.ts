import { app, BrowserWindow, ipcMain, dialog, shell } from 'electron';
import path from 'path';
import fs from 'fs';
import os from 'os';
import { spawn, execSync, execFileSync, ChildProcess } from 'child_process';
import { fileURLToPath } from 'url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

const IS_DEV = process.env.NODE_ENV === 'development' || process.argv.includes('--dev');
const ICON_PATH = path.join(__dirname, '..', 'gitforest.png');
const API_PORT = parseInt(process.env.GF_API_PORT || '7878', 10) || 7878;
const P2P_PORT = parseInt(process.env.GF_P2P_PORT || '9418', 10) || 9418;
export const DAEMON_URL = `http://127.0.0.1:${API_PORT}`;

const GF_CONFIG_DIR = path.join(os.homedir(), '.config', 'gitforest');
const GF_DATA_DIR = path.join(os.homedir(), '.local', 'share', 'gitforest');
const TOKEN_PATH = path.join(GF_DATA_DIR, 'api.token');
const ONBOARDED_PATH = path.join(GF_DATA_DIR, '.onboarded');

let daemonProcess: ChildProcess | null = null;
let weOwnDaemon = false;
let mainWindow: BrowserWindow | null = null;

// ---------- daemon helpers ----------

function findBinaries(): { gitforest: string | null; remoteHelper: string | null } {
  const candidates: string[] = [];
  const pinned = (process.env.GF_GITFOREST_BIN || '').trim();
  if (pinned) candidates.push(pinned);
  if (app.isPackaged) {
    const base = path.join(process.resourcesPath, 'bin');
    candidates.push(path.join(base, 'gitforest'));
  } else {
    // dev: repo target/release next to desktop/
    candidates.push(path.join(__dirname, '..', '..', 'target', 'release', 'gitforest'));
    candidates.push(path.join(__dirname, '..', '..', 'target', 'debug', 'gitforest'));
  }
  candidates.push(path.join(os.homedir(), '.local', 'bin', 'gitforest'));
  candidates.push('/usr/local/bin/gitforest');
  candidates.push('/usr/bin/gitforest');

  let gitforest: string | null = null;
  for (const c of candidates) {
    if (fs.existsSync(c)) { gitforest = c; break; }
  }
  if (!gitforest) {
    try {
      gitforest = execSync('which gitforest', { encoding: 'utf8' }).trim().split('\n')[0] || null;
    } catch { gitforest = null; }
  }

  let remoteHelper: string | null = null;
  if (gitforest) {
    const dir = path.dirname(gitforest);
    const rh = path.join(dir, 'git-remote-gitforest');
    if (fs.existsSync(rh)) remoteHelper = rh;
    else {
      try {
        remoteHelper = execSync('which git-remote-gitforest', { encoding: 'utf8' }).trim().split('\n')[0] || null;
      } catch { remoteHelper = null; }
    }
  }
  return { gitforest, remoteHelper };
}

export function readApiToken(): string | null {
  try {
    if (fs.existsSync(TOKEN_PATH)) {
      return fs.readFileSync(TOKEN_PATH, 'utf8').trim();
    }
  } catch {}
  return null;
}

async function daemonHealthy(): Promise<boolean> {
  try {
    const controller = new AbortController();
    const t = setTimeout(() => controller.abort(), 1500);
    const res = await fetch(`${DAEMON_URL}/api/status`, { signal: controller.signal });
    clearTimeout(t);
    return res.status === 200 || res.status === 401; // 401 = alive but needs token
  } catch {
    return false;
  }
}

async function waitForDaemon(maxAttempts = 45, delayMs = 500): Promise<boolean> {
  for (let i = 1; i <= maxAttempts; i++) {
    if (await daemonHealthy()) return true;
    await new Promise(r => setTimeout(r, delayMs));
  }
  return false;
}

function spawnDaemon(): boolean {
  const { gitforest } = findBinaries();
  if (!gitforest) {
    console.error('[Main] gitforest binary not found');
    return false;
  }
  fs.mkdirSync(GF_CONFIG_DIR, { recursive: true });
  fs.mkdirSync(GF_DATA_DIR, { recursive: true });

  const logPath = path.join(GF_DATA_DIR, 'daemon.log');
  const logFd = fs.openSync(logPath, 'a');
  const env = { ...process.env, RUST_LOG: process.env.RUST_LOG || 'info' };

  if (process.platform === 'darwin') {
    try {
      const dir = path.dirname(gitforest);
      for (const f of fs.readdirSync(dir)) {
        try {
          execFileSync('/usr/bin/xattr', ['-d', 'com.apple.quarantine', path.join(dir, f)], { stdio: 'ignore' });
        } catch {}
      }
    } catch {}
  }

  console.log(`[gf:main] spawning daemon: ${gitforest} daemon (log: ${logPath})`);
  daemonProcess = spawn(gitforest, ['daemon'], {
    stdio: ['ignore', logFd, logFd],
    detached: process.platform !== 'win32',
    env,
  });
  weOwnDaemon = true;
  daemonProcess.on('error', (e) => console.error('[Daemon error]', e.message));
  daemonProcess.on('close', (code) => {
    console.log(`[Daemon] exited with code ${code}`);
    daemonProcess = null;
    weOwnDaemon = false;
    mainWindow?.webContents.send('daemon-state-changed', { running: false });
  });
  return true;
}

export async function ensureDaemon(): Promise<{ ok: boolean; reason?: string }> {
  if (await daemonHealthy()) return { ok: true };
  if (!spawnDaemon()) return { ok: false, reason: 'binary-not-found' };
  const up = await waitForDaemon();
  return up ? { ok: true } : { ok: false, reason: 'startup-timeout' };
}

export function stopDaemon(): void {
  if (!daemonProcess || !weOwnDaemon) return;
  const pid = daemonProcess.pid;
  if (!pid) { daemonProcess = null; return; }
  if (process.platform === 'win32') {
    try { execSync(`taskkill /F /T /PID ${pid}`, { stdio: 'ignore' }); } catch {}
  } else {
    try { process.kill(-pid, 'SIGTERM'); } catch {}
  }
  daemonProcess = null;
  weOwnDaemon = false;
}

// ---------- generic API proxy (adds bearer token) ----------

export function identityExists(): boolean {
  const { gitforest } = findBinaries();
  if (!gitforest) return false;
  try {
    execFileSync(gitforest, ['id'], { encoding: 'utf8', timeout: 10000, stdio: 'pipe' });
    return true;
  } catch {
    return false;
  }
}

export function runInit(): { ok: boolean; error?: string } {
  const { gitforest } = findBinaries();
  if (!gitforest) return { ok: false, error: 'binary-not-found' };
  fs.mkdirSync(GF_CONFIG_DIR, { recursive: true });
  fs.mkdirSync(GF_DATA_DIR, { recursive: true });
  try {
    execFileSync(gitforest, ['init'], { encoding: 'utf8', timeout: 15000, stdio: 'pipe' });
    return identityExists() ? { ok: true } : { ok: false, error: 'identity file missing after init' };
  } catch (e) {
    return { ok: false, error: (e as Error).message };
  }
}

export function isOnboarded(): boolean {
  return fs.existsSync(ONBOARDED_PATH);
}

export function setOnboarded(): { ok: boolean; error?: string } {
  try {
    fs.mkdirSync(GF_DATA_DIR, { recursive: true });
    fs.writeFileSync(ONBOARDED_PATH, new Date().toISOString());
    return { ok: true };
  } catch (e) {
    return { ok: false, error: (e as Error).message };
  }
}

interface ProxyOptions {
  method?: string;
  body?: unknown;
  raw?: boolean;
}

export async function apiFetch(route: string, opts: ProxyOptions = {}): Promise<{ ok: boolean; status: number; data?: unknown; error?: string }> {
  const token = readApiToken();
  const headers: Record<string, string> = {};
  if (token) headers['Authorization'] = `Bearer ${token}`;
  if (opts.body !== undefined) headers['Content-Type'] = 'application/json';
  try {
    const res = await fetch(`${DAEMON_URL}${route}`, {
      method: opts.method || 'GET',
      headers,
      body: opts.body !== undefined ? JSON.stringify(opts.body) : undefined,
    });
    const ct = res.headers.get('content-type') || '';
    let data: unknown;
    if (ct.includes('json')) data = await res.json();
    else data = await res.text();
    if (!res.ok) console.error(`[gf:main] api ${opts.method || 'GET'} ${route} -> HTTP ${res.status}`);
    return { ok: res.ok, status: res.status, data };
  } catch (e) {
    console.error(`[gf:main] api ${opts.method || 'GET'} ${route} transport error: ${(e as Error).message}`);
    return { ok: false, status: 0, error: (e as Error).message };
  }
}

// ---------- window ----------

function createWindow() {
  mainWindow = new BrowserWindow({
    width: 1400,
    height: 900,
    minWidth: 900,
    minHeight: 600,
    titleBarStyle: 'hiddenInset',
    ...(fs.existsSync(ICON_PATH) ? { icon: ICON_PATH } : {}),
    webPreferences: {
      preload: path.join(__dirname, 'preload.cjs'),
      contextIsolation: true,
      nodeIntegration: false,
    },
  });

  mainWindow.on('maximize', () => mainWindow?.webContents.send('window-state-changed', { isMaximized: true }));
  mainWindow.on('unmaximize', () => mainWindow?.webContents.send('window-state-changed', { isMaximized: false }));

  if (IS_DEV) {
    mainWindow.loadURL('http://localhost:7457');
    mainWindow.webContents.openDevTools();
  } else {
    mainWindow.loadFile(path.join(__dirname, '../dist/index.html'));
  }
}

app.on('before-quit', () => stopDaemon());

app.whenReady().then(() => {
  createWindow();
  ensureDaemon().then((r) => {
    if (r.ok) mainWindow?.webContents.send('daemon-state-changed', { running: true });
  });
  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') app.quit();
});

// ---------- IPC ----------

ipcMain.handle('gf:daemon-state', async () => ({
  running: await daemonHealthy(),
  owned: weOwnDaemon,
}));

ipcMain.handle('gf:daemon-start', async () => ensureDaemon());

ipcMain.handle('gf:daemon-stop', async () => {
  stopDaemon();
  return { ok: true };
});

ipcMain.handle('gf:daemon-restart', async () => {
  stopDaemon();
  await new Promise(r => setTimeout(r, 1200));
  return ensureDaemon();
});

ipcMain.handle('gf:api', async (_e, route: string, opts?: ProxyOptions) => apiFetch(route, opts));

ipcMain.handle('gf:identity-exists', async () => identityExists());

ipcMain.handle('gf:init-identity', async () => runInit());

ipcMain.handle('gf:onboarded-get', async () => isOnboarded());

ipcMain.handle('gf:onboarded-set', async () => setOnboarded());

ipcMain.handle('gf:config-dir', () => GF_CONFIG_DIR);
ipcMain.handle('gf:data-dir', () => GF_DATA_DIR);

ipcMain.handle('gf:read-log', async (_e, tailLines = 200) => {
  const logPath = path.join(GF_DATA_DIR, 'daemon.log');
  try {
    if (!fs.existsSync(logPath)) return { ok: true, data: '' };
    const content = fs.readFileSync(logPath, 'utf8');
    const lines = content.trim().split('\n');
    return { ok: true, data: lines.slice(-tailLines).join('\n') };
  } catch (e) {
    return { ok: false, error: (e as Error).message };
  }
});

ipcMain.handle('gf:pick-directory', async (_e, message?: string) => {
  const win = BrowserWindow.getFocusedWindow();
  if (!win) return { canceled: true };
  return dialog.showOpenDialog(win, { properties: ['openDirectory'], ...(message ? { title: message, message } : {}) });
});

ipcMain.handle('gf:pick-save-path', async (_e, defaultName: string) => {
  const win = BrowserWindow.getFocusedWindow();
  if (!win) return { canceled: true };
  return dialog.showSaveDialog(win, { defaultPath: path.join(os.homedir(), defaultName) });
});

ipcMain.handle('gf:pick-file', async () => {
  const win = BrowserWindow.getFocusedWindow();
  if (!win) return { canceled: true };
  return dialog.showOpenDialog(win, { properties: ['openFile'] });
});

ipcMain.handle('gf:save-file', async (_e, filePath: string, content: string) => {
  try {
    fs.writeFileSync(filePath, content, { mode: 0o600 });
    return { ok: true };
  } catch (e) {
    return { ok: false, error: (e as Error).message };
  }
});

ipcMain.handle('gf:open-external', async (_e, url: string) => {
  if (typeof url !== 'string' || !(url.startsWith('https://') || url.startsWith('http://'))) {
    return { ok: false, error: 'blocked: only http(s) URLs may be opened externally' };
  }
  await shell.openExternal(url);
  return { ok: true };
});

// window controls
ipcMain.on('window-close', () => mainWindow?.close());
ipcMain.on('window-minimize', () => mainWindow?.minimize());
ipcMain.on('window-maximize', () => {
  if (!mainWindow) return;
  if (mainWindow.isMaximized()) mainWindow.unmaximize();
  else mainWindow.maximize();
});
ipcMain.handle('window-is-maximized', () => mainWindow?.isMaximized() ?? false);

const APP_VERSION = (() => {
  try {
    const pkg = JSON.parse(fs.readFileSync(path.join(__dirname, '..', 'package.json'), 'utf8'));
    return pkg.version as string;
  } catch { return '0.0.0'; }
})();

function cmpVer(a: string, b: string): number {
  const pa = a.replace(/^v/, '').split('.').map(Number);
  const pb = b.replace(/^v/, '').split('.').map(Number);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const na = pa[i] || 0, nb = pb[i] || 0;
    if (na > nb) return 1;
    if (na < nb) return -1;
  }
  return 0;
}

ipcMain.handle('get-app-version', () => APP_VERSION);

const MANIFEST_URL = 'https://storage.googleapis.com/gitforest-executables/manifest.json';

function updateAssetForPlatform(downloads: any): { url: string; file: string; kind: string } | null {
  if (!downloads) return null;
  if (process.platform === 'darwin' && downloads['desktop-macos-dmg']) {
    return { url: downloads['desktop-macos-dmg'], file: 'gitforest-desktop-macos-arm64.dmg', kind: 'dmg' };
  }
  if (process.platform === 'linux') {
    if (process.env.APPIMAGE && downloads['desktop-linux-appimage']) {
      return { url: downloads['desktop-linux-appimage'], file: 'gitforest-desktop-linux-x64.AppImage', kind: 'appimage' };
    }
    if (downloads['desktop-linux-deb']) {
      return { url: downloads['desktop-linux-deb'], file: 'gitforest-desktop-linux-x64.deb', kind: 'deb' };
    }
  }
  return null;
}

function runCmd(cmd: string, args: string[], timeoutMs = 180000): Promise<{ code: number; stderr: string }> {
  return new Promise((resolve) => {
    const p = spawn(cmd, args, { stdio: ['ignore', 'pipe', 'pipe'] });
    let stderr = '';
    p.stderr?.on('data', (d) => { stderr += d.toString(); });
    const t = setTimeout(() => { try { p.kill('SIGKILL'); } catch {} }, timeoutMs);
    p.on('error', () => { clearTimeout(t); resolve({ code: -1, stderr }); });
    p.on('close', (code) => { clearTimeout(t); resolve({ code: code ?? -1, stderr }); });
  });
}

ipcMain.handle('check-for-updates', async () => {
  try {
    const res = await fetch(MANIFEST_URL, { signal: AbortSignal.timeout(15000) });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const manifest: any = await res.json();
    const latest = (manifest.version || '0.0.0') as string;
    const hasUpdate = cmpVer(latest, APP_VERSION) > 0;
    console.log(`[gf:main] update check: installed=${APP_VERSION} latest=${latest} hasUpdate=${hasUpdate}`);
    return {
      success: true,
      currentVersion: APP_VERSION,
      latestVersion: latest.replace(/^v/, ''),
      hasUpdate,
      releaseUrl: 'https://gitforest.net/downloads.html',
      asset: updateAssetForPlatform(manifest.downloads),
      devMode: IS_DEV,
    };
  } catch (e) {
    return { success: false, error: (e as Error).message, currentVersion: APP_VERSION };
  }
});

ipcMain.handle('download-update', async (_e, url: string, file: string) => {
  try {
    const safe = path.basename(file).replace(/[^A-Za-z0-9._-]/g, '_');
    if (!url.startsWith('https://storage.googleapis.com/gitforest-executables/')) {
      throw new Error('refusing to download from an unexpected host');
    }
    console.log(`[gf:main] update download started: ${url}`);
    const res = await fetch(url, { signal: AbortSignal.timeout(600000) });
    if (!res.ok || !res.body) throw new Error(`HTTP ${res.status}`);
    const total = Number(res.headers.get('content-length') || 0);
    const dest = path.join(os.tmpdir(), safe);
    const out = fs.createWriteStream(dest);
    let received = 0;
    const reader = res.body.getReader();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      received += value.length;
      if (!out.write(value)) await new Promise<void>((r) => out.once('drain', () => r()));
      mainWindow?.webContents.send('update-download-progress', { received, total });
    }
    await new Promise<void>((resolve, reject) => {
      out.once('error', reject);
      out.once('finish', () => resolve());
      out.end();
    });
    console.log(`[gf:main] update download complete: ${dest} (${received} bytes)`);
    return { success: true, path: dest };
  } catch (e) {
    console.error(`[gf:main] update download failed: ${(e as Error).message}`);
    return { success: false, error: (e as Error).message };
  }
});

ipcMain.handle('install-update', async (_e, filePath: string, kind: string) => {
  try {
    if (IS_DEV) throw new Error('in-app update is disabled in dev mode');
    if (!filePath.startsWith(os.tmpdir() + path.sep)) throw new Error('unexpected update path');
    console.log(`[gf:main] update install started: kind=${kind} path=${filePath}`);
    if (process.platform === 'linux' && kind === 'appimage' && process.env.APPIMAGE) {
      fs.copyFileSync(filePath, process.env.APPIMAGE);
      fs.chmodSync(process.env.APPIMAGE, 0o755);
      console.log(`[gf:main] update install: AppImage replaced at ${process.env.APPIMAGE}`);
    } else if (process.platform === 'linux' && kind === 'deb') {
      const r = await runCmd('pkexec', ['dpkg', '-i', filePath]);
      if (r.code !== 0) throw new Error(r.stderr.trim().split('\n').pop() || 'package install failed');
      console.log('[gf:main] update install: deb installed');
    } else if (process.platform === 'darwin' && kind === 'dmg') {
      const mnt = path.join(os.tmpdir(), 'gf-update-mnt');
      fs.rmSync(mnt, { recursive: true, force: true });
      fs.mkdirSync(mnt, { recursive: true });
      const attached = await runCmd('hdiutil', ['attach', '-nobrowse', '-mountpoint', mnt, filePath]);
      if (attached.code !== 0) throw new Error('could not mount update image');
      try {
        const src = path.join(mnt, 'gitforest.app');
        if (!fs.existsSync(src)) throw new Error('update image has no gitforest.app');
        fs.rmSync('/Applications/gitforest.app', { recursive: true, force: true });
        fs.cpSync(src, '/Applications/gitforest.app', { recursive: true });
      } finally {
        await runCmd('hdiutil', ['detach', mnt]);
      }
    } else {
      throw new Error('unsupported platform for in-app update');
    }
    stopDaemon();
    try { fs.rmSync(filePath); } catch {}
    console.log('[gf:main] update install complete, relaunching');
    app.relaunch();
    app.exit(0);
    return { success: true };
  } catch (e) {
    console.error(`[gf:main] update install failed: ${(e as Error).message}`);
    return { success: false, error: (e as Error).message };
  }
});
