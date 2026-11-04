'use strict';

const path = require('node:path');
const fs = require('node:fs');
const os = require('node:os');
const { spawn, execFileSync } = require('node:child_process');
const readline = require('node:readline');

const ROOT = path.resolve(__dirname, '..');
const CDP_PORT = process.env.GUI_CDP_PORT || '9222';
process.env.GUI_CDP = process.env.GUI_CDP || `http://127.0.0.1:${CDP_PORT}`;
const CDP_HOST = process.env.GUI_CDP;
const OUT = process.env.GUI_OUT || path.join(__dirname, 'out');

const { connect } = require('./driver');

function gfBin() {
  if (process.env.GF_BIN && fs.existsSync(process.env.GF_BIN)) return process.env.GF_BIN;
  const rel = path.join(ROOT, 'target/release/gitforest');
  if (fs.existsSync(rel)) return rel;
  return 'gitforest';
}

function electronBin() {
  return path.join(ROOT, 'desktop/node_modules/electron/dist/electron');
}

const state = {
  app: null,
  xvfb: null,
  session: null,
  tmp: null,
};

async function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

async function waitHttpOk(url, timeoutMs, what) {
  const start = Date.now();
  for (;;) {
    try {
      const res = await fetch(url);
      if (res.ok) return;
    } catch {
      /* retry */
    }
    if (Date.now() - start > timeoutMs) throw new Error(`timeout waiting for ${what}`);
    await sleep(2000);
  }
}

async function waitCdpPage(timeoutMs) {
  const start = Date.now();
  for (;;) {
    try {
      const res = await fetch(`${CDP_HOST}/json/list`);
      const targets = await res.json();
      if (targets.some((t) => t.type === 'page')) return;
    } catch {
      /* retry */
    }
    if (Date.now() - start > timeoutMs) throw new Error('timeout waiting for electron page target on CDP');
    await sleep(2000);
  }
}

async function toolLaunch() {
  if (state.session) throw new Error('already launched; call gui_stop first');
  if (!fs.existsSync(path.join(ROOT, 'desktop/dist-electron/main.js'))) {
    throw new Error('desktop not built (run npm build steps in desktop/ first)');
  }
  try {
    const res = await fetch('http://127.0.0.1:7878/api/status');
    if (res.ok) throw new Error('port 7878 already in use (stop that daemon first)');
  } catch (e) {
    if (e.message.startsWith('port 7878')) throw e;
  }
  let display = process.env.DISPLAY;
  if (!display) {
    state.xvfb = spawn('/usr/bin/Xvfb', [':42', '-screen', '0', '1400x900x24'], {
      stdio: 'ignore',
    });
    display = ':42';
    await sleep(3000);
  }
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'gf-gui-mcp-'));
  const home = path.join(tmp, 'home');
  fs.mkdirSync(home, { recursive: true });
  state.tmp = tmp;
  execFileSync(gfBin(), ['init', '--name', 'gui-mcp'], {
    env: { ...process.env, HOME: home },
    stdio: 'ignore',
  });
  fs.mkdirSync(path.join(home, '.local/share/gitforest'), { recursive: true });
  fs.writeFileSync(path.join(home, '.local/share/gitforest/.onboarded'), '');
  fs.mkdirSync(OUT, { recursive: true });
  const logFd = fs.openSync(path.join(OUT, 'app.log'), 'a');
  const app = spawn(
    electronBin(),
    ['.', '--no-sandbox', `--remote-debugging-port=${CDP_PORT}`, `--user-data-dir=${path.join(tmp, 'udata')}`],
    {
      cwd: path.join(ROOT, 'desktop'),
      env: { ...process.env, DISPLAY: display, HOME: home, XDG_RUNTIME_DIR: tmp },
      stdio: ['ignore', logFd, logFd],
      detached: true,
    }
  );
  state.app = app;
  app.on('exit', () => {
    try {
      fs.closeSync(logFd);
    } catch {
      /* ignore */
    }
    if (state.app === app) state.app = null;
  });
  await waitHttpOk('http://127.0.0.1:7878/api/status', 120000, 'app-spawned daemon');
  await waitCdpPage(60000);
  state.session = await connect();
  return `launched (cdp ${CDP_HOST}, home ${home})`;
}

function requireSession() {
  if (!state.session) throw new Error('not launched; call gui_launch first');
  return state.session;
}

async function toolStop() {
  const errs = [];
  if (state.session) {
    try {
      state.session.close();
    } catch (e) {
      errs.push(`session: ${e.message}`);
    }
    state.session = null;
  }
  const procs = [];
  const killOne = (proc, key, grouped) =>
    new Promise((resolve) => {
      const target = grouped ? -proc.pid : proc.pid;
      proc.once('exit', () => resolve());
      try {
        process.kill(target, 'SIGTERM');
      } catch (e) {
        errs.push(`${key}: ${e.message}`);
        resolve();
        return;
      }
      setTimeout(() => {
        try {
          process.kill(target, 'SIGKILL');
        } catch {
          /* already gone */
        }
        resolve();
      }, 5000).unref();
    });
  for (const [key, grouped] of [['app', true], ['xvfb', false]]) {
    const proc = state[key];
    if (proc && proc.pid) {
      procs.push(proc.exitCode === null ? killOne(proc, key, grouped) : Promise.resolve());
      state[key] = null;
    }
  }
  await Promise.all(procs);
  if (state.tmp) {
    try {
      fs.rmSync(state.tmp, { recursive: true, force: true });
    } catch (e) {
      errs.push(`tmp: ${e.message}`);
    }
    state.tmp = null;
  }
  if (errs.length > 0) throw new Error(errs.join('; '));
  return 'stopped';
}

const TOOLS = [
  {
    name: 'gui_launch',
    description: 'Launch the gitforest desktop app under a scratch home with CDP enabled and connect. Fails if port 7878 is busy.',
    inputSchema: { type: 'object', properties: {}, additionalProperties: false },
    run: () => toolLaunch(),
  },
  {
    name: 'gui_click_text',
    description: 'Click a button containing the given text.',
    inputSchema: {
      type: 'object',
      properties: { text: { type: 'string' } },
      required: ['text'],
      additionalProperties: false,
    },
    run: async ({ text }) => {
      await requireSession().clickText(text);
      return `clicked button: ${text}`;
    },
  },
  {
    name: 'gui_click',
    description: 'Click the element matching a CSS selector.',
    inputSchema: {
      type: 'object',
      properties: { selector: { type: 'string' } },
      required: ['selector'],
      additionalProperties: false,
    },
    run: async ({ selector }) => {
      await requireSession().click(selector);
      return `clicked: ${selector}`;
    },
  },
  {
    name: 'gui_fill',
    description: 'Clear an input and type text into it.',
    inputSchema: {
      type: 'object',
      properties: { selector: { type: 'string' }, text: { type: 'string' } },
      required: ['selector', 'text'],
      additionalProperties: false,
    },
    run: async ({ selector, text }) => {
      await requireSession().fill(selector, text);
      return `filled ${selector}`;
    },
  },
  {
    name: 'gui_screenshot',
    description: 'Capture the current window as a PNG image (also saved under gui-test/out/).',
    inputSchema: { type: 'object', properties: {}, additionalProperties: false },
    run: async () => {
      const file = path.join(OUT, `mcp-${Date.now()}.png`);
      await requireSession().screenshot(file);
      const data = fs.readFileSync(file).toString('base64');
      return {
        content: [
          { type: 'text', text: `saved to ${file}` },
          { type: 'image', data, mimeType: 'image/png' },
        ],
      };
    },
  },
  {
    name: 'gui_body_text',
    description: 'Visible text of the current page.',
    inputSchema: { type: 'object', properties: {}, additionalProperties: false },
    run: async () => requireSession().bodyText(),
  },
  {
    name: 'gui_wait_text',
    description: 'Wait until the page contains the given text.',
    inputSchema: {
      type: 'object',
      properties: {
        text: { type: 'string' },
        timeout_ms: { type: 'number' },
      },
      required: ['text'],
      additionalProperties: false,
    },
    run: async ({ text, timeout_ms }) => {
      await requireSession().waitForText(text, { timeoutMs: timeout_ms || 30000 });
      return `saw text: ${text}`;
    },
  },
  {
    name: 'gui_wait_selector',
    description: 'Wait until a CSS selector matches.',
    inputSchema: {
      type: 'object',
      properties: {
        selector: { type: 'string' },
        timeout_ms: { type: 'number' },
      },
      required: ['selector'],
      additionalProperties: false,
    },
    run: async ({ selector, timeout_ms }) => {
      await requireSession().waitForSelector(selector, { timeoutMs: timeout_ms || 30000 });
      return `saw selector: ${selector}`;
    },
  },
  {
    name: 'gui_evaluate',
    description: 'Evaluate a JS expression in the page and return its JSON value.',
    inputSchema: {
      type: 'object',
      properties: { expression: { type: 'string' } },
      required: ['expression'],
      additionalProperties: false,
    },
    run: async ({ expression }) => {
      const value = await requireSession().evaluate(expression);
      return typeof value === 'string' ? value : JSON.stringify(value);
    },
  },
  {
    name: 'gui_console_errors',
    description: 'Page errors and console.error entries collected since launch (favicon/net noise filtered).',
    inputSchema: { type: 'object', properties: {}, additionalProperties: false },
    run: async () => {
      const errors = requireSession().consoleErrors.filter(
        (e) => !e.includes('favicon') && !e.toLowerCase().includes('net::')
      );
      return errors.length === 0 ? 'no console errors' : errors.join('\n');
    },
  },
  {
    name: 'gui_stop',
    description: 'Close the app and clean up the scratch home.',
    inputSchema: { type: 'object', properties: {}, additionalProperties: false },
    run: () => toolStop(),
  },
];

function ok(id, result) {
  return { jsonrpc: '2.0', id, result };
}

function err(id, code, message) {
  return { jsonrpc: '2.0', id, error: { code, message } };
}

async function handle(msg) {
  const method = msg.method;
  if (!method || method.startsWith('notifications/') || msg.id === undefined) return null;
  const id = msg.id;
  if (method === 'initialize') {
    return ok(id, {
      protocolVersion: '2024-11-05',
      capabilities: { tools: {} },
      serverInfo: { name: 'gitforest-gui', version: '0.1.0' },
    });
  }
  if (method === 'ping') return ok(id, {});
  if (method === 'tools/list') {
    return ok(id, { tools: TOOLS.map(({ run, ...def }) => def) });
  }
  if (method === 'tools/call') {
    const name = msg.params && msg.params.name;
    const args = (msg.params && msg.params.arguments) || {};
    const tool = TOOLS.find((t) => t.name === name);
    if (!tool) return err(id, -32602, `unknown tool: ${name}`);
    try {
      const out = await tool.run(args);
      if (out && out.content) return ok(id, out);
      return ok(id, { content: [{ type: 'text', text: String(out) }] });
    } catch (e) {
      return err(id, -32603, e.message || String(e));
    }
  }
  return err(id, -32601, `method not found: ${method}`);
}

async function main() {
  const cleanup = () => {
    toolStop().catch(() => {});
  };
  process.on('SIGTERM', () => {
    cleanup();
    setTimeout(() => process.exit(0), 2000).unref();
  });
  process.on('SIGINT', () => {
    cleanup();
    setTimeout(() => process.exit(0), 2000).unref();
  });
  const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
  for await (const line of rl) {
    const text = line.trim();
    if (!text) continue;
    let msg;
    try {
      msg = JSON.parse(text);
    } catch {
      process.stdout.write(
        JSON.stringify({ jsonrpc: '2.0', id: null, error: { code: -32700, message: 'parse error' } }) + '\n'
      );
      continue;
    }
    const resp = await handle(msg);
    if (resp) process.stdout.write(JSON.stringify(resp) + '\n');
  }
}

main().catch((e) => {
  process.stderr.write(`fatal: ${e.message}\n`);
  process.exit(1);
});
