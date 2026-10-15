# 会话血缘（session-lineage）：父子关联、采集机制与树呈现

> 本页是这条特性的**产品语义参照点**：字段语义、软约束的边界、排序与 pin 规则。
> 实现落点：后端 `SessionMetadata.parent_session_id`（`libs/core/wing/store/base.py`）、
> Rust 侧树算法唯一实现 `crates/wing/src/shared/session_tree.rs`、CLI 采集
> `crates/wing/src/cmd/common.rs`。

## 1. 产品语义

每个会话可选携带一个**创建者**引用（`parent_session_id`）：谁创建了我，单向、层层
指向根。根 = 用户直接创建的会话（用户自己的 shell 里没有被注入的环境变量）。

来源机制：Bash 工具为每个 agent 的 shell 显式注入 `WING_SESSION_ID=<本会话 id>`
（`libs/core/wing/tools/builtin/bash.py`）；agent 经 CLI 创建新会话时，CLI 读取它并
在创建请求里带给网关——于是「Session B 由 Session A 创建」被持久记录。

这是**软约束**：不做存在性校验、不设强制门禁，且刻意保留全部绕过面（HTTP 直调不带
字段、`WING_SESSION_ID=x wing run` 挂到别处、`env -u WING_SESSION_ID` 脱离）。它服务的
是**分类与呈现**（任务树 / 团队视图 / `wing ps --parent` 筛选），不是访问控制。

与 `forked_from` 是**两个正交维度**：`forked_from` 记录内容血统（fork 的源会话），
`parent_session_id` 记录创建动作（谁启动了它）。fork 不写 parent（v1）。

## 2. 数据模型与协议

- **持久**：`SessionMetadata.parent_session_id: str | None`（metadata.json）。创建时
  一次写入，此后**没有修改路径**（树是创建时刻的事实）。无父会话零字段（序列化排除
  None，存量文件零变化）。
- **校验**：只做格式卫生（同 session id 规则：非空、UTF-8、≤128 字节、无路径分隔符 /
  `..` / 控制字符、不以 `.` 开头）；**不校验存在性**（弱引用——幽灵父由展示层降级为根）。
  自指（`session_id` 与 `parent_session_id` 同时给出且相等）→ 400。
  收养路径（create-or-adopt 命中已存在 id）**忽略** parent（resume 语义），记 warning。
- **HTTP**：`POST /api/session/create` 请求与响应都携带 `parent_session_id`（响应是
  **读回**，调用方据此对账）；`GET /api/session/list` 支持 `?parent=<sid>` 过滤（与
  status / tag 同款子集语义：全量列表的子序列，直接子）；`SessionInfo` 列表行与
  `/api/session/info` 都携带该字段。

## 3. CLI 采集

- 创建点统一经 `cmd/common.rs` 的一个 helper 读取 `WING_SESSION_ID`（合法 → 带上；
  **非法 → 丢弃 + 日志，绝不阻断创建**——env 是软信号）：`wing run` / `wing new` /
  `wing -p`（stdio，含 create-or-adopt）/ `wing acp` 的 session/new，以及 **TUI 进程的
  全部创建路径**——启动建会话、`/new`、以及恢复时磁盘无此会话后的重新铸造
  （`app/transport.rs` 的新铸路径同样创建新会话）。
- 输出对账：`wing run` / `wing new` 的文本与 `--json` 输出展示**读回的** parent；
  请求了但网关没带（旧网关）只记日志（软机制，不吵）。
- `wing ps --parent <sid>` 列出某会话的直接子（隐含 `--all`——血缘是长期事实，与
  `--tag` 同理）；`wing ps --tree` 是树状人类模式（默认平铺输出不变）；`wing info`
  显示 `parent:` 行。
- `wing fork` 不参与（见 §1 的两维度说明）。

## 4. 树序与 pin（三个消费面共用的排序契约）

树构建（`shared/session_tree.rs`，Rust 侧唯一实现；前端 TS 侧按同契约另实现）：

1. parent 不在展示集合（被过滤 / 折叠 / 幽灵引用）→ 该节点升为根；
2. 环（只可能来自手改 metadata）→ 环上节点全部升为根（诚实降级，不挂死）。

**兄弟组内排序**（自顶向下递归，每一层独立应用）：

1. 被 pin 的在前（pin 时间降序，无时间的次之）；
2. 其余按**子树 max(last_interaction)** 降序（最近有动静的任务树浮上来）；
3. 并列按 id 升序（跨消费面一致）。

**pin 分两层**（产品语义，来自用户拍板）：

- **根的 pin**（parent 不在集合、展示为根的节点）：上浮到跨树最前，**子树跟随**——
  即出现在置顶区；
- **子节点的 pin**：只在它所在的兄弟组内排前，**不提升自己、也不影响父节点**的排序。
  置顶区只对根开放（预期用户很少 pin 一个子节点）。

**渲染前缀**：`├─` / `└─` / `│` 三字形；根不带前缀（树与树之间不连线）；缩进层数封顶
16 层（深链防御）。

`wing ps` 的**默认（非 --tree）输出排序保持现状**（pin + 后端基准序），树序只作用于
`--tree` 与 TUI 面板。

## 5. 边界与已知限制

- 幽灵父 / 环 / 深链：均按 §4 降级，不挂死、不污染健康行；
- memory 后端的父会话在进程重启后消失 → 链断（孤儿降级为根），可接受；
- 绕过面（HTTP 直调 / 覆盖环境变量）是**刻意保留**的软约束，不是漏洞；
- 后端不做「子不得生孙」的 policy 执行——数据面就绪，执行器未建（未来按需外挂）。

## 6. 未来（非本 PR）

- policy：子会话不得再创建子会话（现在有了判定所需的全部数据）；
- 子会话的 Ask 由父会话回答（已有半条链路：`wing asks` + `--tool-call-id` 应答）；
- 子树级联展示：成本聚合（usage 求和）、折叠交互、wing-app 的 rail 嵌套（TS 侧）。
