'use strict';

const CDP_HOST = process.env.GUI_CDP || 'http://127.0.0.1:9222';

async function cdpGet(path) {
  const res = await fetch(CDP_HOST + path);
  if (!res.ok) throw new Error(`CDP GET ${path} -> ${res.status}`);
  return res.json();
}

function buildEvalPayload(id, expression, awaitPromise) {
  return {
    id,
    method: 'Runtime.evaluate',
    params: {
      expression,
      awaitPromise: awaitPromise === true,
      returnByValue: true,
    },
  };
}

function buildScreenshotPayload(id) {
  return { id, method: 'Page.captureScreenshot', params: { format: 'png' } };
}

function buildMousePayload(id, type, x, y) {
  return {
    id,
    method: 'Input.dispatchMouseEvent',
    params: { type, x, y, button: 'left', clickCount: type === 'mousePressed' ? 1 : 0 },
  };
}

function buildKeyPayload(id, type, text) {
  if (type === 'insertText') {
    return { id, method: 'Input.insertText', params: { text } };
  }
  return { id, method: 'Input.dispatchKeyEvent', params: { type, windowsVirtualKeyCode: 13 } };
}

function centerOfRect(r) {
  return { x: Math.round(r.x + r.width / 2), y: Math.round(r.y + r.height / 2) };
}

function extractValue(msg) {
  if (msg.error) throw new Error(`CDP error: ${JSON.stringify(msg.error)}`);
  const res = msg.result && msg.result.result;
  if (!res) throw new Error(`CDP bad result: ${JSON.stringify(msg).slice(0, 200)}`);
  if (res.subtype === 'error' || res.className === 'Error') {
    throw new Error(`page error: ${res.description || res.value}`);
  }
  return res.value;
}

class Session {
  constructor(ws) {
    this.ws = ws;
    this.nextId = 1;
    this.pending = new Map();
    this.consoleErrors = [];
    ws.onmessage = (event) => {
      let msg;
      try {
        msg = JSON.parse(event.data);
      } catch {
        return;
      }
      if (msg.id && this.pending.has(msg.id)) {
        const { resolve, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (msg.error) reject(new Error(JSON.stringify(msg.error)));
        else resolve(msg);
      } else if (msg.method === 'Runtime.exceptionThrown' || msg.method === 'Log.entryAdded') {
        const detail = msg.method === 'Log.entryAdded' ? msg.params?.entry : msg.params?.exceptionDetails;
        const text = detail?.text || detail?.exception?.description || JSON.stringify(detail || {}).slice(0, 300);
        if (msg.method === 'Runtime.exceptionThrown' || (detail && detail.level === 'error')) {
          this.consoleErrors.push(`${msg.method}: ${text}`.slice(0, 500));
        }
      }
    };
    ws.onerror = (event) => {
      for (const { reject } of this.pending.values()) reject(new Error(`ws error: ${event.message || 'unknown'}`));
      this.pending.clear();
    };
  }

  send(payload) {
    const id = this.nextId++;
    const withId = { ...payload, id };
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify(withId));
      setTimeout(() => {
        if (this.pending.has(id)) {
          this.pending.delete(id);
          reject(new Error(`CDP timeout on ${payload.method}`));
        }
      }, 30000).unref?.();
    });
  }

  async evaluate(expression, awaitPromise) {
    const msg = await this.send({ method: 'Runtime.evaluate', params: { expression, awaitPromise: awaitPromise === true, returnByValue: true } });
    return extractValue(msg);
  }

  async screenshot(path) {
    const fs = await import('node:fs');
    const msg = await this.send({ method: 'Page.captureScreenshot', params: { format: 'png' } });
    if (msg.error) throw new Error(`CDP error: ${JSON.stringify(msg.error)}`);
    const data = msg.result && msg.result.data;
    if (!data) throw new Error(`CDP bad screenshot result: ${JSON.stringify(msg).slice(0, 200)}`);
    fs.writeFileSync(path, Buffer.from(data, 'base64'));
    return path;
  }

  async click(selector) {
    const rect = await this.evaluate(
      `(() => { const el = document.querySelector(${JSON.stringify(selector)}); if (!el) return null; const r = el.getBoundingClientRect(); return { x: r.x, y: r.y, width: r.width, height: r.height }; })()`
    );
    if (!rect) throw new Error(`no element: ${selector}`);
    const { x, y } = centerOfRect(rect);
    await this.send({ method: 'Input.dispatchMouseEvent', params: { type: 'mousePressed', x, y, button: 'left', clickCount: 1 } });
    await this.send({ method: 'Input.dispatchMouseEvent', params: { type: 'mouseReleased', x, y, button: 'left', clickCount: 1 } });
  }

  async clickText(text) {
    const rect = await this.evaluate(
      `(() => { const els = [...document.querySelectorAll('button')]; const el = els.find((e) => (e.textContent || '').includes(${JSON.stringify(text)})); if (!el) return null; const r = el.getBoundingClientRect(); return { x: r.x, y: r.y, width: r.width, height: r.height }; })()`
    );
    if (!rect) throw new Error(`no button with text: ${text}`);
    const { x, y } = centerOfRect(rect);
    await this.send({ method: 'Input.dispatchMouseEvent', params: { type: 'mousePressed', x, y, button: 'left', clickCount: 1 } });
    await this.send({ method: 'Input.dispatchMouseEvent', params: { type: 'mouseReleased', x, y, button: 'left', clickCount: 1 } });
  }

  async type(selector, text) {
    await this.click(selector);
    await this.send({ method: 'Input.insertText', params: { text } });
  }

  async clear(selector) {
    await this.click(selector);
    await this.send({ method: 'Input.dispatchKeyEvent', params: { type: 'keyDown', modifiers: 2, windowsVirtualKeyCode: 65 } });
    await this.send({ method: 'Input.dispatchKeyEvent', params: { type: 'keyUp', modifiers: 2, windowsVirtualKeyCode: 65 } });
    await this.send({ method: 'Input.dispatchKeyEvent', params: { type: 'rawKeyDown', windowsVirtualKeyCode: 8 } });
    await this.send({ method: 'Input.dispatchKeyEvent', params: { type: 'keyUp', windowsVirtualKeyCode: 8 } });
  }

  async fill(selector, text) {
    const ok = await this.evaluate(
      `(() => { const el = document.querySelector(${JSON.stringify(selector)}); if (!el) return false; el.focus(); const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype; const setter = Object.getOwnPropertyDescriptor(proto, 'value').set; setter.call(el, ${JSON.stringify(text)}); el.dispatchEvent(new Event('input', { bubbles: true })); return true; })()`
    );
    if (!ok) throw new Error(`no element: ${selector}`);
  }

  async nthRect(tag, index) {
    return this.evaluate(
      `(() => { const els = document.querySelectorAll(${JSON.stringify(tag)}); const el = els[${index}]; if (!el) return null; el.scrollIntoView({ block: 'center' }); const r = el.getBoundingClientRect(); return { x: r.x, y: r.y, width: r.width, height: r.height }; })()`
    );
  }

  async clickRect(rect) {
    if (!rect) throw new Error('null rect');
    const { x, y } = centerOfRect(rect);
    await this.send({ method: 'Input.dispatchMouseEvent', params: { type: 'mousePressed', x, y, button: 'left', clickCount: 1 } });
    await this.send({ method: 'Input.dispatchMouseEvent', params: { type: 'mouseReleased', x, y, button: 'left', clickCount: 1 } });
  }

  async fillNth(tag, index, text) {
    const ok = await this.evaluate(
      `(() => { const els = document.querySelectorAll(${JSON.stringify(tag)}); const el = els[${index}]; if (!el) return false; el.scrollIntoView({ block: 'center' }); el.focus(); const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype; const setter = Object.getOwnPropertyDescriptor(proto, 'value').set; setter.call(el, ${JSON.stringify(text)}); el.dispatchEvent(new Event('input', { bubbles: true })); return true; })()`
    );
    if (!ok) throw new Error(`no element: ${tag}[${index}]`);
  }

  async bodyText() {
    return this.evaluate(`document.body.innerText || ''`);
  }

  async waitFor(fn, { timeoutMs = 30000, pollMs = 500, what = 'condition' } = {}) {
    const start = Date.now();
    for (;;) {
      const value = await fn();
      if (value) return value;
      if (Date.now() - start > timeoutMs) throw new Error(`timeout waiting for ${what}`);
      await new Promise((r) => setTimeout(r, pollMs));
    }
  }

  async waitForText(text, opts) {
    return this.waitFor(async () => (await this.bodyText()).includes(text), { ...opts, what: `text ${JSON.stringify(text)}` });
  }

  async waitForSelector(selector, opts) {
    return this.waitFor(
      async () => this.evaluate(`document.querySelector(${JSON.stringify(selector)}) ? true : false`),
      { ...opts, what: `selector ${selector}` }
    );
  }

  close() {
    this.ws.close();
  }
}

async function connect(urlMatcher) {
  const targets = await cdpGet('/json/list');
  const page = targets.find((t) => t.type === 'page' && (!urlMatcher || (t.url || '').includes(urlMatcher)));
  if (!page) throw new Error(`no page target${urlMatcher ? ` matching ${urlMatcher}` : ''} (saw ${targets.length} targets)`);
  const ws = new WebSocket(page.webSocketDebuggerUrl, { maxPayload: 256 * 1024 * 1024 });
  await new Promise((resolve, reject) => {
    ws.onopen = resolve;
    ws.onerror = (e) => reject(new Error(`ws open failed: ${e.message || 'unknown'}`));
  });
  const session = new Session(ws);
  await session.send({ method: 'Page.enable', params: {} });
  await session.send({ method: 'Runtime.enable', params: {} });
  await session.send({ method: 'Log.enable', params: {} });
  return session;
}

module.exports = {
  connect,
  Session,
  buildEvalPayload,
  buildScreenshotPayload,
  buildMousePayload,
  buildKeyPayload,
  centerOfRect,
  extractValue,
};
