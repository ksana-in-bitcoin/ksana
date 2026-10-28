/**
 * Browser check of the wallet demo page (M6, D-055): opens `examples/wallet-demo` in Chromium (playwright-core), injects a mock
 * Unisat wallet (the `mock` parameter of the page URL), clicks "Connect Unisat" → "Estimate fee" → "Sign and submit" in turn to
 * submit a KSA transfer over path B, and waits until the page shows the operation as fast_confirmed. The page and the node RPC
 * have different origins, so the node must enable CORS (`--rpc-cors`).
 * Called by `tests/integration/tests/m6_regtest.rs`:
 *
 *   node dist/test/e2e/demo-page.js '<JSON input>'
 *
 * Input: `{ rpc, secret_key, to, amount (decimal string), browser?, screenshot? }` (the wallet is a Taproot address, D-092); the
 * account must have enough KSA to pay the amount and the fee; `browser` is the Chromium executable (defaults to the one installed
 * by `npx playwright-core install --only-shell chromium`), `screenshot` is the path to save a full-page screenshot to on
 * completion.
 * Output (the last line of stdout, JSON): `{ op_id, fee, account }`, where `fee` is the fee the page filled in from
 * `ksana_estimateFee` and used (KSA base units, decimal string).
 */

import { readFile } from 'node:fs/promises';
import { type AddressInfo } from 'node:net';
import { createServer } from 'node:http';
import { dirname, extname, join, normalize, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

import { type Page, chromium } from 'playwright-core';

interface Input {
  rpc: string;
  secret_key: string;
  to: string;
  amount: string;
  browser?: string;
  screenshot?: string;
}

/** `sdk/ts/examples/wallet-demo` (this file is compiled into `sdk/ts/dist/test/e2e/`). */
const SITE = join(dirname(fileURLToPath(import.meta.url)), '..', '..', '..', 'examples', 'wallet-demo');

const TYPES: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.map': 'application/json',
  '.css': 'text/css; charset=utf-8',
};

/** Local static file server (the demo page and the RPC have different origins). */
async function serve(dir: string): Promise<{ url: string; close(): Promise<void> }> {
  const server = createServer((req, res) => {
    const path = new URL(req.url ?? '/', 'http://localhost').pathname;
    const file = normalize(join(dir, path === '/' ? 'index.html' : decodeURIComponent(path)));
    if (!file.startsWith(dir + sep)) {
      res.writeHead(403).end();
      return;
    }
    readFile(file).then(
      (body) => res.writeHead(200, { 'content-type': TYPES[extname(file)] ?? 'application/octet-stream' }).end(body),
      () => res.writeHead(404).end(),
    );
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const { port } = server.address() as AddressInfo;
  return {
    url: `http://127.0.0.1:${port}`,
    close: () => new Promise((resolve) => server.close(() => resolve())),
  };
}

async function text(page: Page, selector: string): Promise<string> {
  return ((await page.textContent(selector)) ?? '').trim();
}

async function main(): Promise<void> {
  const raw = process.argv[2];
  if (raw === undefined) throw new Error('Usage: node dist/test/e2e/demo-page.js <JSON input>');
  const input = JSON.parse(raw) as Input;
  const site = await serve(SITE);
  const browser = await chromium.launch({
    headless: true,
    ...(input.browser === undefined ? {} : { executablePath: input.browser }),
  });
  const errors: string[] = [];
  try {
    const page = await browser.newPage();
    page.on('pageerror', (error) => errors.push(error.message));
    const query = new URLSearchParams({
      rpc: input.rpc,
      chain: 'devnet',
      mock: 'p2tr',
      key: input.secret_key,
      network: 'regtest',
    });
    await page.goto(`${site.url}/index.html?${query.toString()}`);
    try {
      await page.click('#connect-unisat');
      await page.waitForFunction(() => document.querySelector('#nonce')?.textContent !== '—');
      const account = await text(page, '#wallet-account');
      await page.fill('#to', input.to);
      await page.fill('#amount', input.amount);
      await page.click('#estimate');
      await page.waitForFunction(() => (document.querySelector('#fee') as HTMLInputElement).value !== '');
      const fee = await page.inputValue('#fee');
      await page.click('#submit');
      // failed: included but failed at execution (the fee is still charged); no need to wait for the timeout
      await page.waitForFunction(
        () => ['fast_confirmed', 'failed'].includes(document.querySelector('#op-status')?.textContent ?? ''),
        null,
        { timeout: 90_000 },
      );
      const status = await text(page, '#op-status');
      if (status !== 'fast_confirmed') throw new Error(`operation status is ${status}`);
      if (errors.length > 0) throw new Error(`page errors: ${errors.join('; ')}`);
      if (input.screenshot !== undefined) await page.screenshot({ path: input.screenshot, fullPage: true });
      console.log(JSON.stringify({ op_id: await text(page, '#op-id'), fee, account }));
    } catch (error) {
      console.error(`Page log:\n${await text(page, '#log')}`);
      throw error;
    }
  } finally {
    await browser.close();
    await site.close();
  }
}

main().catch((error: unknown) => {
  console.error(error);
  process.exitCode = 1;
});
