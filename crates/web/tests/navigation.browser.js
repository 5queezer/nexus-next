// Run against a synthetic loopback instance with Playwright CLI:
// playwright-cli open http://127.0.0.1:18191
// playwright-cli run-code --filename crates/web/tests/navigation.browser.js
async (page) => {
  const origin = new URL(page.url()).origin;
  if (!['127.0.0.1', 'localhost', '[::1]'].includes(new URL(origin).hostname)) {
    throw new Error('Use a synthetic loopback instance, never owner data');
  }
  const context = page.context();
  await context.addInitScript(() => {
    window.__navigation = { transition: false, frames: [] };
    window.addEventListener('pagereveal', event => {
      window.__navigation.transition = !!event.viewTransition;
      let remaining = 12;
      const sample = () => {
        if (!document.body || !document.querySelector('.top')) return;
        const body = getComputedStyle(document.body);
        window.__navigation.frames.push({
          font: body.fontFamily,
          background: body.backgroundColor,
          headerHeight: document.querySelector('.top').getBoundingClientRect().height,
        });
        if (--remaining) requestAnimationFrame(sample);
      };
      requestAnimationFrame(sample);
    });
  });
  const cdp = await context.newCDPSession(page);
  await cdp.send('Network.enable');
  const results = [];
  for (const colorScheme of ['light', 'dark']) {
    for (const reducedMotion of ['no-preference', 'reduce']) {
      for (const cold of [false, true]) {
        await page.emulateMedia({ colorScheme, reducedMotion });
        await cdp.send('Network.setCacheDisabled', { cacheDisabled: cold });
        await cdp.send('Network.setBypassServiceWorker', { bypass: cold });
        await cdp.send('Network.emulateNetworkConditions', {
          offline: false, latency: cold ? 150 : 0,
          downloadThroughput: cold ? 125000 : -1, uploadThroughput: cold ? 125000 : -1,
        });
        await page.goto(origin);
        for (const [name, path] of [['CV', '/cv'], ['Follow-ups', '/queue'], ['Pipeline', '/']]) {
          await page.getByRole('navigation', { name: 'Main', exact: true })
            .getByRole('link', { name, exact: true }).click();
          if (new URL(page.url()).pathname !== path) throw new Error(`Navigation failed: ${path}`);
          await page.waitForFunction(() => window.__navigation.frames.length === 12);
          const probe = await page.evaluate(() => window.__navigation);
          if (probe.transition) throw new Error(`Page transition overlays destination page: ${path}`);
          const background = colorScheme === 'light' ? 'rgb(246, 245, 241)' : 'rgb(20, 22, 26)';
          if (probe.frames.some(f => !f.font.startsWith('system-ui') || f.background !== background || f.headerHeight !== 53)) {
            throw new Error(`Unstyled navigation frame: ${path}`);
          }
          const blocking = await page.locator('head link[rel="stylesheet"]').evaluate(link =>
            !!link.sheet && !link.disabled && !link.hasAttribute('media') && !link.hasAttribute('onload'));
          if (!blocking) throw new Error(`Stylesheet no longer blocks rendering: ${path}`);
          results.push({ colorScheme, reducedMotion, cold, path, sampledFrames: probe.frames.length });
        }
      }
    }
  }
  await cdp.detach();
  return results;
}
