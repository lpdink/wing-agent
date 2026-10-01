# Web 与桌面壳（apps/web · apps/desktop）

> 本文档当前覆盖 **桌面壳 `apps/desktop`**（`wing-app` 计划的 step 05 交付）。
> `apps/web`（移动友好 Web 客户端）与两者的共享层章节由后续步骤（07–11）补充；
> 公共 TS 包见 [vscode-extension.md](vscode-extension.md) §9.3。

## 1. 形态与职责

`@wing-agent/desktop` 是 Electron 薄壳：**只**做「窗口 + 网关发现/拉起 + 配置 + 证书策略 +
`wing-app://` 静态协议 + preload 桥」，界面完全来自 web 构建（step 10 把 `apps/web` 的产物放进
`renderer/`）。它不打包 Python 运行时，靠本机已装的 `wing` CLI。

```
apps/desktop/
├── src/main.ts           唯一的值导入 electron 的入口（其余模块只 `import type`）
├── src/preload.ts        contextBridge → window.wingDesktop（契约见 src/bridge.ts）
├── src/config.ts         <userData>/config.json 读写（原子写 + 0600）
├── src/certificate.ts    忽略自签名证书的判定（纯函数）
├── src/gateway/launcher.ts  probe → `wing start` 一次 → 轮询（移植自 vscode 扩展）
├── src/web-document.ts   serveWebDocument：wing-app:// 的静态服务
├── src/menu.ts           最小菜单模板
├── renderer/             prod 静态根（当前为占位页；step 10 换成 web 构建）
└── scripts/{dev,smoke-app,after-pack}.mjs
```

层门禁：**electron 的值导入只允许 `src/main.ts` / `src/preload.ts`**，由 `eslint.config.mjs` 与
`tests/layers.test.ts`（解析真实 import 图，权威门禁）双重强制；其余模块都是普通 Node，`vitest` 直接跑。

## 2. 命令

| 命令 | 说明 |
|---|---|
| `pnpm --filter @wing-agent/desktop build` | esbuild 打 `src/main.ts`→`dist/main.js`（ESM）、`src/preload.ts`→`dist/preload.cjs`（CJS，`sandbox: true` 下 preload 必须是 CJS） |
| `pnpm --filter @wing-agent/desktop dev` | 构建 + 起 Electron，加载 `WING_APP_URL`（默认 `http://localhost:5173`，即 `apps/web` 的 vite dev server）。附加参数透传：`pnpm dev -- --smoke` 可无窗口自检 |
| `pnpm --filter @wing-agent/desktop smoke` | `electron . --smoke`（dev 形态自检） |
| `pnpm --filter @wing-agent/desktop dist:dir` | `electron-builder --dir` → `release/mac-arm64/Wing.app` |
| `pnpm --filter @wing-agent/desktop dist:mac` | dmg + zip → `release/Wing-<version>-arm64.dmg` / `-arm64-mac.zip` |
| `pnpm --filter @wing-agent/desktop smoke:app` | 对打包产物跑 `--smoke`（临时 `--user-data-dir`，不动真实配置） |

`--smoke`：初始化（读配置 → 装证书策略 → 注册协议 → 装菜单/IPC）后**不开窗口**，向 stdout 打一行 JSON
报告（含 `rendererDocument`，即 `net.fetch('wing-app://app/')` 的真实结果），退出码 `0` = 初始化全绿、
`1` = 失败（含协议取不到文档）。它不拉网关、不打单实例锁，可安全地在有真实实例运行时执行。

## 3. 配置（主进程持有）

`<userData>/config.json`，macOS 上 `<userData>` = `~/Library/Application Support/Wing`（`productName` 决定）。
键：

| 键 | 默认 | 说明 |
|---|---|---|
| `gatewayBaseUrl` | `http://127.0.0.1:32523` | 网关 origin（http/https，可带路径前缀，无尾斜杠）；对齐 `default_config.py` 的默认端口 |
| `apiKey` | `null` | `Authorization: Bearer …`；空/空白 → `null`（鉴权未开） |
| `ignoreCertErrors` | `false` | 自签名证书开关，见 §4 |
| `certificateWhitelist` | `[]` | `host:port[]`；**没有显式端口的条目会被丢弃**（避免退化成 host 级放行） |
| `wingPath` | `null` | 显式 `wing` 可执行文件；`null` = 常见路径 → `PATH` 三级发现 |
| `autoStart` | `true` | 网关不在时 `wing start` 一次（**只对本地地址**生效） |

读取永不抛：JSON 坏 / 字段类型不对 → 回默认值并把原因记进 `issues`（`--smoke` 报告里可见）。
写入是 `tmp + rename` 原子写，权限 `0600`（内含 API key）。renderer 通过 IPC 读写（`wing:settings-read` /
`wing:settings-write`），不直接碰文件。

## 4. 忽略自签名证书

放行条件（**两个都要满足**）：`ignoreCertErrors === true` **且**目标在白名单里。

两处钩子，语义随 Electron 给出的信息走：

- `session.setCertificateVerifyProc` → `allowsIgnoringCertificateHost(hostname, policy)`：**Electron 只暴露
  `hostname`**（实测 Electron 44 的 request 字段：`hostname / certificate / validatedCertificate /
  isIssuedByKnownRoot / verificationResult / errorCode`），所以这一层是 host 级匹配，`0` = 通过、`-3` = 交回
  Chromium 默认校验（自签被拒）。
- `app.on('certificate-error')` → `allowsIgnoringCertificate(url, policy)`：有完整 URL，**保持 `host:port`
  精确匹配**（端口不同一律拒）。

白名单条目形态 `host:port`（可带 scheme/路径，会被剥掉；支持 `[::1]:8443`）；空列表 + 开关打开 = 什么都不放行，
启动时会 `console.warn`。**不自动把网关地址塞进白名单**：开启开关时由设置面板写入（`certificateTargetFor(url)`
导出给 UI 用）。

## 5. `wing-app://` 与 CORS 契约

prod 页面地址是 `wing-app://app/`（**不用 `file://`**：那会让页面变成 opaque origin，`fetch` 带
`Origin: null`、拿不到安全上下文）。协议以 `standard + secure + supportFetchAPI + corsEnabled + stream +
codeCache` 注册（必须在 `app.ready` 之前），`serveWebDocument` 负责：只 GET/HEAD、路径必须落在 `renderer/`
内（越界 403）、无扩展名且文件不存在的路径回退 `index.html`（SPA）、`/assets/<name>-<hash>.<ext>` 标
`immutable`、其余 `no-cache`。

**因此网关的 CORS 允许列表必须包含 `wing-app://app` 与 dev 的 `http://localhost:5173`**，否则 HTTP 端点
（建会话/列表）会跨源失败，而 WS 不受影响——症状容易被误诊为「连上了但建不了会话」。

## 6. 打包（macOS，未签名）

`electron-builder.yml`：`appId: com.wing-agent.app`（与 7 月分支一致，决定 userData 目录）、
`productName: Wing`、`asar: true`、`files: [dist/**, renderer/**, package.json]`、`identity: null`、
`notarize: false`、`publish` 不配置。

两个必须知道的点：

1. **`afterPack` 钩子做 ad-hoc 重签名**（`scripts/after-pack.mjs`）：electron-builder 改写 bundle 后 Electron
   自带的 ad-hoc 签名失效（`codesign --verify` → "code has no resources…"，`Identifier=Electron`），本地能跑，
   但带 quarantine 就会被判「已损坏」。钩子对完成态 bundle 跑 `codesign --force --deep --sign -`，之后
   `codesign --verify` 干净、`Identifier=com.wing-agent.app`。
2. **`@electron/get` 需要 override**（根 `pnpm-workspace.yaml`）：`app-builder-lib@26.15.3` 声明 `^3.0.0`
   却调用 v5 的 `ElectronDownloadCacheMode`，不 override 时打包在写 `.app` 之前就崩。

产物未签名/未公证：经浏览器下载会被打 quarantine，首次打开需「右键 → 打开」或用
`xattr -dr com.apple.quarantine Wing.app`。打包不进 CI（无签名、无 GUI），由开发机产出。

## 7. 边界（留给后续步骤）

- 窗口 UI、菜单完整集、托盘、多窗口、自动更新、代码签名/公证：**不做**（proposal Out of Scope）。
- GUI 行为（窗口渲染、真实 web 构建、自签网关在 renderer 里的开/关两态、`wss://`）留 step 10 / 集成期验收；
  本步用纯函数单测 + `--smoke` 把协议与管道钉住。
