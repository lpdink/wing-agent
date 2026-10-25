import { fileURLToPath } from 'node:url';

import react from '@vitejs/plugin-react';
import { defineConfig } from 'vitest/config';

/**
 * Two test projects, matching the two runtimes we ship into:
 *
 * - `node` — host / core / shared / layer-guard / build-artifact logic. `vscode` is
 *   aliased to `tests/mocks/vscode.ts`, which is what makes host code
 *   headless-testable. `tests/artifact` builds the webview with the real Vite
 *   config and runs the result in a DOM without Node globals.
 * - `webview` — React components under jsdom, plus the patch reducer under node
 *   (the reducer has no DOM dependency; keep those files in `tests/webview/`
 *   only if they need the DOM).
 *
 * `pnpm test` runs both, so the layer guard is part of every gate (make test, CI).
 */
export default defineConfig({
  test: {
    // vitest 默认 5s。这套测试与 python / probe / rust 三组同时跑，满载时连 jsdom
    // 环境的创建都能被拖到秒级（实测单文件超时）——超时值要按「最差并发」定，
    // 不按空载定（空载这组只有几秒）。
    testTimeout: 15000,
    projects: [
      {
        extends: true,
        test: {
          name: 'node',
          environment: 'node',
          include: ['tests/{core,host,shared,layers,state,artifact}/**/*.test.ts'],
          alias: {
            vscode: fileURLToPath(new URL('tests/mocks/vscode.ts', import.meta.url)),
          },
        },
      },
      {
        extends: true,
        plugins: [react()],
        test: {
          name: 'webview',
          environment: 'jsdom',
          // `vmThreads` 按 worker 建一次 jsdom，而不是每个测试文件建一次：50 个文件里
          // 有 17 个吃 jsdom，环境创建本来占掉近四成追踪时间（整组 4.4s → 2.4s）。
          // 逐文件的模块隔离仍然保留（这是 `isolate: false` 做不到的）。
          // 只给 webview 工程：`node` 工程里的 artifact 套件要在真实的 Vite 构建管线里
          // 跑产物、launcher 套件要改进程 PATH——两者在 vm 上下文里都不成立。
          pool: 'vmThreads',
          setupFiles: ['tests/setup/webview.ts'],
          include: ['tests/webview/**/*.test.ts', 'tests/webview/**/*.test.tsx'],
          css: { modules: { classNameStrategy: 'non-scoped' } },
        },
      },
    ],
  },
});
