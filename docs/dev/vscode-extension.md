# VSCode 扩展（`extensions/vscode/`）

> 定位：wing 的**第四个前端形态**（前三个：TUI、stdio、编排 CLI）。它与 TUI 消费同一套网关
> 控制面——HTTP 管生命周期与查询，WebSocket 只承载 ReAct 事件流——不同的是它跑在 VS Code 里：
> 扩展宿主进程持有连接与会话状态，侧边栏 Webview 只做渲染。
>
> 本页面向**接手者**：读完应该能改代码、能定位时序/归约类 bug、知道哪些约束不许碰。
> 快速上手（安装 / F5 / 命令表）在 [`extensions/vscode/README.md`](../../extensions/vscode/README.md)；
> 用户逐项验收在任务目录的 `acceptance-checklist.md`（不在仓库里）。
>
> 事实来源优先级：**代码 > 本页 > README**。发现不一致以代码为准。

## 1. 边界（先读这个）

**做**：多 Tab 会话（新建 / 切换 / 关闭 / resume / fork）、完整对话渲染（streaming / thinking /
工具调用 / diff / todo / Ask / 审批）、控制面（模型、think/effort、yolo、中断、compact、rewind、
fork、prompt 命令）、连接自愈（探活 + 一次自动拉起 + 断线重连重订阅重放）。

**不做**（Out of Scope，别顺手加）：

- Goal 编排（TUI/编排 CLI 有，本扩展不做）；
- 图片/文件上传、附件、多模态、`@file` 引用（后端未支持，视觉上也不出现）；
- VS Code 原生 Chat Participant API（我们做自己的 Webview 视图）；
- 远程场景（SSH / WSL / Dev Container）、多机网关；
- Electron / Web 前端本体（只保留 `src/core` 这个接缝）；
- i18n（UI 文案英文，与 TUI 一致）。

## 2. 分层与数据流

### 2.1 四层 + 两个辅助层

```
extensions/vscode/
├── src/shared/    两侧契约：类型、常量、纯函数（命令匹配、cell 判别联合……）。
│                  不依赖任何东西（无 npm、无 node、无 vscode、无 DOM）。
├── src/core/      网关能力层：协议镜像（显式解码）、WS 连接 + 重连监督、HTTP 客户端、
│                  分片重组、退避、错误模型、URL 组装。Electron 直接搬这一层。
├── src/host/      扩展宿主：WingHost（连接生命周期）+ SessionManager（多 Tab 编排 / 控制面 /
│                  归约宿主）+ reducer/model/derive（事件 → UI 模型）+ bridge + 视图 + 命令 + 设置。
├── src/webview/   React 渲染层：cells、markdown 增量渲染、shiki、外壳（Tab 栏 / composer /
│                  状态区 / 面板）、bridge 客户端、zustand store。纯渲染，状态由宿主驱动。
├── src/testing/   夹具 + scripted host（fixtures.ts / mockBridge.ts）。
│                  只能被 tests/ 与 preview/ 引用（层守门强制）。
└── preview/       无 VS Code 的预览 harness（Vite，端口 5199）。
```

依赖矩阵（`tests/layers/layers.test.ts` 的 `RULES`，权威口径）：

| 层 | 可 import 的自家层 | npm | node builtins | `vscode` |
|---|---|---|---|---|
| `shared` | 同级 | ✗ | ✗ | ✗ |
| `core` | `shared` | ✓ | ✓ | ✗ |
| `host` | `shared`、`core` | ✓ | ✓ | ✓（只有它可以） |
| `webview` | `shared` | ✓ | ✗ | ✗ |
| `testing` | `shared` | ✗ | ✗ | ✗ |

`preview/` 只允许 import `preview` 自己、`shared`、`testing`、`webview`。

### 2.2 数据流（一张图）

```
        VS Code 扩展宿主进程（Node）                     │  Webview（iframe / DOM）
                                                         │
  wing-gateway                                           │
      ▲  HTTP（生命周期 / 查询 / 变更）                    │
      │  WS  /ws（ReAct 事件流 + 上行帧）                  │
      │                                                  │
  ┌───┴──────────────┐     桥协议（postMessage）           │
  │ src/core         │   hydrate / patch / state /        │
  │ GatewayConnection│   panels / tabs / ui          ┌────┴─────────────┐
  │ GatewayHttpClient│  ────────────────────────────► │ src/webview      │
  └───▲──────────────┘                                │ appStore         │
      │ 事件                                          │  applyCellPatches│
  ┌───┴──────────────┐   ready / resync / 用户意图     │  ├ cells 渲染     │
  │ src/host         │  ◄──────────────────────────── │  ├ markdown 增量  │
  │ WingHost         │                                │  └ 外壳 / 面板    │
  │ └ SessionManager │                                └──────────────────┘
  │    └ reducer ──► SessionRecord（cells/state/meta）
  └──────────────────┘
```

关键点：**WebSocket 只存在于扩展宿主**。Webview 永远不直接连网关（它的 CSP 里连
`connect-src` 都没有），一切数据经宿主归约后过桥。

### 2.3 层门禁为什么是「三套机制」

单靠一套都拦不住，这是被 01 的评审逐条确认过的结构：

1. **分 tsconfig 的 `lib`/`types`**（`tsconfig.node.json` 无 DOM、`tsconfig.webview.json` 无
   node/vscode）——写 `document`/`process` 直接编译错误。它管「用了什么全局」。
2. **ESLint zones**（`eslint.config.mjs` 的 `no-restricted-imports`）——`vscode` 出 `host`、
   `host → webview`、`webview → host/core`、产品代码引用 `src/testing` 都在编辑时就红。
   它管「常见越层 import」，但表达不了完整矩阵（静态/动态 import、re-export、`require`）。
3. **`tests/layers/layers.test.ts`**——解析每个源文件的**真实 import 图**（含动态
   `import()`、`require()`、`export ... from`、`import type`），逐条断言上表；还顺带检查
   `src/**` 里没有 `.js/.jsx`（避免"文件存在但 typecheck/lint 都看不见"）、webview CSS 里没有
   硬编码颜色。它是权威门禁，跑在 `pnpm test` 里。

所以「越层」会同时挂 `make check` 与 `make test` 两处——这是有意的冗余。

## 3. 冻结的裁定（改之前先想清楚）

这些是 03（宿主）与 05（外壳）并行开发出冲突后、由调度者写进 `interfaces.md` 并**双方强制遵守**
的裁定，落到本仓库的代码里。不是风格问题——每一条都对应一类曾经真实出现过的 bug。

### 3.1 宿主是 UI 模型的唯一权威；重放 == 直播

- cells 归约、状态派生、标题派生全部在宿主完成；webview 只做确定性的 op 应用
  （`src/webview/state/applyPatch.ts` 的 `applyCellPatches`），不猜、不推导、不解析半截 JSON
  （局部 JSON 解析在宿主 `src/host/session/partial-json.ts`）。
- 网关的 `sync_session` 重放与 live 事件走**同一条归约路径**（`reducer.ts` 的 `applySync` /
  `applyLive` 共享同一套模型变更），这是「重放路径 vs 活跃路径渲染不一致」一类 bug 的结构性
  解药（R1 返修记录里叫 S1「重放 ≠ 直播」，缺一个 ReAct separator 就会分叉）。
- 派生单元不重放：`sync_session` 只重放**事实**（Message 投影 + FACT_EVENTS），metrics 这类
  展示单元格直播时有、重放后没有。所以「重放后的 cells 数 ≤ 直播时」是正确行为，测试断言写
  「内容在、不重复」，不写「逐 cell 相等」。

### 3.2 单 WS 多订阅（一个 `client_id`）

一条 `GatewayConnection` 覆盖所有 Tab；每个 Tab 打开即
`POST /api/session/subscribe`（响应触发 `route_attach` + `sync_session` 重放），关闭即
`POST /api/session/unsubscribe`。**绝不**每个 Tab 一条连接：多连接会带来多份重放、事件归属
混乱、client 数膨胀。

- 事件的归属由事件自带的 `session_id` 决定，只归约到对应 Tab 的 `SessionRecord`，其它 Tab 的
  cells 完全不动（`multi-tab-isolation` smoke 场景钉死这一点）。
- 重连后：`onConnected` → `resubscribeAll()`（`manager.ts`），每个还开着的 Tab 重新 subscribe，
  重放回来不重复。
- **订阅时先 `route_attach` 再推重放**——重放与 live 的边界由网关这一性质保证，宿主侧不得凭空
  引入中间态（比如收到 subscribe 响应前就先进 live 事件）。订阅成功后宿主还会拉一次
  `GET /api/session/info` 刷新运行时状态（见 3.5 的 yolo 说明）。

### 3.3 overlay 的开合归宿主，catalog 只承载数据

`PanelsModel`（`src/shared/session.ts`）定稿：

```ts
interface PanelsModel {
  modelPicker:   ModelPickerModel | null;   // 非 null = 宿主打开的模型面板
  globalNotice:  GlobalNoticeModel | null;  // 宿主自己会过期的横幅
  commandCatalog: CommandCatalogModel | null; // 数据；null = 还没拉到，永远不是 overlay
  sessionPicker: SessionPickerModel | null;   // 非 null = 在屏上
  branchPicker:  BranchPickerModel | null;    // 非 null = 在屏上
}
```

- 「开」与「数据」必须**原子到达**（同一个 `panels` 消息）：不允许「先显示空面板再填数据」
  （空闪）或「显示上一份陈旧数据」。
- 用户输入裸 `/ss`、`/rewind`、`/fork`（以及 Tab 栏历史按钮）时，webview 发
  `runPromptCommand { name, argsText: '' }`——**不是**本地开面板；宿主收到裸命令后拉数据
  （`/api/session/list`、`/api/session/branches`）→ 填 picker → 随 `panels` 推下去。
- Esc / 背板 / 关闭按钮 = `closeOverlays`，宿主清空三个 picker（`globalNotice` 除外，它由宿主
  自己过期）。选择行后宿主除了执行动作还要清掉对应 picker（动作后不残留）。
- 键盘高亮、滚动位置这类纯展示态归 webview（默认选中 `current === true` 的行）；行数据一律由
  宿主填，webview 不得伪造行（面板未就绪时可回落展示本地 tabs，但数据不得编造）。

### 3.4 标题派生规则（与后端逐字符一致）

`src/host/session/derive.ts#deriveTitle`：

1. 显式名字（`metadata.session_name`）优先；
2. 否则首条用户消息**按 Unicode 码点直切片 100 个字符，无省略号**——这是后端
   `first_user_message`（`store/base.py` 的 `content[:100]`）与 `session_manager.py` 的
   auto-title 规则，两边必须逐字符一致，否则 Tab 上的标题会和 `/ss` 里的标题出现两种写法；
3. 否则 workspace basename；
4. 否则 `New session`。

刷新时机：首条用户消息入 cells 时即时重算（`SessionRecord.refreshTitle`）——**不依赖「切 Tab
才刷新」**。

### 3.5 runtime 状态只在「订阅后」补齐

`sync_session.agent` 不含 yolo；`session_state_changed` 只在变更时发。宿主在 subscribe 成功后
调 `GET /api/session/info` 合并 `yolo/thinking/reasoningEffort`（以及空值兜底的
`model`/`workdir`），只填空、不覆盖竞态中的用户选择。**这是检查点② bug（resume 后 yolo 显示为
关）的修复**，重连重订阅同样刷新；持续轮询不在范围内。

## 4. 时序

`SessionManager` 里所有结构性操作（open / close / resume / fork / resubscribe）过一个串行队列
（`session/queue.ts`），保证「create 没回来就按了关闭」这类交错不会产生半个会话。

### 4.1 激活

```
extension.activate
  → WingHost.start()
     → GET /api/health 探活（2s 超时）
        · 活着 → 什么都不做（绝不重启用户正在用的网关）
        · 死了且 wing.autoStart → 跑一次 `wing start`（20s 上限）→ 轮询探活
        · 取消 autoStart / 找不到 wing → 横幅 + 一次性错误提示
     → GatewayConnection.connect() → WS 连上（connected）
        · 首连失败：宿主自己的重试阶梯（1s·2ⁿ，封顶 30s；core 只监督"连上过之后"的连接）
        · unauthorized：停止一切重试，提示填 `wing.apiKey`（凭据不会自愈）
  → webview ready → 宿主回 tabs + hydrate
  → 没有任何 Tab 时自动建一个（workspace = 窗口第一个 folder；没有 folder → 明确报错，不建半会话）
```

`ensureRunningOnce` 的 autoStartAttempted 是一次性预算：`wing.reconnectGateway`（用户显式动作）
会重置它。

### 4.2 一轮对话（send）

```
webview: sendMessage { sessionId, text }
  → 宿主守卫：该 Tab 必须已 subscribe 且连接就绪（create→subscribe→send 的闸门；不满足时提示
    "Not sent — the gateway is not connected." 并把草稿留在输入框——绝不排队，TUI 对齐）
  → POST /api/session/send { message, request_id }
  → WS 事件（同一 session_id）：
      stream_delta / thinking_delta    → reducer 追加文本（append_text patch）
      tool_call_* / tool_result        → tool cell（partial JSON 在宿主解析成稳定行）
      diff / todo / ask / approval     → 对应 cell 类型
      turn_result / 状态变化            → state 更新（working → idle、metrics、usage）
  → 每个变更组成 patch（seq = 上一 seq + 1）→ postMessage
  → webview applyCellPatches：seq 不连续 / 找不到 cell / op 不匹配 → resync → 宿主 hydrate
```

中断 = `interrupt` 意图 → `POST /api/session/interrupt`；已生成内容保留，半截工具块由网关侧
提交语义剔除（与 TUI 行为一致）。

### 4.3 resume / fork / rewind

| 操作 | 链路 |
|---|---|
| 新建 Tab（`+` / `/new`） | `create` → `subscribe`（自动带上 agent 默认配置）→ 新 Tab active；旧 Tab 不受影响 |
| resume（`/ss` 选行、`/session <id>`、重开历史） | 已有 Tab → 只聚焦；未打开 → `resume` → `subscribe` → hydrate 重放（标题从 metadata 来） |
| fork（`/fork` 裸 → 面板 → 选点） | `fork { uuid }` → 新会话 id → **新 Tab** → `subscribe`；原会话不变；draft（所选消息文本）随 hydrate 回来 |
| rewind（`/rewind` 裸 → 面板 → 选点） | `rewind { uuid }` → 网关回 `sync_session`（replace 语义）→ 宿主整表替换（`replace_all` / hydrate）+ draft 回填 |

rewind 后 transcript「回退到该消息之前」+ 该消息文本回到输入框，是 06 联调时用户可见的行为。

### 4.4 关闭与切换

- 关闭 Tab：`unsubscribe` + 从 Tab 列表移除；此后再来的该 session 事件不会「复活」它
  （`closed` 标记挡住迟到的异步工作）。
- 切换 Tab 只是 UI 焦点：**不重订阅**（连接与订阅仍在），也不触发重放；每个 Tab 的模型一直在
  内存里。
- webview 重载（侧边栏被重建、容器切换）：`ready` → 全量 `hydrate`；`retainContextWhenHidden:
  true` 让正常切换不触发这一步（DOM 与草稿保留）。

### 4.5 断线重连

```
core 监督（连上过之后）：连接断开 → reconnecting（退避重试，client_id 置空）
  宿主：横幅「Gateway connection lost — reconnecting…」
  （注意：重订阅只发生在 connected 之后——重试窗口里 client_id 是空的，提前订阅只会空转）
→ connected：client_id 更新 → 宿主 onConnected()
  → resubscribeAll()：每个还开着的 Tab subscribe → 网关重推 sync（replace）
  → 每个会话 refreshRuntimeState()（/api/session/info）
```

smoke 的 `reconnect-resubscribe` 场景钉死：重连后重放**不重复**、继续能收新事件。

## 5. 事件归约管线（宿主侧）

```
WS 帧
 └─ core：单帧 ≤ 16 MiB（网关对 > 8 MiB 的载荷在 wire 出口用 `_chunk` 信封切分，
    客户端在读任务内合并还原 —— core/chunk.ts；应用层只见完整事件）
     └─ 事件解码（protocol/events.ts，显式解码器，不 `as` 硬塞）
         └─ SessionManager.handleEvent → 按 session_id 找 record
             └─ reducer.applyLive（或 applySync）
                 ├─ 变更 cells（user/assistant/thinking/tool/diff/todo/ask/metrics/system）
                 ├─ 变更 state（status/turn/meta/panels）
                 └─ 产出 journal
                     └─ 打成 bridge 消息：
                         hydrate  全量快照（ready / resync / 重开 Tab 时）
                         patch    有序 cell 变更（带 seq；唯一的高频消息）
                         state    除 cells 外的全量替换（不占 seq）
                         panels   overlay 数据
                         tabs     全局 Tab 列表 + active
                         ui       一次性动作（toast / focusComposer / scrollToBottom / closeOverlays）
```

- **patch 顺序规则**：并发工具调用靠 `insert_after`（锚定前一个 cell）保持到达顺序，而不是
  每次重发整段 transcript。
- **长文本**：单条 `append_text` 超过上限（`MAX_PATCH_TEXT_CHUNK`）时拆成多条 patch，保证单帧
  不失控。
- **流式工具参数（评审 #109 [P1-2]）**：provider 每个 SSE chunk 发一片 `tool_call_stream`，一片
  一次全量重解析 + 全量回灌是 O(n²)。宿主把累积文本放在 `SessionRecord.toolArgsStream`
  （host-only，不进桥），**渲染预算**内才更新 cell：首片 / `is_final` / 累积 ≤512 字符（普通调用
  行为不变）/ 距上次 ≥100ms / 新增 ≥2048 字符 / 距上次 ≥32 片。跨桥的 `argsText` 另有
  `TOOL_ARGS_MAX_CHARS = 8000` 传输上限——它只是**预览**，权威的完整参数由最终 `tool_call`
  （parsed `args`）给出，展开卡片优先用后者。跳过某片时宿主模型同样不更新（mirror == host 的
  不变量因此仍然成立）。
- **diff 计算闸（评审 #109 [P1-1]）**：后端对 `Write` 发**全量** old/new 文本，Myers 的 trace 是
  `D × 2(N+M)`（3000 行整体重写实测 ~340MB，6000 行 ~1.19GB）。`lineDiff` 在裁剪公共前后缀之后按
  `DIFF_MAX_EDIT_LINES = 4000` 设闸，超限退化为「整块 del + 整块 add」（UI 只展示 800 行，看不出
  区别）；`DIFF_MAX_ROWS = 800` 依旧只管传输窗口——两者独立，别只依赖后者（它的位置在计算之后）。
- **toast 与会话快照的生命周期（评审 #109 [P2-4]）**：`MAX_TOASTS = 5`，每条按 level 超时自动
  `dismissToast`（`TOAST_TIMEOUT_MS`：info 4s / warning 8s / error 20s，另有手动关闭按钮）；
  `applyTabs` 顺手回收不在 tabs 里的 `sessions[id]` 快照（`retainContextWhenHidden` 下这是随窗口
  线性增长的泄漏）。两种到达顺序（`adopt()` 的 hydrate→tabs、`onReady()` 的 tabs→hydrate）都不会
  误裁活着的会话。
- **草稿的一次性令牌（评审 #109 [P2-5]）**：`state.draft` 的采纳判据是宿主单调递增的 `draftSeq`
  （不再是「值相等」——同一段文本连续两次发送失败时第二次会被吞掉）。网关未连接时 `sendMessage`
  的早退路径把文本回填（`SessionRecord.setDraft` + `state`），composer 的乐观清空不再是丢输入的
  窗口。`replyAsk` 不需要回填：ask cell 仍是 `awaiting`，用户可以再点一次。
- **失败模式**：webview 端不变量（`applyPatch.ts`）——seq 必须恰好 `lastSeq + 1`、寻址的 cell
  必须存在、op 必须匹配 cell 种类；任何一条不满足就 `resync`（报告
  `seq-gap`/`unknown-cell`/`duplicate-cell`/`unsupported-op`/`protocol`），宿主回 `hydrate`。
  **webview 永远不猜测**——猜测会在界面上留下永久错误的中间态。
- 桥协议版本 `BRIDGE_PROTOCOL_VERSION = 1`：宿主与 webview 版本不匹配时记警告日志（bundle 与
  宿主同包发布，正常不会遇到）。

## 6. 控制面与命令

webview 意图（`src/shared/bridge.ts` 的 `WebviewToHostMessage`，全部有类型）：`sendMessage` /
`interrupt` / `answerAsk` / `approveTool` / `newSession` / `closeSession` / `activateSession` /
`compact` / `setModel` / `setThinking` / `setEffort` / `setYolo` / `runPromptCommand` /
`openModelPicker` / `closeOverlays` / `openLink` / `openFile` / `openDiff` / `copyText`。

命令有两个来源（与 TUI 一致）：

1. **gateway prompt command**（`GET /api/commands`，如 `/init`）：作为普通消息文本发给后端，
   由后端展开 `$ARGUMENTS` 再给模型；
2. **frontend command**（`src/shared/commands.ts` 的 `FRONTEND_COMMANDS`，是 TUI
   `TUI_ONLY_COMMANDS` 的 1:1 镜像，去掉两条 Goal 命令）：不发模型。`kind: 'intent'` 的走
   专用意图（`/model`、`/think`、`/yolo`、`/ss`…），`kind: 'forward'` 的由宿主
   `runPromptCommand` 处理（`/context`、`/skills`、`/reload`、`/copy`、`/title`、`/workdir`、
   `/agents`、`/clear`…）。

已知偏差（有意）：

- `/clear` 是视图级语义（清屏但历史仍在），宿主模型是网关状态投影、没有「只清视图」的实现 →
  **明确提示不支持，绝不发给模型**（旧行为是把 `clear` 发给模型，被检查点②抓住）；
- 审批卡只有 approve/deny 两条路径（TUI 可输入 `yolo` 应答），需要 yolo 时用状态区开关或
  `/yolo on`；
- `/copy` 成功也会 toast（webview 没有 TUI 那样常驻的状态栏）。

## 7. 连接自愈的策略边界

- **探活优先**：扩展从不假设「没连上就是没启动」；`GET /api/health` 说活着就什么都不做。
  （`wing start` 只是"通常安全"，探活是唯一诚实的判断。）
- **一次自动拉起**：每次激活 / 每次手动 reconnect 最多跑一次 `wing start`；不无休止拉起。
- **首连重试归宿主**：core 的监督从「成功连接过」开始；激活时的连接失败由 `WingHost` 的阶梯
  重试。
- **unauthorized 不重试**：凭据问题重试无意义，配了错误的 `wing.apiKey` 就停在明确提示。
- 找不到 `wing`：设置项 `wing.wingPath` 可显式指定；否则搜 PATH 与常见安装位置
  （`~/.local/bin`、`~/.cargo/bin`、`~/bin`、`~/.wing/bin`、`/usr/local/bin`、Homebrew、`/usr/bin`）。

## 8. 构建与调试

### 8.1 三条 build 管线

| 产物 | 工具 | 特点 |
|---|---|---|
| `out/extension.js`（扩展宿主） | esbuild（`esbuild.mjs`） | CJS、`target: node20`（VS Code 1.100 = Electron 34 / Node 20.19）、`external: vscode`、sourcemap、**不 minify**（宿主日志可读性 > 几 KB）。`ws` 作为 devDependency 被 bundle 进来，作为「宿主没有全局 `WebSocket`（Node 20 就是这样）」的回落实现（评审 #109 [P1-3]；`src/core/transport/socket.ts` 的惰性 `require`） |
| `dist/webview/{main.js,main.css}` | Vite lib 模式（`vite.config.mts`） | 单 IIFE、**无动态 chunk**、无第三方 origin、无运行期 `fetch`；`main.js` 文件名是宿主契约的一部分 |
| `out/smoke/smoke.mjs` | esbuild（`esbuild.smoke.mjs`） | smoke 的开发工具产物（不进 `.vsix`） |

`pnpm run build` = extension + webview 两条；`pnpm run build:preview` 另出 `dist/preview/`。

### 8.2 Webview 运行时的硬规则

- CSP：`default-src 'none'` + 白名单（`img-src`/`font-src`/`style-src` 给 `cspSource`，
  `script-src 'nonce-…'` 单次 nonce）。宿主生成文档（`src/host/html.ts` 纯函数），bootstrap
  值 `window.__WING_BOOTSTRAP__` 内联且 HTML 转义。
- **bundle 不得引用 Node 全局**。`vite.config.mts` 显式内联 `process.env.NODE_ENV` 并把
  `NODE_ENV=production` 钉死。Vite 的 **lib** 构建不做这个替换，漏掉时 React 的 CJS 入口会留
  一条 `process.env` 分支 → 真实窗口里白屏 + `ReferenceError: process is not defined`。
- 样式全部走 CSS Modules + `--vscode-*` 主题变量；视觉常量集中在
  `src/webview/styles/tokens.css`，每个值注明取自本机 VS Code 1.129.1
  `workbench/contrib/chat/browser/widget/**` 的文件与行号。硬编码颜色被层守门拒绝。
- 代码块复制、文件跳转、打开原生 diff、外链一律过桥（`copyText`/`openFile`/`openDiff`/
  `openLink`）——渲染层不碰编辑器与剪贴板。

### 8.3 构建产物门禁的存在理由（别删）

`tests/artifact/webviewBundle.test.ts` **构建真实 bundle 并在没有 Node 全局的 jsdom realm 里
执行它**。它守的失败模式其它门全都看不见：检查点① 曾交付一个在真实窗口里白屏的 webview
（`process is not defined`），而 typecheck / lint / 层守门 / 组件测试全绿——因为它们看的是
TypeScript 与源码，看不到 bundler 实际吐出的字节。这个测试：

1. 用仓库自己的 `vite.config.mts` 临时构建（配置本身是输入 → 配置回归也会红）；
2. 断言产物文本（无 `process.env`、无 React 开发版标记、无 Node/CJS 残留）；
3. 用 `node:vm` 在「只有 DOM、什么都没有」的 realm 里跑起来并确认应用真的挂载。

### 8.4 F5 与预览 harness

- F5：用仓库里的 `.vscode/launch.json`（`extensionDevelopmentPath=${workspaceFolder}` +
  `--disable-extensions`），但**先 `pnpm run build`**（F5 复用 `out/` 与 `dist/`）。
- 迭代：`pnpm run watch`（宿主增量）+ `pnpm run dev:preview`（webview 在浏览器里，5199）。
- preview harness 有工具栏：切 fixture、流式一轮、打断 patch 流（验证 resync）、推 UI 动作——
  不启动 VS Code 就能看渲染改动的首选路径。

## 9. 测试

### 9.1 单元 / 组件（vitest，双 project）

| project | 环境 | 覆盖 |
|---|---|---|
| `node` | node | `tests/{core,host,shared,state,layers,artifact}`；`vscode` 模块 alias 到 `tests/mocks/vscode.ts`（记录式小 mock）——这就是扩展宿主可以无头测试的原因 |
| `webview` | jsdom | `tests/webview/**`，`@testing-library/react`，经真实 `mountApp` 对 `src/testing/mockBridge.ts` 的 scripted host 挂载 |

- `fake-gateway.ts`（core/host 各一份）是进程内假网关，用来重放「事故复盘」式序列：时序、隔离、
  重放、重连、错误面。
- `tests/shared/contract.test.ts` 把桥协议、命令表等跨侧契约钉死在测试里（改契约必须改测试，
  防止一侧静默漂移）。
- 组件测试覆盖交互分支（composer 路由、面板键盘、Tab 栏、流式渲染只重渲尾部等）。

命令（全部实跑过）：

```bash
cd extensions/vscode
pnpm run test          # 全量（44 文件 / 666 用例）
pnpm run typecheck     # tsc --noEmit × 3 projects
pnpm run lint          # eslint（含层门禁 zone）
pnpm run format:check  # prettier
```

### 9.2 smoke：真网关 × 真宿主 × 假模型

`pnpm run smoke:gateway` 是本仓库的**端到端确定性**验证：进程里跑生产宿主代码
（`WingHost` + `SessionManager` + reducer + 真实 WS/HTTP），对着一个**真 `wing-gateway` 子进程**
说话；模型侧是一个 Node 假 Provider（OpenAI 兼容 SSE，剧本按 model 路由）。断言落在**宿主 UI
模型层**（经 `WebviewMirror` 用 shipped 的 `applyCellPatches` 还原成 cells/状态/标题/meta/panels）
以及真实副作用（workspace 文件、Provider 请求数）。

拓扑与隔离（硬约束）：

```
Node smoke 进程
├── FakeProvider（Node http；按序消费剧本；请求留档）
├── wing-gateway 子进程（临时 WING_HOME + OS 分配端口；绝不碰 ~/.wing / 32523）
└── 生产宿主（真实 globalThis.WebSocket + fetch）
       └─ WS 经「无压缩闸门」TCP relay 连网关（见下）
```

- 网关二进制解析：`$WING_GATEWAY_BIN` → 仓库 `.venv/bin/wing-gateway` → `PATH`；都没有 →
  `SMOKE SKIP`（exit 3）。
- 退出码：`0` 通过 / `1` 失败（打印世界快照 + 网关日志尾）/ `3` 跳过（缺二进制或
  `WING_SMOKE_SKIP=1`）。
- flags：`--only <substring>`（单跑）、`--keep`（保留现场）、`--list`（列场景）。
- 环境变量：`WING_SMOKE_SKIP`、`WING_SMOKE_KEEP_COMPRESSION`（A/B 复现用，默认关）、
  `WING_SMOKE_AUTH_KEY`（生成的 config.yaml 打开 auth 且宿主带 `Authorization: Bearer`——
  12 个场景因此覆盖真实鉴权中间件的 HTTP + WS 两条路径；默认不设＝无鉴权）、
  `WING_SMOKE_WS_FALLBACK`（启动前删掉宿主进程的全局 `WebSocket`，12 个场景全部走 bundle 进来的
  `ws` 回落实现——复现 VS Code 1.100 / Node 20.19 宿主的唯一现实手段，见评审 #109 [P1-3]）、
  `WING_SMOKE_ROOT`、`WING_SMOKE_GATEWAY_PORT`。

12 个场景：`create-subscribe-send-stream`、`tool-call-diff`、`ask-round-trip`、
`bash-approval`、`interrupt`、`resume-history-state`、`rewind`、`fork`、`multi-tab-isolation`、
`reconnect-resubscribe`、`local-commands`、`gateway-command-to-model`。

**「无压缩闸门」是什么、为什么在**：`tools/smoke/ws-proxy.ts` 是一条本地 TCP 转发，只把客户端
upgrade 请求里的 `Sec-WebSocket-Extensions` 剥掉（= 网关不压缩 WS 帧），HTTP 原样透传。
动因是实测的环境缺陷：Node 25 的 undici WebSocket 在 `permessage-deflate` 下会**静默滞留**部分
帧（TCP tap 证明帧已由网关发出；裸 Python 客户端、无压缩握手都正常）。这影响的是 smoke 的
Node 宿主进程，不是产品（真实 VSCode/Electron 宿主未观察到）。`WING_SMOKE_KEEP_COMPRESSION=1`
可保留压缩复现 A/B。**不要在没重现缺陷前删掉它。**

### 9.3 仓库级接线

- `make check` / `make test` 的三个/四个分组里，`ts` 组 = `make check-ts` / `make test-ts`
  （先按 lockfile 安装，再 lint/format/typecheck 或 vitest）。分组执行器是
  `scripts/collect_output.sh`。
- `check-ts` / `test-ts` 开头显式探测 node（≥22.12，vitest 5 / vite 8 的下限）与 pnpm 11，缺工具
  时打印可操作提示（`corepack enable` / `SKIP_TS=1 make check`）再退出；`SKIP_TS=1` 是本地逃生舱，
  `collect_output.sh` 会把 ts 组标成「已跳过」而不是「通过」。**CI 不设该变量**，门禁照旧强制。
- `make fmt` / `make fmt-check` **有意不含 TS**：pre-commit 要秒级、且不该强依赖
  `node_modules`；TS 格式门禁在 `make check-ts` 与 CI 里（另有 `make fmt-ts` / `fmt-check-ts`
  可手动用）。
- CI job `typescript-check`：install（`--frozen-lockfile`，pnpm 版本取自 `package.json`
  `packageManager`）→ lint → format:check → typecheck → test → build → build:preview → package
  → 上传 `.vsix` artifact。缓存键是 `extensions/vscode/pnpm-lock.yaml`（**锁文件必须提交**）。

## 10. 打包与安装

```bash
cd extensions/vscode
pnpm run package        # vscode:prepublish → pnpm run build；然后 vsce package
pnpm exec vsce ls       # 核对进包清单
```

进包只有：`out/extension.js`、`dist/webview/{main.js,main.css}`、`media/wing.svg`、
`LICENSE.txt`、`package.json`、`README.md`（+ vsce 的两个清单文件）。`src/`、`tests/`、
`tools/`、`preview/`、`out/smoke/`、`*.map` 被 `.vscodeignore` 排除。

细节与理由：

- 所有 runtime 依赖都声明为 **devDependency**：全部被 bundle 进 `out/`/`dist/`，`vsce` 没有
  生产依赖树要走（也少一类「忘了 dependencies」的事故）。
- `engines.vscode`（当前 `^1.100.0`）与 `@types/vscode`（`1.100.0`）必须对齐，`vsce` 会以此
  报警。
- `LICENSE.txt` 是仓库许可证的拷贝——包必须自带 license 文件。
- 安装（用户环境）：**旧包与新包同 ID**（`wing-agent.wing-vscode`）。本机曾装有 7 月的旧实现
  （chat participant 形态，同为 `0.1.0`），同版本不走 VS Code 的 update 语义，确定路径是先卸
  再装：

  ```bash
  code --list-extensions --show-versions | grep wing-agent   # 看现在装了什么
  code --uninstall-extension wing-agent.wing-vscode          # 卸载旧实现（若有）
  code --install-extension wing-vscode-0.1.0.vsix            # 安装新包（路径以实际为准）
  # 若跳过卸载：同版本重装需要
  # code --install-extension wing-vscode-0.1.0.vsix --force
  ```

  装完 `Developer: Reload Window`（或重启 VSCode）让新宿主生效。

## 11. 已知限制与 follow-up

功能/环境限制（都在验收清单与 PR 描述里有对应条目）：

1. `/clear` 不支持（见 §6）；
2. 审批卡无 `yolo` 应答路径（见 §6）；
3. **F5 环境风险**：Node 25 / undici 在 `permessage-deflate` 下可能滞留 WS 帧（smoke 用无压缩
   闸门绕过）。真实 VSCode 里若「resume 后历史迟迟不出现」，切一次 Tab 或
   `Wing: Reconnect to Gateway` 会重新推送重放；把 Output → Wing 日志附进报告；
4. 远程 / SSH / 多机网关不在支持范围；鉴权开启时在设置里填 `wing.apiKey`（`Authorization: Bearer`
   **header**，不进 URL；明文存储，建议只放 **User** settings——工作区作用域会写进可提交的
   `.vscode/settings.json`，Settings Sync 也会同步；改用 `context.secrets` 是 follow-up）；
5. 多工作区窗口只在单测层覆盖（取第一个 folder），真机行为未系统验证。

视觉打磨 follow-up（用户检查点②反馈，**本轮有意不修**，供后续 PR 引用）：thinking/工具卡折叠无
过渡动画、长 thinking 收起高度跳变；代码块从纯文本到 shiki 着色的跳变；流式表格宽度抖动；diff
卡小宽度换行多；状态区窄宽度省略且无 hover 全量；Tab 关闭按钮/状态点触达区偏小；overlay
滚动条与内容重叠；Wing 输出通道首屏无分隔标题。

## 12. 排错

| 症状 | 原因 / 处理 |
|---|---|
| 视图白屏 | 打开 Webview DevTools 看 console；多数是 CSP / 模块加载 / Node 全局问题。`tests/artifact/webviewBundle.test.ts` 能本地复现同一类失败 |
| console: `main.js.map violates … CSP` | 无害：sourcemap 走 `connect-src`，文档有意不开；map 只给开发用（`.vsix` 里没有） |
| 网关连不上 | Output → Wing 看日志；确认 `wing start` 或 `wing.autoStart`；`wing` 找不到时设 `wing.wingPath` |
| 鉴权被拒 | 提示会直说；核对设置 `wing.apiKey`（网关 auth 关闭时留空） |
| `vsce` 报 `@types/vscode` | 把 `engines.vscode` 与 `@types/vscode` 调回同一版本线 |
| smoke 直接 skip | 本机没有 `wing-gateway`：`uv sync` 后跑，或设 `WING_GATEWAY_BIN`；故意跳过用 `WING_SMOKE_SKIP=1` |
| resume 后历史不出现（真实窗口） | 见 §11.3；切 Tab / 重连重推重放，并收集 Output → Wing 日志 |
| F5 窗口没有 Wing 图标 | 先 `pnpm run build`；看开发窗口的 Extension Host 日志 |

## 13. 改哪里：常见改动地图

| 想做的事 | 从哪开始 |
|---|---|
| 加一种 cell 类型 | `src/shared/cells.ts`（联合）→ 宿主 `reducer.ts`/`derive.ts`（怎么产生）→ `src/webview/chat/Cells.tsx` + `CellView.tsx`（怎么画）→ `applyPatch.ts` 若引入新 op |
| 加一个 webview 意图 | `src/shared/bridge.ts`（消息联合）→ `src/webview/app/Composer.tsx` 或对应组件（发出）→ `src/host/session/manager.ts`（处理）→ 两侧各自的测试 |
| 加一个本地命令 | `src/shared/commands.ts`（名字与 kind）+ 宿主 `runPromptCommand` 分支 + `tests/shared/commands.test.ts`（命令表被测试钉死） |
| 改桥协议 / 面板字段 | `src/shared/bridge.ts`/`session.ts` + `tests/shared/contract.test.ts`，两侧同步——`interfaces.md` 的教训：单侧私改必冲突 |
| 调视觉 | `src/webview/styles/tokens.css`（先溯源到本机 VS Code 源码再改；硬编码颜色会被门禁拒绝） |
| 加 smoke 场景 | `tools/smoke/scenarios.ts`（剧本 + 断言都在代码里，不写配置） |
| 动连接策略 | 先读 §7 与 `WingHost`/`launcher.ts` 的注释；策略是「探活优先、一次拉起、不猜」 |

## 14. 验证命令速查（都实跑过）

```bash
# 工程门禁（extensions/vscode 内）
pnpm install --frozen-lockfile --prefer-offline
pnpm run typecheck && pnpm run lint && pnpm run format:check
pnpm run test                 # 666 用例
pnpm run build && pnpm run build:preview
pnpm run package              # → wing-vscode.vsix
pnpm run smoke:gateway        # 12 场景（真网关 + 假 Provider）
pnpm run smoke:list           # 只看场景清单
pnpm run dev:preview          # 本地预览（http://localhost:5199/）
pnpm run watch                # 宿主增量构建

# 仓库根（含 python / rust / ts）
make check
make test
make fmt-check
```
