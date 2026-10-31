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

## 1.1 与唤醒的关系

唤醒（`wing watch` / `wing wake` / `wing run --notify*`）的**默认目标**就是这个环境变量：
「谁叫醒我」缺省 = `WING_SESSION_ID`（我自己的会话），于是「我派出去的会话做完了回来告诉我」
在 agent 的 shell 里不需要任何参数。血缘与唤醒是**两个独立机制**（血缘记「谁创建了我」，
唤醒记「什么事实叫醒谁」），唯一的交点是这个环境变量——两者都不校验对方的存在性，都是软机制。
详见 [wake.md](wake.md)。

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
  `--tag` 同理）；`wing ps` 的人类默认输出是**层视图**（一层一个任务，后代折进 `CREW`
  列 `40 ?1 ●3`：后代总数 / 其中在等你 / 其中在干活），`--flat` 是旧的全量平铺、
  `--tree` 是整棵树的缩进画法（三者的定序与聚合见 §4）；`wing info` 显示 `parent:` 行。
- `wing fork` 不参与（见 §1 的两维度说明）。

## 4. 树序、pin 与 crew 投影（三个消费面共用的契约）

树构建（`shared/session_tree.rs`，Rust 侧唯一实现；前端 TS 侧按同契约另实现）：

1. parent 不在展示集合（被过滤 / 折叠 / 幽灵引用）→ 该节点升为根；
2. 环（只可能来自手改 metadata）→ 环上节点全部升为根（诚实降级，不挂死）。

**兄弟组内排序**（自顶向下递归，每一层独立应用；根层与任意子层同一条比较器）：

1. 被 pin 的在前（pin 时间降序，无时间的次之）；
2. **子树注意力**：自己或任一后代在等人类回答（`waiting`）的在前；
3. 其余按**子树 max(last_interaction)** 降序（最近有动静的任务树浮上来）；
4. 并列按 id 升序（跨消费面一致）。

pin 与注意力**刻意不对称**：pin 只在兄弟组内说话（见下），注意力是**子树属性**——
有后代在等你，整棵树上浮。

**pin 分两层**（产品语义，来自用户拍板）：

- **根的 pin**（parent 不在集合、展示为根的节点）：上浮到跨树最前，**子树跟随**——
  即出现在置顶区；
- **子节点的 pin**：只在它所在的兄弟组内排前，**不提升自己、也不影响父节点**的排序。
  置顶区只对根开放（预期用户很少 pin 一个子节点）。

**crew 聚合**（每个节点的四个数：折叠时「代表但不枚举」后代所需；一次自底向上算全）：

| 度量 | 口径 |
|---|---|
| 后代总数 | 全部后代，**含 dormant**——它回答「这个任务有多大」，不是「这一屏有多少行」 |
| 后代 waiting / working | 后代里在等人类回答 / 在跑轮次的条数（同样含 dormant） |
| 子树活跃 | 自己与全部后代的 `max(last_interaction)`，与排序键同源 |

**存活闭包**：`kept(r) = live(r) ∨ pinned(r) ∨ ∃ 子 c: kept(c)`，其中
`live = status != inactive`、状态未知（旧网关 / 用量报告）按存活处理。闭包是必要的：
否则一个 dormant 的父任务会凭空消失，「层 = 任务」退化成「一堆散点 crew 升格成根」。
消费面选择显示 dormant 行时（全量逃生舱）闭包不生效，`kept ≡ true`。

**层投影**：`level(scope)` = 该 scope 的**直接子** ∩ 保留集，顺序 = 森林全序在该层的
限制（只过滤、不重排）；`scope = None` 是根层。同一次投影给出本层**被收起的直接子数**
（「⋯ N dormant」尾注）。

三件事——保留集、被收起的直接子数、crew 的三个计数（`activity` 同源）——都按**全集**算，
因此**投影的输入必须是未按存活过滤的行集**：过滤是投影的一部分，不能发生在投影之前
（否则保留集 ≡ 输入集、计数少算 dormant 后代；一个 dormant 父被剪掉，它的活子就升成假根）。

不变量（`session_tree.rs` 的测试锚定）：

- 投影是 `(rows, scope, include_dormant)` 的**纯函数**：不看时钟、不看随机数、不看输入顺序
  （同 id 的重复行例外：只可能来自畸形输入，届时按输入序兜底成全序）；
- 投影**不改变任何一行的相对顺序**；
- 保留集里每个节点在它所属的那一层**恰好出现一次**（无重复、无孤儿）；
- 投影吐出的行下标是**输入切片的坐标**：消费面在持有投影期间不得重排 / 裁剪那份数组；
- 三个消费面共用这一份实现，不各写一份比较器。

**渲染前缀**：`├─` / `└─` / `│` 三字形；根不带前缀（树与树之间不连线）；缩进层数封顶
16 层（深链防御）。

`wing ps` 的三个人类模式见 §3：三者是同一份行集的不同投影，`--json` 的行集与字段不受
呈现方式影响。

## 5. 边界与已知限制

- 幽灵父 / 环 / 深链：均按 §4 降级，不挂死、不污染健康行；
- memory 后端的父会话在进程重启后消失 → 链断（孤儿降级为根），可接受；
- 绕过面（HTTP 直调 / 覆盖环境变量）是**刻意保留**的软约束，不是漏洞；
- 后端不做「子不得生孙」的 policy 执行——数据面就绪，执行器未建（未来按需外挂）。

## 6. 未来（非本 PR）

- policy：子会话不得再创建子会话（现在有了判定所需的全部数据）；
- 子会话的 Ask 由父会话回答（已有半条链路：`wing asks` + `--tool-call-id` 应答）；
- 子树级联展示：成本聚合（usage 求和）、折叠交互、wing-app 的 rail 嵌套（TS 侧）。
