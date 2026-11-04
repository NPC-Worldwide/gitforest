'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const {
  Session,
  buildEvalPayload,
  buildScreenshotPayload,
  buildMousePayload,
  buildKeyPayload,
  centerOfRect,
  extractValue,
} = require('./driver');

test('eval payload enables value return', () => {
  const p = buildEvalPayload(7, '1+1', true);
  assert.equal(p.id, 7);
  assert.equal(p.method, 'Runtime.evaluate');
  assert.equal(p.params.expression, '1+1');
  assert.equal(p.params.awaitPromise, true);
  assert.equal(p.params.returnByValue, true);
});

test('screenshot payload requests png', () => {
  const p = buildScreenshotPayload(3);
  assert.equal(p.method, 'Page.captureScreenshot');
  assert.equal(p.params.format, 'png');
});

test('mouse payload clicks left once on press', () => {
  const down = buildMousePayload(1, 'mousePressed', 10, 20);
  assert.equal(down.params.type, 'mousePressed');
  assert.equal(down.params.button, 'left');
  assert.equal(down.params.clickCount, 1);
  assert.deepEqual([down.params.x, down.params.y], [10, 20]);
  const up = buildMousePayload(2, 'mouseReleased', 10, 20);
  assert.equal(up.params.clickCount, 0);
});

test('key payload inserts text or enters', () => {
  const ins = buildKeyPayload(1, 'insertText', 'hi');
  assert.equal(ins.method, 'Input.insertText');
  assert.equal(ins.params.text, 'hi');
  const enter = buildKeyPayload(2, 'rawKeyDown');
  assert.equal(enter.params.windowsVirtualKeyCode, 13);
});

test('center rounds to whole pixels', () => {
  assert.deepEqual(centerOfRect({ x: 1.2, y: 2.6, width: 9, height: 7 }), { x: 6, y: 6 });
});

test('session routes responses to pending sends', async () => {
  const sent = [];
  const ws = { send: (s) => sent.push(JSON.parse(s)), close: () => {} };
  const session = new Session(ws);
  const pending = session.send({ method: 'Runtime.evaluate', params: {} });
  assert.equal(sent.length, 1);
  assert.equal(sent[0].method, 'Runtime.evaluate');
  ws.onmessage({ data: JSON.stringify({ id: sent[0].id, result: { result: { value: 'ok' } } }) });
  const msg = await pending;
  assert.equal(msg.result.result.value, 'ok');
});

test('session records console errors from events', () => {
  const ws = { send: () => {}, close: () => {} };
  const session = new Session(ws);
  ws.onmessage({ data: JSON.stringify({ method: 'Runtime.exceptionThrown', params: { exceptionDetails: { text: 'kaboom' } } }) });
  ws.onmessage({ data: JSON.stringify({ method: 'Log.entryAdded', params: { entry: { level: 'error', text: 'bad' } } }) });
  ws.onmessage({ data: JSON.stringify({ method: 'Log.entryAdded', params: { entry: { level: 'info', text: 'fine' } } }) });
  assert.equal(session.consoleErrors.length, 2);
});

test('extractValue returns value and rejects errors', () => {
  assert.equal(extractValue({ id: 1, result: { result: { value: 42 } } }), 42);
  assert.throws(() => extractValue({ id: 1, error: { message: 'nope' } }), /nope/);
  assert.throws(
    () => extractValue({ id: 1, result: { result: { subtype: 'error', description: 'boom' } } }),
    /boom/
  );
  assert.throws(() => extractValue({ id: 1, result: {} }), /bad result/);
});
