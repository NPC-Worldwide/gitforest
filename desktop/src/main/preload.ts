import { contextBridge, ipcRenderer, IpcRendererEvent } from 'electron';

export interface IGitForestAPI {
  api: (route: string, opts?: { method?: string; body?: unknown }) => Promise<any>;
  daemonState: () => Promise<{ running: boolean; owned: boolean }>;
  daemonStart: () => Promise<{ ok: boolean; reason?: string }>;
  daemonStop: () => Promise<{ ok: boolean }>;
  daemonRestart: () => Promise<{ ok: boolean; reason?: string }>;
  onDaemonStateChange: (cb: (s: { running: boolean }) => void) => () => void;
  identityExists: () => Promise<boolean>;
  initIdentity: () => Promise<{ ok: boolean; error?: string }>;
  onboardedGet: () => Promise<boolean>;
  onboardedSet: () => Promise<{ ok: boolean; error?: string }>;
  configDir: () => Promise<string>;
  dataDir: () => Promise<string>;
  readLog: (tail?: number) => Promise<{ ok: boolean; data?: string; error?: string }>;
  pickDirectory: () => Promise<any>;
  pickSavePath: (defaultName: string) => Promise<any>;
  pickFile: () => Promise<any>;
  saveFile: (filePath: string, content: string) => Promise<{ ok: boolean; error?: string }>;
  openExternal: (url: string) => Promise<any>;
  windowControls: { minimize: () => void; maximize: () => void; close: () => void };
  windowState: { isMaximized: () => Promise<boolean> };
  onWindowStateChange: (cb: (s: { isMaximized: boolean }) => void) => () => void;
  checkForUpdates: () => Promise<any>;
  getAppVersion: () => Promise<string>;
}

contextBridge.exposeInMainWorld('gf', {
  api: (route: string, opts?: any) => ipcRenderer.invoke('gf:api', route, opts),
  daemonState: () => ipcRenderer.invoke('gf:daemon-state'),
  daemonStart: () => ipcRenderer.invoke('gf:daemon-start'),
  daemonStop: () => ipcRenderer.invoke('gf:daemon-stop'),
  daemonRestart: () => ipcRenderer.invoke('gf:daemon-restart'),
  onDaemonStateChange: (cb: any) => {
    const h = (_e: IpcRendererEvent, s: any) => cb(s);
    ipcRenderer.on('daemon-state-changed', h);
    return () => ipcRenderer.removeListener('daemon-state-changed', h);
  },
  identityExists: () => ipcRenderer.invoke('gf:identity-exists'),
  initIdentity: () => ipcRenderer.invoke('gf:init-identity'),
  onboardedGet: () => ipcRenderer.invoke('gf:onboarded-get'),
  onboardedSet: () => ipcRenderer.invoke('gf:onboarded-set'),
  configDir: () => ipcRenderer.invoke('gf:config-dir'),
  dataDir: () => ipcRenderer.invoke('gf:data-dir'),
  readLog: (tail?: number) => ipcRenderer.invoke('gf:read-log', tail),
  pickDirectory: () => ipcRenderer.invoke('gf:pick-directory'),
  pickSavePath: (name: string) => ipcRenderer.invoke('gf:pick-save-path', name),
  pickFile: () => ipcRenderer.invoke('gf:pick-file'),
  saveFile: (p: string, c: string) => ipcRenderer.invoke('gf:save-file', p, c),
  openExternal: (url: string) => ipcRenderer.invoke('gf:open-external', url),
  windowControls: {
    minimize: () => ipcRenderer.send('window-minimize'),
    maximize: () => ipcRenderer.send('window-maximize'),
    close: () => ipcRenderer.send('window-close'),
  },
  windowState: { isMaximized: () => ipcRenderer.invoke('window-is-maximized') },
  onWindowStateChange: (cb: any) => {
    const h = (_e: IpcRendererEvent, s: any) => cb(s);
    ipcRenderer.on('window-state-changed', h);
    return () => ipcRenderer.removeListener('window-state-changed', h);
  },
  checkForUpdates: () => ipcRenderer.invoke('check-for-updates'),
  getAppVersion: () => ipcRenderer.invoke('get-app-version'),
} as IGitForestAPI);

declare global {
  interface Window {
    gf: IGitForestAPI;
  }
}
