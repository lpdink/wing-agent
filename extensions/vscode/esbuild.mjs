// Extension-host bundler.
//
// The extension host is a CommonJS Node environment: bundle everything except
// `vscode` (provided by the editor at runtime) into `out/extension.js`, which is
// what `package.json#main` points at.
//
// `--watch` keeps an incremental build alive for F5 sessions; `--dev` skips the
// production `NODE_ENV` define so React-free host code behaves the same but
// stack traces stay readable either way (we never minify: a readable extension
// host log is worth more than the few KB).

import { mkdirSync } from 'node:fs';
import process from 'node:process';

import esbuild from 'esbuild';

const watch = process.argv.includes('--watch');
const production = !process.argv.includes('--dev');

/** @type {import('esbuild').BuildOptions} */
const options = {
  entryPoints: ['src/host/extension.ts'],
  outfile: 'out/extension.js',
  bundle: true,
  format: 'cjs',
  platform: 'node',
  // VS Code 1.100 ships Electron 34 / Node 20.19; `node20` keeps us honest about
  // the runtime APIs the host bundle may use.
  target: 'node20',
  external: ['vscode'],
  sourcemap: true,
  minify: false,
  logLevel: 'info',
  define: {
    'process.env.NODE_ENV': production ? '"production"' : '"development"',
  },
};

mkdirSync('out', { recursive: true });

if (watch) {
  const context = await esbuild.context(options);
  await context.watch();
  console.log('[esbuild] watching src/host/extension.ts → out/extension.js');
} else {
  await esbuild.build(options);
}
