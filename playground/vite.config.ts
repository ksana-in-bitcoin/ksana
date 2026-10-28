import { execSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// During development the node, the faucet and the compile service can be proxied to same-origin paths:
//   KSANA_RPC=http://127.0.0.1:18545 KSANA_COMPILE=http://127.0.0.1:8090 npm run dev
//   then open http://localhost:5174/?rpc=/rpc&compile=/compile
// (KSANA_FAUCET likewise for /faucet). Without a compile service the precompiled examples still deploy; editing needs one.
function proxyTo(target: string | undefined, prefix: string) {
  if (!target) return undefined;
  return { target, changeOrigin: true, rewrite: (path: string) => path.replace(new RegExp(`^/${prefix}`), '') };
}

// The git commit the page was built from, shown in its footer: KSANA_COMMIT when given, then CI's GITHUB_SHA or Workers Builds'
// WORKERS_CI_COMMIT_SHA, then the local git
function buildCommit(): string {
  const given = process.env.KSANA_COMMIT || process.env.GITHUB_SHA || process.env.WORKERS_CI_COMMIT_SHA;
  if (given) return given.trim().slice(0, 9);
  try {
    return execSync('git rev-parse --short=9 HEAD', { stdio: ['ignore', 'pipe', 'ignore'] }).toString().trim();
  } catch {
    return '';
  }
}

// The hosted copy's files (D-119), in dist/ only for `vite build --mode hosted` (npm run build:hosted, the Cloudflare Worker's
// build, wrangler.jsonc): deployment.json, the signet's endpoints the page calls across origins, and _headers, the headers
// Cloudflare serves the files with. Other builds and the development server have neither, so they take their endpoints
// from the address bar (src/chain/deployment.ts)
const HOSTED_FILES = ['deployment.json', '_headers'];

export default defineConfig(({ mode }) => {
  const commit = buildCommit();
  const rpc = proxyTo(process.env.KSANA_RPC, 'rpc');
  const faucet = proxyTo(process.env.KSANA_FAUCET, 'faucet');
  const compile = proxyTo(process.env.KSANA_COMPILE, 'compile');
  return {
    base: './',
    plugins: [
      react(),
      // version.json next to index.html: the commit the page was built from
      { name: 'ksana-version', generateBundle() { this.emitFile({ type: 'asset', fileName: 'version.json', source: `${JSON.stringify({ commit })}\n` }); } },
      {
        name: 'ksana-hosted',
        generateBundle() {
          if (mode !== 'hosted') return;
          for (const fileName of HOSTED_FILES) {
            this.emitFile({ type: 'asset', fileName, source: readFileSync(new URL(`./hosted/${fileName}`, import.meta.url), 'utf8') });
          }
        },
      },
    ],
    define: { __KSANA_COMMIT__: JSON.stringify(commit) },
    server: {
      port: 5174,
      // the example contracts' sources are read from contracts/examples/ (src/examples/index.ts)
      fs: { allow: ['..'] },
      proxy: {
        ...(rpc ? { '/rpc': rpc } : {}),
        ...(faucet ? { '/faucet': faucet } : {}),
        ...(compile ? { '/compile': compile } : {}),
      },
    },
    build: {
      outDir: 'dist',
      emptyOutDir: true,
      target: 'es2022',
      cssCodeSplit: false,
      modulePreload: { polyfill: false },
    },
    test: {
      environment: 'node',
      include: ['src/**/*.test.ts'],
    },
  };
});
