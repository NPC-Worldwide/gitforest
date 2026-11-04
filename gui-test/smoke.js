'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { connect } = require('./driver');

const OUT = process.env.GUI_OUT || path.join(__dirname, 'out');
const COMMIT_SHA = process.env.GUI_COMMIT_SHA || 'HEAD';

const PAGES = [
  { label: 'Repos', marker: 'Hosted repos', shot: 'repos.png' },
  { label: 'Network', marker: 'Network', shot: 'network.png' },
  { label: 'Analytics', marker: 'Analytics', shot: 'analytics.png' },
  { label: 'Compute', marker: 'Compute marketplace', shot: 'compute.png' },
  { label: 'Profile', marker: 'Profile & identity', shot: 'profile.png' },
  { label: 'Settings', marker: 'Machine & daemon', shot: 'settings.png' },
];

async function main() {
  fs.mkdirSync(OUT, { recursive: true });
  const session = await connect();
  const failures = [];
  try {
    await session.waitForSelector('nav', { timeoutMs: 90000, what: 'app sidebar (daemon up + onboarded)' });
    const text = await session.bodyText();
    for (const label of PAGES.map((p) => p.label)) {
      if (!text.includes(label)) failures.push(`nav label missing: ${label}`);
    }
    console.log('sidebar ready');

    for (const page of PAGES) {
      await session.clickText(page.label);
      await session.waitForText(page.marker, { timeoutMs: 15000 });
      await session.screenshot(path.join(OUT, page.shot));
      console.log(`page ok: ${page.label}`);
    }

    await session.clickText('Profile');
    await session.waitForText('Profile & identity', { timeoutMs: 15000 });
    await session.fill('input[placeholder="How others see you on the network"]', 'GUI Smoke');
    await session.clickText('Publish');
    await session.waitForText('Display name saved', { timeoutMs: 15000 });
    await session.screenshot(path.join(OUT, 'profile-saved.png'));
    console.log('functional ok: profile publish');

    await session.clickText('Compute');
    await session.waitForText('Compute marketplace', { timeoutMs: 15000 });
    await session.waitForText('Submit a job', { timeoutMs: 15000 });
    try {
      await session.waitFor(
        async () => !(await session.bodyText()).includes('Add a repo first'),
        { timeoutMs: 15000, what: 'fixture repo in compute form' }
      );
    } catch {
      failures.push('compute form disabled: fixture repo missing (run.sh must seed one)');
    }
    if (!(await session.bodyText()).includes('Add a repo first')) {
      await session.fillNth('input', 0, COMMIT_SHA);
      await session.fillNth('input', 1, '100');
      await session.clickText('Submit job');
      await session.waitForText('Job submitted:', { timeoutMs: 30000 });
      await session.screenshot(path.join(OUT, 'compute-submitted.png'));
      console.log('functional ok: compute submit');
    }

    const errors = session.consoleErrors.filter(
      (e) => !e.includes('favicon') && !e.toLowerCase().includes('net::')
    );
    if (errors.length > 0) {
      failures.push(`${errors.length} console errors:\n` + errors.slice(0, 10).join('\n'));
    }
  } finally {
    session.close();
  }
  if (failures.length > 0) {
    console.error('GUI SMOKE FAIL:\n' + failures.join('\n'));
    process.exit(1);
  }
  console.log(`GUI SMOKE PASS (${OUT})`);
}

main().catch((e) => {
  console.error(`GUI SMOKE FAIL: ${e.message}`);
  process.exit(1);
});
