# 设置面（Setting API · TUI 设置面板 · 首次运行向导 · `wing config`）

本页是**配置这条产品线**的机制级文档：一份声明（`S(...)`）怎么变成目录 / 模板 / 校验，配置怎么经
API 保存与生效，配置不可用时网关怎么活着（setup mode），以及三个消费入口（TUI 浮层面板 /
首次运行向导 / `wing config` CLI）各自长什么样、改它们要注意什么。

涉及两个文件（都叫 `config.yaml`，语义完全不同）：

| 文件 | 谁的数据 | 目录（catalog）来源 | 写盘路径 |
|---|---|---|---|
| `$WING_HOME/core/config.yaml` | 网关后端配置 | `GET /api/settings/schema`（Python 侧 `config/catalog.py`） | `POST /api/settings/set` → `runtime.apply_settings`（唯一写盘路径） |
| `$WING_HOME/tui/config.yaml` | TUI 自己的配置 | Rust 侧 `config/catalog.rs::interface_catalog()`（后端不知道它） | `config/store.rs::write_interface_doc`（纯本地原子写） |

一句话：**用户再也不用手编这两个文件**——首次运行 `wing` 就是向导，日常在 TUI 的 `/settings`
面板里改，headless 场景用 `wing config`。但文件仍然存在、仍然是唯一事实来源（面板只是它的编辑器）。

为什么要有这一层（改它之前先明白的动机）：配置的正确性知识（哪些必填、枚举有哪些值、改了什么时候生效）
过去只锁在**人类读的注释**里，机器读不到 → 任何设置界面都长不出来；而配置写错时网关进程会**直接崩溃退出**
——最需要设置界面的那一刻，后端不存在。这两件事分别由「声明层」与「setup mode」解决。

## 1. 一份声明，三份产物

字段的元信息在**声明处**写一次：`libs/core/wing/config/spec.py` 的 `S(...)` 把 `SettingMeta` 塞进
`FieldInfo.json_schema_extra["wing"]`（键名常量 `WING_META_KEY`），读取口只有 `setting_meta(field)` 一个。

| 元信息 | 含义 |
|---|---|
| `doc` | 一行摘要（必填）：面板行内提示 + YAML 注释 |
| `notes` | 多行详解：面板详情栏 + YAML 块注释（按 `\n` 切行） |
| `title` | 人类标签；缺省 = 字段名 |
| `apply` | 生效域（见 §7）；**门禁强制每个 `S(` 调用显式写出** |
| `secret` | 密文：只写不回显（见 §6） |
| `choices` | 枚举值域：值 → 含义（面板的内联选择项；`Literal` 之外的补充） |
| `example` | 示例值（详情栏 + YAML `# e.g. …` 注释） |
| `editable` | `false` = 面板灰显只读 |
| `summary_fields` | 列表项标题行的字段名序（`providers` / `agents` / `providers[].models` 各声明一组） |
| `min_items` / `max_items` | 列表规模**声明**（"不得为空" = `min_items: 1`）；强制仍归跨字段检查 |
| `deprecated` | 预留（本期不消费） |

**分组不在这里声明**（曾经的 `section` / `section_doc` 已移出 `SettingMeta`）：界面分类住在
`config/groups.py` 的一张有序表里，见 §1.1。

其余参数（`default` / `default_factory` / `gt` / `pattern` / …）原样透传 `pydantic.Field`。
**必填只有一处定义**：字段没有 `default`（catalog 读 `FieldInfo.is_required()`）——不引入第二个 `required=`。

三份产物都从 `config/models.py` 的声明生成，各自是纯函数（无 I/O）：

| 产物 | 模块 | 消费方 |
|---|---|---|
| 设置目录树（`SettingNode`）+ 路径文法（`parse_path`） | `config/catalog.py` | `GET /api/settings/schema`、TUI 面板、`wing config` CLI、`emit.py` 自己 |
| 规范形 YAML（注释来自声明） | `config/emit.py` | 首启模板（`loader._create_config_template`）与保存写盘（`apply_settings` 第 ⑥ 步）**共用同一个函数** |
| 跨字段检查（`ConfigProblem` / `check` 纯函数） | `config/problems.py` | 加载期（`Config._validate_config` 只 raise 第一个）+ 面板 / `status` / `doctor`（全部问题 + 精确路径） |

**`config/default_config.py`（247 行手写 YAML 模板）已删除**，`# SYNC` 那套纪律随之死亡：
模板 = `emit_config_yaml(default_document(), build_catalog())`，`default_document()` = `{"providers": [], "agents": []}`
（首启模板的空列表是**天然的 problem**——向导据此指路，不再需要 `ChangeHere` 这种假占位符）。

规模（`build_catalog()` 实测）：**68 个声明字段 / 80 个节点 / 55 个叶子 / 7 个业务分组**；
`secret` 2（`providers[].api_key`、`gateway.auth.keys[].key`）、`enum` 6、`min_items=1` 3 组。

### 1.1 业务分组：存储形式与界面分类的解耦缝

`config/groups.py` 的 `SETTING_GROUPS` 是**界面分类的唯一声明处**：一组 = `(id, title, doc, members)`，
表的顺序即界面顺序（设置面板左列锚点、`wing config list` 的分组头、`config.yaml` 的 `# ── Name ───`
分隔行都由此而来）。成员是 `Config` 的**顶层键名**——`config.yaml` 的存储形状一概不动，
所以加组 / 并组 / 改名 / 调序都是**零迁移**的声明层改动。

| id | title | 成员（顶层键） |
|---|---|---|
| `providers` | Providers | `providers` |
| `agents` | Agents | `agents` |
| `behavior` | Behavior | `safe_command_patterns` · `yolo` · `steer` · `tool_result_truncate` |
| `images` | Images | `images` |
| `sessions` | Sessions | `sessions` |
| `gateway` | Gateway | `gateway` |
| `advanced` | Advanced | `hooks` · `commands` · `log` · `user_agent` |

（`Interface` 是第 8 个锚点，但它是 TUI 自己的配置：声明在 Rust 侧 `config/catalog.rs::interface_groups()`，
后端不知道它。见 §9。）

三个消费口径（都从这一张表来，不存在第二份）：

| 消费方 | 读什么 |
|---|---|
| `GET /api/settings/schema` | `groups[]`（id / title / doc / members）——**前端左列锚点的唯一来源**，前端零硬编码 |
| catalog 节点 | `build_catalog()` 最后一步把 `section = group.title` 盖到 root 的直接子节点上，`section_doc = group.doc` 只盖在**声明序首成员**上（emitter 的一条块注释） |
| `emit.py` | 按 `section` 分段发分隔行；**键序恒为模型声明序**，分隔行按组的首次出现发一次（分组表管界面顺序，不管文件顺序） |

门禁（`tests/test_config_groups.py`）：id / title 唯一、成员非空且都是真实顶层字段、
每个顶层字段**恰好**属于一个组（漏 = 界面里无处可去，重复 = 两份真相）、顺序钉死。
`build_groups()` 在构建期就抛（不是运行期悄悄漏），probe 侧另有 wire 形状与保存回归的整机断言
（`scenarios/test_settings_groups.py`）。

**两侧对"漏了怎么办"是刻意的两种口径**：后端**硬失败**（未分组的顶层键 → `build_catalog()` 抛 →
`GET /api/settings/schema` 500，因为那是开发者写错了声明，CI 必须挡住）；前端**软兜底**
（没被任何组认领的键进一个 `{root}:ungrouped` 锚点，因为前端要面对的是"任意版本的网关"——
老网关、半升级的网关、被 hack 过的网关都不能让设置面板打不开）。硬失败管住自己的仓库，
软兜底管住别人的进程，两者不矛盾。

## 2. 门禁：不是纪律，是机制

五处测试让「声明 / 目录 / 分组 / 模板 / serde」不许漂移（改声明而不同步的代价是测试变红，不是"有人会记得改"）：

| 门禁 | 位置 | 钉住 |
|---|---|---|
| 每个配置字段必须声明 | `libs/core/tests/test_config_spec.py` | 递归遍历 `Config` 及嵌套模型：字段缺声明 / `apply` 没显式写（AST 检查）/ 字段 docstring 残留 / `secret` 不是 `str` / `choices` 缺 `Literal` 的键 → 红；**声明层不再携带分组**（AST 拒绝 `S(section=...)`） |
| 分组是一张完整的划分 | `libs/core/tests/test_config_groups.py` | 顺序钉死、id / title 唯一、成员都是真实顶层字段、每个顶层字段**恰好**属于一个组；坏表（漏 / 重 / 幻影 / 空组）→ `validate_groups` 抛；目录投影（`section` = title、`section_doc` 只在首成员）+ wire 形状 |
| 分组的 wire 形状 | probe `scenarios/test_settings_groups.py` | 真网关的 `groups[]`（顺序 / 成员 / 与节点 `section` 同源）+ 零迁移与保存回归（改跨旧分组的键、文件顶层键不变、分隔行跟着新分组） |
| Interface 根双向对账 | `crates/wing/src/config/catalog.rs` 的单测 | **穷尽 struct 字面量**构造 `AppConfig` 全字段样本（加字段即编译失败）→ catalog 缺声明 / 声明了幻影键 → 红；`interface_groups()` 的成员 = catalog root 的子节点 |
| 模板不许撒谎 | `test_config_emit.py` | 首启模板可解析 + 覆盖每个声明字段（键行或注释行）+ `ChangeHere` 计数 = 0 + 注释掉的默认值与声明一致 + **分隔行恰好是分组表的 title 序** |

`config/catalog.rs` 是 Rust 侧的**声明表**（后端不知道 TUI 的类型），与后端 catalog **同构**：
同一个 `SettingNode` 类型（来自 `wing-api-client`），所以树 widget 零分支。Interface 根 22 个叶子 =
`colors.*` 15（`preset` + 14 个颜色槽，槽位带 `value_hint: "color"` 触发色块预览）+ `layout.*` 3 +
`rendering.*` 3 + `api_key`（`secret` + `apply: restart`——它只在建连时读一次）。

## 3. 稀疏文档与「缺席即默认」

`config.yaml` 是**稀疏文档**：只写用户显式设置的键。没有写下的键 = 跟随声明默认值，
升级后未钉住的值会跟随新默认。两条推论写进产品的每一层：

- **emitter 把缺席且有默认的值写成注释掉的行**（`# timeout_first_chunk: 300.0`）——文件自文档，
  想钉住就取消注释，想跟随升级就留着注释；
- **「复位为默认」= 从文档里移除该键**（面板的 `r`、CLI 的 `unset`），不是往文件里写默认值。

emitter 的输出规则（`config/emit.py`，首启模板与每次保存写盘都是它的产物）：

| 规则 | 行为 |
|---|---|
| 文件头 | 4 行注释（产品名 / 文件路径 / "推荐在 TUI 里用 `/settings` 编辑" / 无时间戳——避免 diff 噪声） |
| 分组 | 按业务分组（§1.1）分段：`# ── Name ───` 分隔行 + `section_doc`，按组的**首次出现**发一次（键序恒为声明序） |
| 注释位置 | 文档注释在值**上方**（`doc` → `notes` → `# e.g. …` → 生效域 → 密钥说明），不用行尾注释 |
| 缺席有默认 | 标量写成注释行；object 整棵子树注释；list / map 注释成 `# key: []` / `# key: {}` |
| 缺席必填 | 写出键 + 空值（`""` / `0` / `false` / `[]` / `{}`）——它就是一条 problem，向导据此指路 |
| 生效域标记 | `restart` → `# 生效：需重启网关`；`next_session` → `# 生效：新会话`；`hot` 不加（避免噪声） |
| 密文 | 值照写（文件本来就是明文密钥的家）+ 注释 `# 密钥：面板里只写不回显（末 4 位提示）` |
| 空容器 | 必须显式写 `key: {}` / `key: []`（只写 `key:` 是 YAML null，round-trip 会破） |
| 引号 | 能不加就不加（判据 = 不加引号能否原样读回）；需要时双引号 + JSON 转义，非 ASCII 原样 |

**未知键保留（前向兼容）**：`config/document.py` 读取时把 schema 之外的键收进 `extra`
（**结构性三元组** `ExtraKey(父容器前缀, 原始键名, 值)`，如 `("gateway", "weird", …)` /
`("providers[0]", "foo", …)`；freeform map 的内部键不算），
保存时从**磁盘文档**取回、写在父容器已知键之后，上面一行
`# unknown key (not recognized by this wing version)`；父容器自己缺席时兜底写在文件末尾。
——新版本写的键，旧版本编辑时**不能吃掉**（这是"安全"与"毁掉用户配置"的分界线）。
**键名不参与任何字符串切分**：`gateway.foo.bar` 这种「键名本身含点」的未知键必须原地写回
（曾经用 `rpartition(".")` 反推父容器，会把键挪到错误的名字 / 位置）。
代价：`get` 不下发未知键（面板没有节点可渲染它们），客户端无法删除它们。

**`.bak`**：保存前把现有文件**字节级**复制到 `config.yaml.bak`（覆盖式，只留最近一份；
读不出文档的坏文件也逐字留证）。它是"第一次经面板保存会把手写注释换成声明生成的注释"的兜底。

## 4. 路径文法（catalog 与 values 共用）

```
path    := segment ("." segment)*
segment := name | name "[" idx "]" | name "[]"
name    := [A-Za-z_][A-Za-z0-9_]*      idx := 非负整数（≤ 2**64-1，拒绝 a[+1] / a[-1]）
```

`PathStep` 三变体（P3）：`Key(name)` / `Index(i)` / `Element`。三条口径：

- **catalog 用 `[]`**（元素模板）：`providers[].models[].capabilities.vision`；
  **values / problems / changed / CLI 用具体下标**：`providers[0].models[2].capabilities.vision`。
  模板 → 具体是一对多展开（前端按文档实际长度展开）；
- **下标不查界**：catalog 是模板，不知道文档里有多少元素。`providers[7].api_key` 寻址到元素模板，
  越界由**文档编辑**阶段报错（CLI）或就是一条普通 problem；
- **根名前缀 `config.` 可选**：`node_at_path`（Python）与 `node_at`（Rust）都容忍两种拼写；
  `wing config` 在入口做一次规范化（**只**剥根前缀，剥完为空 → 用法错误并列出可用顶层节名，AD8），
  回显与错误信息一律用规范路径。

两份实现（Python `catalog.parse_path` / Rust `wing_api_client::models::parse_path`）跨语言无法共享代码，
但文法冻结、两侧用同一批用例各自单测（含 `a[+1]` 拒绝）。`parse_path` 解析失败返回 `None` / 不抛。

## 5. Setting API（4 个端点 + 保存事务）

| 方法 | 路径 | 语义 | 鉴权 | setup mode |
|---|---|---|---|---|
| GET | `/api/settings/schema` | 设置目录（catalog 树）+ **业务分组 `groups[]`**（§1.1）+ 版本 + `config.yaml` 绝对路径；纯静态 | 常规 | ✅ |
| GET | `/api/settings/get` | 稀疏文档（密文叶子 = `null`）+ 密文状态表 + 指纹 + 全部问题 | 常规 | ✅ |
| GET | `/api/settings/status` | `{valid, setup_mode, problems, fingerprint}`——启动路径上的最便宜预检 | 常规 | ✅ |
| POST | `/api/settings/set` | 保存事务（见下） | **admin**（`tool_runtime` 403） | ✅（唯一修复路径） |

读端点**不经过 `server.runtime`**（纯函数 + 投影组合），写路径住在 runtime；服务端**不缓存文档**——
每次 GET / POST 都现读磁盘，永远与文件一致。

保存事务（`WingRuntime.apply_settings`，唯一写盘路径）：

```
① 现读磁盘 → (current_doc, current_fp)        ← 读不出来（YAML 语法错）也继续，见下
② base != current_fp → 409，不写盘            ← 乐观并发；base=None（CLI --force）跳过检查
③ 密文回填：null = 保留磁盘现值                ← 按身份配对，见 §6
④ 字段级 + 跨字段校验（全有或全无）             ← 有 problem → 200 + ok=false，一个字节都不写
⑤ 备份 config.yaml.bak（字节级原子写，覆盖式）
⑥ emit_config_yaml(incoming) → 原子写 config.yaml
⑦ 生效：正常模式 = 热重载六项；setup mode = 就地转入正常模式（§8）
⑧ changed_paths / restart_required（只读**叶子**的 apply，P13）
⑨ 广播 SettingsChangedEvent（global）
⑩ 回执 SettingsSetResponse
```

**校验失败走 HTTP 200 + `ok=false` + `problems`（不是 4xx）**——这是刻意的取舍，别"修好"它：
请求本身完全合法，是**用户填的内容**不合法；把它当业务结果返回，让前端保存路径永远拿到同一个响应类型
（`ok` 决定成败、`problems` 逐条驱动标红）。HTTP 错误码只留给协议级失败：
**409**（指纹不匹配，`error="conflict"`，`detail` 带当前指纹）、**401/403**（鉴权）、**500**（写盘 `OSError`）。

回执字段：

| 字段 | 含义 |
|---|---|
| `ok` | 事务是否成功（`false` 时文件一个字节都没写；`fingerprint` 是磁盘当前值） |
| `fingerprint` | 落盘后的 sha256（文件不存在时字面量 `"absent"`） |
| `problems` | 校验问题（path / kind / message / hint） |
| `changed` / `restart_required` | 相对保存前的变更路径；其中 `apply == restart` 的（只读叶子） |
| `reload` | 逐项热重载结果（`name` / `ok` / `detail`，与 `/api/system/reload` 同形） |
| `setup_mode_exited` | 这次保存是否把网关从 setup mode 推进正常模式 |
| `backup_path` | `.bak` 路径；磁盘上原来没有文件时为 `null` |
| `warnings` | 非致命告知（与 `problems` 的区别：problems 让保存失败，warnings 只是提醒） |

`changed_paths` 的口径：对两棵稀疏文档做递归 diff，**列表按下标**（删除 `providers[0]` 会把后续项
逐个记成 modified——这是"N 项变更"的展示粒度，不是三方合并）、类型敏感（`True != 1`、`300 != 300.0`）、
缺席与空容器等价。`restart_required` 只读**非结构节点**的 `apply`（容器节点的 apply 是"子树最粗"，
不能用来判定；解析不出的路径跳过——宁可漏报也不误报）。

保存时写一行结构化 INFO 日志：`settings changed: paths=[…] restart_required=[…] fingerprint=<old>→<new>`
——**只有路径，没有值**（值一律不落日志，密文自然脱敏）。不新增审计文件：配置变更史就是
git-tracked 的 `config.yaml` + `.bak`。

**读得出文档的前提下，一切照上表；读不出（语法错 / 顶层不是映射）时事务继续**：
现文档视为**空**（未知键无从保留）、指纹仍按文件字节算（乐观并发不因文件坏了失效）、`.bak` 照常生成，
回执带 `warnings: ["原配置文件无法解析，其中的密钥无法保留，请重新填写"]`
——语法错意味着我们不知道里面有什么，旧密钥无法回填是正确取舍（见 §8 的修复路径）。

## 6. 密文：只写不回显

- **读**（`get`）：`values` 里每个存在的密文叶子替换为 `null`（真实值不出网关）；
  平行表 `secrets: {<路径>: {state, hint}}`：
  `state ∈ {set, empty, absent}`（非空 / 存在但是空串 / 键不在文档里），
  `hint` = 值长度 **≥ 8** 时的末 4 位，否则 `null`（短密钥不给 hint，避免泄露比例过高）。
- **写**（`set`）的三态：`null` → **保留磁盘现值**；字符串 → 设为该值（`""` = 显式清空）；
  键缺席 → 该项不被覆盖（从文件移除；两个密文字段都是必填，所以这会成为 problem，而不是静默清空）。
- **`null` 的「保留」按身份配对，不按下标**（`document.resolve_secrets` 的 LIST 分支，
  三段式、顺序即优先级；声明面是 `SettingMeta.identity_field`，目录落在 `SettingNode.identity_field`，
  **不进 wire**）：
  1. **身份配对**：列表节点声明了 `identity_field` 时，用磁盘现值建「身份值 → 项」映射
     （只收录非空字符串身份），incoming 项按自己的身份值查表配对。`providers → name`
     （`cross_field_problems` 强制 provider name 全局唯一，所以它是合法身份）。
  2. **安全的下标回落**：**仅当** `len(incoming) == len(current)` **且该下标未被第 1 步认领**
     （consumed 守卫）时，对没配上身份的项按下标配对——覆盖「重命名」（身份变了但结构没变）。
     **一旦真的按位置保留了密文，回执必出声**（B1 返修 / AD18）：逐条给出
     `providers[1].api_key 按位置保留了磁盘上的值（该项的 name 与磁盘上的项不一致）——若这是重命名，无需处理；若是替换成了新的项，请重新填写该密钥`
     （原因子句三态：该列表没有身份字段 / 该项没有 <身份字段> / 该项的 <身份字段> 与磁盘上的项不一致）。
     整表替换（全删全加、长度相等）与「全改名」在文档里结构不可区分，守卫关不掉——所以这里
     把「宁可不猜」退半步成**有告知的歧义**：保留值 + 警告，而不是硬失败。不撤 (b) 的理由：
     撤掉会让「重命名一个带密钥的 provider」直接保存失败，而密钥是掩码的、面板里拿不回原值
     （只有末 4 位 hint），代价不对等；而 (b) 的误判面需要客户端为**全新项**发 `null` 哨兵
     （存量 TUI stub 写 `api_key: ""`、CLI `add` 的 stub 不写该键，两个前端都触发不了）。
  3. **宁可不猜**：以上都配不上（长度变化 / 槽位已被认领 / 身份重名 / 无身份可查）时，该子树的
     `null` 哨兵解析为「键被移除」（必填字段随之成为 problem，保存被拦住），并在回执 `warnings`
     里逐条点名：
     `无法确定 providers[1].api_key 属于哪一项（列表结构变化且无法按身份配对），已移除该密钥，请重新填写`。
     真丢过值才警告（候选位置没有值 / 用户从没设过密钥时静默，那是既有语义）。
  - **不变量：列表长度不等时绝不按下标配对**——按下标回填会把 A 的密钥**静默**写给 B
    （保存成功、回执不报告、用户下次调用才发现 401 或打错账号），是本模块唯一的数据损坏级缺陷来源。
  - 没有可达密文叶子的列表（`agents` / `providers[].models`）不需要 `identity_field`；
    `gateway.auth.keys` **没有可用身份**（`key` 是密文、`role` 不唯一）→ 不声明，走 2/3：
    长度不变照旧按位置保留（**每次都会出声**，见第 2 条——没有身份就没有别的依据），
    长度一变整表落「不猜」（丢弃 + 警告 + 重填）。
- **契约红线**（`shared/panels/settings/mod.rs` 的模块文档与单测钉住）：**前端必须把从 `get` 拿到的
  `null` 原样回传**。丢掉这个键 = 清空密钥 = 用户下一次调用 401。面板从不删除用户没动过的密文键，
  密文编辑器缓冲**从空开始**（提交空缓冲 = 取消），且缓冲只有 `visible_buffer()`（`•` × 长度）一个出口
  ——没有任何路径能把明文渲染到屏幕上。
- `wing config` 侧同样零回显：`list` / `get` / 写回执 / `--json` 一律 `••••••••` + 末 4 位 hint，
  另有按目录递归的脱敏兜底（`redact_secrets` / `redact_node_defaults`），即便后端没掩码也不输出真值。

## 7. 生效域（apply）

每个声明字段必须写 `apply`，四档：

| 档 | 含义 |
|---|---|
| `hot` | 保存即热重载生效（provider 池重建 / 每调用现读） |
| `next_session` | 新建会话生效；进行中的会话保持自己的状态（agent 模板 / agent 构造期快照 / 会话自己的 glob patterns） |
| `restart` | 进程级：必须重启网关（监听地址、启动时注册的 job 间隔） |
| `readonly` | 派生值 / env 覆盖 / 只读事实，面板不可编辑（本期没有字段用它） |

**容器节点**（`providers` / `sessions` / `gateway` …）的 apply 是**子树里最粗的一档**，只作展示提示；
**权威在叶子**——保存回执的 `restart_required` 只读叶子路径。全表（照 `config/models.py` 的声明抄）：

| 路径 | apply | 依据（读取点） |
|---|---|---|
| `providers[].name` | `next_session` | 池按 name 懒建实例；改名 = 旧 name 从配置消失、新 name 出现。retire 语义保留已移除 name 的旧实例（钉在它上面的会话不被拆解），旧会话可用到重启，新会话用新 name |
| `providers[].protocol` / `base_url` / `api_key` / `timeout_first_chunk` / `timeout_total` / `max_retries` / `max_retry_delay` / `explicit_cache_mode` / `reasoning_effort` / `max_tokens` / `extra_body` / `anthropic_version` / `models[]` / `image_delivery` / `image_max_bytes` | `hot` | 热重载第 4 项 `reset_providers()` 重建共享池（先建后换，旧实例在途计数归零后退场） |
| `agents[].*`（`name` / `model` / `default` / `system_prompt` / `tools` / `context_window_tokens` / `keep_recent_tokens` / `skills` / `rules` / `max_turns` / `yolo`） | `next_session` | `SessionManager.reload_templates()` 重建模板；已在内存的会话保持自己的 agent。`skills` / `rules` 另外在 `ContextManager` 构造时快照 patterns，热重载时按**会话自己的** patterns 重新 glob |
| `safe_command_patterns[]` | `hot` | `tools/internal/shell_safety.py` 每调用 `get_config()` |
| `yolo` / `steer` | `next_session` | agent / ReAct 循环构造时读 |
| `tool_result_truncate.*` | `hot` | 工具执行器每调用读 |
| `images.*` | `hot` | ReadImage 与请求期媒体投影每调用读（见 [media-images.md](media-images.md)） |
| `sessions.eviction.enabled` / `idle_ttl_seconds` | `hot` | 逐出扫描每次 sweep 读 |
| `sessions.eviction.sweep_interval_seconds` | `restart` | 启动时注册 job 的间隔读一次（热重载不改变已注册 job） |
| `gateway.host` / `gateway.port` | `restart` | uvicorn 已绑定；且 Rust 侧启动时读文件决定连哪（不做假热更） |
| `gateway.auth.enabled` / `keys[]` | `hot` | `server.auth_config` property 每请求 `load_config()`，中间件不缓存 |
| `gateway.remote_tool_timeout` | `hot` | 每次远程工具 dispatch 读 |
| `hooks[]` | `hot` | 热重载第 2 项 `hooks.clear() + load_hooks()` |
| `commands.paths[]` | `hot` | 热重载第 3 项 `remove_by_source("prompt") + register_prompt_commands()` |
| `log.level` | `hot` | 热重载**末项**：`setup_logger` 幂等重挂 handler（每日文件日志恒 DEBUG，不受此项影响） |
| `user_agent.preset` | `hot` | `get_headers()` 每次调用读 |

**热重载的逐项名字序是对外契约**（probe `test_system_reload` 钉住，只许在**末尾**追加）：

```
config.yaml → hooks → prompt commands → provider → skills & rules → log level
```

`POST /api/system/reload` 与保存事务的第 ⑦ 步走**同一条管道**：config 项失败立即中止（后续项不再尝试），
其余项失败继续，逐项 `ok` / `detail` 如实上报；失败**不回滚文件**（配置本身合法）。

已知边界：**列表元素模板（`[]` 节点）是合成节点，apply 恒为 `hot`**（不继承字段的 apply）——
`agents[].skills` 这个**字段**是 `next_session`，而 `agents[0].skills[0]` 这个**元素行**显示 `hot`。
今天无功能影响（唯一的 `restart` 叶子都不在列表里，`restart_required` 只读叶子），
但**不要**依赖元素模板的 apply 做判定。

## 8. Setup Mode（配置不可用时的降级启动）

配置缺失 / 非法时网关**不再崩溃退出**（那条路径已被消灭）：`config/boot.py::boot_config()` 是启动路径上
唯一「读配置且不抛」的入口，四种结局：

| 结局 | 情形 |
|---|---|
| `template_created` | 文件不存在 → 写默认模板（`providers: []` / `agents: []`）→ 降级 |
| `parse_error` | YAML 语法错 / 顶层不是映射 / 读不出来（文案含 `problem_mark` 的行列号） |
| `invalid` | 能解析成映射但校验不过（字段级 + 跨字段，与 `/api/settings/status` 同一口径） |
| `missing_file` | 文件不存在且模板也写不出来（只读 home / 磁盘满） |

「永不抛」是**结构性**保证：最外层 `except Exception` 兜底 + `locate_problems()` 自带兜底
（任何输入形态都产出问题清单，而不是 500 / traceback）；顶层键不是字符串（`1: oops`）另有一条
说得清的显式守卫。`boot.ok == true` ⟺ `load_config()` 会成功（成功判定用同一个 `Config(**raw)`）。
单例优先：进程里已经有 `Config` 单例时直接以它为准，不重新读盘。

降级后网关进入 **setup mode**：

- **对外只有一个面**：`SETUP_ALLOWED_PATHS` 九条 —— `/api/health` · `/api/settings/schema` ·
  `/api/settings/get` · `/api/settings/status` · `/api/settings/set` · `/api/shutdown` ·
  `/openapi.json` · `/docs` · `/redoc`（修复期也要能查协议）。其余一切 **503**，
  `error == "setup_mode"`（`detail` 是前 10 条 problem + 修复指引）。
  注意 `HTTP_ERROR_TYPES[503]` 是通用的 `service_unavailable`——setup 语义是守门**显式覆盖**的，
  Rust 侧 `is_setup_mode()` 的判据就是「503 且结构化 body 的 `error == "setup_mode"`」。
- **`/ws` 在 accept 之前以 1013 关闭**：客户端看到的是握手失败而不是"连上就断"；
  setup mode 下 WS 没有任何可用功能（修复全在 HTTP 设置端点上）。
- **修复模式鉴权 = loopback-only 免 key**：免 key 需要**两个条件同时成立**——
  ① 请求来源是 loopback（`LOOPBACK_HOSTS = {127.0.0.1, ::1, localhost}`），且
  ② **网关自身的绑定地址是 loopback**（`GatewayServer.host`，即 `cli.py` 从文件/参数解析出来的那个，
  **不在中间件里重读 config**——它正是此刻不可信的那份）。任一不成立 → 403。
  配置坏掉 ⇒ auth 配置本身不可信（读不出来），所以修复模式 ≈ 本地控制台权限、**不要求 key**：
  **这是收紧不是放松**：正常模式 `auth.enabled=false` 时任何人都能访问，setup mode 下只有
  「本机来源 + 本机绑定」这一种组合能进。为什么②必须有：**只看来访地址会被本机转发洗白**——
  把网关绑到 `0.0.0.0` 之后，任何从 `127.0.0.1` 转发进来的本机进程（无鉴权反代、容器 sidecar、
  本地端口转发）都会让远端流量以 loopback 身份到达，而此时 setup mode 授予的是**免 key 的整份配置
  写权限**（能改回 `auth.enabled=false`、换 provider 端点）。绑 `0.0.0.0` / `::` / 任何非 loopback
  地址时**连本机来源也拒绝**：setup mode 下没有可核验的 key（`server.auth_config` 恒为安全默认、
  没有 keys），「要求一把无法核验的 key」等价于拒绝，返回 401 才是撒谎；detail 直接指路——
  **把 `gateway.host` 改回 `127.0.0.1` 并重启**（或手工编辑 `config.yaml`，CLI/TUI 之外的最后手段）。
  代价是把网关暴露到 `0.0.0.0` 且配置坏掉的部署无法远程修复——刻意的安全姿态。
  中间件顺序：`AuthMiddleware` 在**外层**（后 add），守门在其内——被拒的来源在 setup mode 下
  任何路径都先吃 403（而不是 503）。
- **`server.runtime` 恒非 Optional**：setup mode 下它是只服务保存事务的**替身**
  （`_SetupRuntime`：只放行 `apply_settings` / `post_write_effect`，其余一切属性抛 `SetupModeError`）。
  于是 20 多处 `server.runtime.xxx()` 一行未改，而「runtime 在 setup mode 不可用」仍成立
  ——守门中间件是主闸，替身 + `app.py` 的异常处理器（503 + `error="setup_mode"`）是万一可达时的安全网。
- **`_enter_operational()` = 六步，幂等**：① `config.yaml`（现读或由调用方给）② `log level`
  ③ `prompt commands` ④ 建 `WingRuntime`（hooks 随之加载）⑤ 逐出 job + 后台任务（lifespan 已跑过时补启）
  ⑥ auth 锁死警告。**任一步失败 ⇒ 停在 setup mode**（`_runtime` 与布尔在同一同步块里翻转，请求之间看不到
  中间态）、文件**不回滚**（它本身是合法配置）、回执如实报明细；下次保存或重启再试。
  启动路径上转入失败**不许静默降级**：记 ERROR 日志 + 写一条 `path=None` 的 boot problem
  （否则会出现 503 说"共 0 条问题"而 `valid=true` 的自相矛盾）。
  **终端横幅覆盖两条降级路径**（`cli.py`）：横幅条件不是 `boot.ok`，而是构造完 server 之后的
  `server.in_setup_mode`——「配置合法但运行时装配失败」这条也打同一段（reason 取 `server.boot_reason`、
  问题清单取 `server.setup_problems`），且**恰好一次**（`boot.ok=false` 那条不再在构造前重复打印）。
- **`valid` 必须自洽**：`valid == (not setup_mode and 无 problem)`——降级态**恒** `false`，
  不允许 `valid=true` 且 `setup_mode=true`。`get` / `status` 的 `setup_mode` 字段是真值
  （取自 `server.in_setup_mode`），面板据此显示徽标。
- **永不退回 setup mode**（反向不成立）：正常模式下外部改坏文件 + `/api/system/reload` 的既有语义是
  "报告 config.yaml 失败、保留旧配置"，照旧。
- **降级期的两个问题源**：`setup_problems`（启动快照，503 detail 的素材）与 `status` 的现读现报。
  `get` / `status` 在降级期把两者**按 `(path, kind)` 去重后并排**——顶层非字符串键在磁盘视图里只是"未知键"
  （不算错误），但网关确实因它没起来，不并进来用户会看到"invalid 却一条问题都没有"。
- **host:port 一致性**：`BootResult.endpoint` 从文件里现取（文件能解析出映射就取，字段类型不对时整段回落
  默认，与 Rust `cmd/backend_config.rs` 的 `#[serde(default)]` 语义对齐）——TUI / CLI 与降级启动的网关
  因此落在同一个端口上。

## 9. TUI 设置面板（v2：浮层卡片 + 双栏）

**落点：居中浮层卡片（不是聊天流里的 cell，也不再是全屏 overlay）**。设置是"去一个地方"而不是
"transcript 里的一条消息"；需要不随滚动漂移的框架（标题栏 / 副标题行 / 键位栏）；一节可能十几个字段，
聊天流的 5 行窗口会窒息；且不扰动 transcript 的滚动位置、选区锚点、图片放置表。
v2 把"全屏"改成"浮层"：四周露出聊天背景，用户始终知道自己还在会话里。

几何（`ui/settings/mod.rs::card_area` 是唯一实现，App 与测试共用）：

| 规则 | 值 |
|---|---|
| 卡片尺寸 | `min(终端宽-4, 110) × min(终端高-4, 32)`，居中 |
| 退化 | 终端 `< 80×24` → 铺满（浮层比全屏更难读的尺寸就别浮了） |
| 左栏宽度 | `clamp(内容宽 × 22%, 14, 22)`，且保证右栏 ≥ 24 列（否则**不出左栏**，右栏独占） |
| 详情栏 | 右栏 ≥ 78 列时竖切出来（树 58% / 详情 42%）；否则右栏末行退化为当前行的 doc 提示 |
| 键位栏 | 按内容装箱 1~2 行（装得下一行就把省下的行还给主体） |

**双栏：左栏 = 业务分组锚点，右栏 = 当前分组的设置项**。锚点表由 `shared/panels/settings/groups.rs`
从两份声明拼出（后端 `schema.groups[]` + Rust 侧 `config/catalog.rs::interface_groups()`），
**前端零硬编码**：组名 / 顺序 / 成员全在声明层（§1.1），加组并组改名不用碰 Rust。
Interface 是**最后一个锚点**（不再有 v1 的"切根"概念，两个根只是锚点表里的两段）；
老网关不发 `groups[]` 时按 root 子节点的 `section` 推导兜底（`SettingGroup::derive_from_sections`），
没被任何组认领的顶层键进一个兜底锚点——**任何情况下都不会有设置项在界面里消失**。

进入一个分组会展开它的顶层成员（右栏一进来就有内容），换组时右栏光标回到首行；
`providers[0]` 这类列表层级只出现在右栏。左栏的锚点带三种徽标：脏（`●`）/ 问题数 / 搜索命中数。

**提交语义 = 实时预览 + 显式保存**（v1 原样保留）：Interface 根的任何提交**立即**产生一次
`PreviewInterface(整份稀疏文档)`（App 换掉自己的 `config` / 调色板 → 整屏重绘；改颜色当场变色，
`Esc` 放弃时回退快照）；Gateway 根的改动只标脏、攒到 `s` 一次保存。`s` **保存两边**、各自成败、
回执合并成一条 notice（`✓ Interface …` / `✓ Gateway …` / `⚠ gateway.port 需重启网关才生效 — Ctrl+R 立即重启`）。
只有真的改了的那一边才会发请求 / 写文件（`gateway_dirty` / `interface_dirty`）。

键位表（`shared/panels/settings/mod.rs` 是唯一实现，`ui/settings/**` 只渲染）：

| 键 | 左栏（分组） | 右栏（设置项） | 编辑器激活 | 问题清单 | 选择项展开 |
|---|---|---|---|---|---|
| `↑` `↓` | 选分组（钳制） | 移动光标（钳制） | **忽略**（模态：必须 Enter / Esc 收口） | 选择（左栏焦点时选分组） | 内项光标 |
| `PageUp` `PageDown` / `Home` `End` | 按锚点可见行数翻页 / 首尾 | 翻页 / 首尾 | `Home`/`End` 行首尾 | 翻页 / 首尾 | — |
| `Tab` | 进右栏 | 回左栏 | 忽略 | 切栏 | 切栏 |
| `←` | 无操作（已在最左） | 折叠 / 已折叠则跳父行 / **depth 0 无处可去则回左栏**；enum 行循环切值（往前） | 左移字符 | 无操作（回树用 `Esc` / `p`） | 折叠（不选） |
| `→` | 进右栏 | 展开；enum 行循环切值（往后） | 右移字符 | 跳到该行（左栏焦点时 = 进那一组的树） | — |
| `Enter` | 进右栏 | 按行类型分派（展开 / 切换 / 选择项 / 编辑 / 新增 / 选中 / 只读） | 提交（**不改 = 无操作**） | 跳到该行 | 选中并折叠 |
| `Space` | — | bool 切换 | 插入空格 | — | 选中 |
| `a` | — | 给最近的列表祖先新增一项（union 列表先选形态） | 输入字符 | — | — |
| `d` | — | 列表项删除 / 标量字段清空（二次确认） | — | — | — |
| `J` `K` | — | 列表项下移 / 上移（顺序有意义：模型目录 / glob 序） | — | — | — |
| `r` | — | 叶子复位（从稀疏文档移除 = 跟随默认）；列表项 / 结构行无操作 | — | — | — |
| `s` | 保存两边 | 保存两边 | — | 保存 | — |
| `/` | 搜索（焦点自动进右栏） | 搜索 | — | 回树 + 搜索 | — |
| `p` | 问题清单（焦点进右栏） | 问题清单 | — | 返回树 | — |
| `?` | 帮助浮层 | 帮助浮层 | — | 帮助 | — |
| `R` | **重新载入**（丢弃本地全部改动、重拉 `get`；脏则先确认） | 同 | — | 同树 | — |
| `Ctrl+R` | **立即重启网关**（仅在 `restart_required` 非空时被接受、也只在键位栏显示；脏则先确认；轮次进行中由 App 拒绝） | 同 | — | 同树 | — |
| `Esc` | 阶梯 7–8（脏改动确认 → 关闭） | 阶梯 5–6（折叠选择项 → 回左栏） | 取消编辑 | 返回树 | 折叠 |
| `Ctrl+C` | **面板不吞**（应用保留手势：双击退出 TUI） | 同 | 同 | 同 | 同 |

`s` / `R` / `Ctrl+R` / `/` / `p` / `?` / `Tab` / `Esc` 在**两栏共用**（先于焦点分派处理）。

`Esc` 阶梯（命中即停）：① 编辑器激活 → 取消编辑；② 模态提示 → 取消；③ 搜索 → 退出 + 清 query +
恢复搜索前的**展开快照与分组**；④ 帮助 → 关闭；⑤ 右栏 + enum 选择项 → 折叠；⑥ 右栏 → **回左栏**
（v2 新增的一级：一次误触不该把用户送出面板）；⑦ 左栏 + 有脏改动 → "放弃 N 项未保存的改动？"
二次确认 → `Close{discard:true}`；⑧ 左栏 → `Close{discard:false}`。**关闭动作由 App 执行**（面板只产出意图）。

其余交互口径：

- **编辑器提交是无操作的**：打开 → 不改 → Enter 不写入、不标脏（预填用有效展示值——缺席时就是声明默认值，
  用户看到的与将要得到的一致）；
- **enum** 的选择项**内联展开为子行**（不切视图），`←`/`→` 在 enum 行上循环切值（power user 快捷键）；
- **密文行**渲染 `•••••••• ab12` / `(empty)` / `(not set)`，编辑器缓冲永不明文渲染（§6）；
- **问题清单是同一个组件的三个场景**：setup 首屏（初始视图 = 问题清单，焦点在右栏）、保存失败
  （`ok=false` 自动切过去）、随时用 `p` 查看。清单**不按分组过滤**（它是全局的），左栏只显示每组的问题数；
  `Enter` 会**切到问题所在的那一组**、展开祖先并把光标落到该行；文档级问题（`path == null`）不可导航、
  也不归任何组（标题栏的总数仍然算它）。本地按目录约束生成的问题（`min_items` / 必填缺席）与后端返回的
  问题合并去重（同 `(根, 路径, kind)` 为同一条；`message` 取后端的、`hint` 取两者中非空的那个），
  排序按严重度（`missing_required` > `unknown_reference` > `duplicate` > `empty_list` > `invalid_value` > `unknown_key`，
  未知种类最后）再按路径；
- **搜索跨全部分组**：子串匹配 `path` / `title` / `doc` / `notes` / `choices`（值 + 含义），命中项的祖先
  自动展开、命中的结构节点连带显示直接子节点。右栏一次只显示一组，所以**当前组零命中而别的组有命中时
  自动跳到第一个有命中的组**（左栏的命中数徽标同时告诉你还有哪些组），`Esc` 恢复搜索前的分组与展开快照；
- **模态优先级**：`ModalOwner::Settings` 最高，且是**唯一连应用保留键也接管的层**——面板开着时
  `Esc` / `PageUp` / `PageDown` / `Ctrl+O` 全部交给面板（`Ctrl+O` 被面板忽略，比让它穿透去改看不见的
  聊天状态安全），**只有 `Ctrl+C` 永远归应用**。面板开着时到达的 `ask` 排队不弹出，关闭后照旧弹出；
- **浮层期间的三件事**（改渲染顺序时最容易漏）：卡片挡住的图片**整张跳过**（`images.paint` 的 `masks`
  里同时有卡片与 toast；半张覆盖会撕碎图形协议），卡片之外的背景图片照画；选区被取消；滚动条不画且清掉
  hover / drag 态（`┃` / `█` 是滚动条独有码位，测试按"整帧扫这两个码位"判定）。
  **开面板与关面板各整屏重画一次**（`needs_full_redraw = true`，与 `images.invalidate()` 是既有配对）：
  卡片底下那张图已经画到屏幕上了，"这一帧不画它"不会让它消失（sixel 尤其如此），只有 `terminal.clear()` 会；
- 保存回执同时弹一条摘要 toast（面板压在 transcript 上，notice 在它底下看不见）；
  `settings_changed` 事件的指纹与本地不同时，副标题行出横幅「配置已被其它客户端修改，按 R 重新载入」
  ——**不自动覆盖**本地未保存的改动（v1 把它画在树的第一行，v2 给了它常驻的副标题行，
  优先级：**搜索行 > 横幅 > `/ 搜索…` 占位**——query 是输入回显，被顶掉就等于"敲了字看不见"，
  所以搜索态下横幅退化成同一行尾部的一段 `⚠ 已被其它客户端修改`，放不下时先丢横幅）。

## 10. 首次运行向导（setup TUI）

`wing`（TUI）的启动链在 WS 连接之前加**一次预检**（`GET /api/settings/status`，最便宜的一次调用）：

| 预检结论 | 行为 |
|---|---|
| `Ready`（`valid=true`，或 404 / 200 但 body 不是 status 形状的老网关兼容路径） | 走今天的启动链，一行不改 |
| `Unusable`（`valid=false`，或 503 `setup_mode`） | 进**无 session 的 setup 循环** |
| `Failed`（网络 / 401 / 403 / 5xx） | 报可操作错误（"Make sure the gateway is running: wing start" / 后端 detail 原文） |

setup 循环（`crates/wing/src/cmd/setup.rs`）的定位：**不是把 `App` 改成可无 session，而是在 `App` 之前
放一个更小的东西**——没有 session、没有 WS、没有意图队列、没有重连、没有图片 / 选区 / 滚动条；
前景就是同一个 `SettingsPanel` + `SettingsOverlay`（首屏 = 问题清单），背板复用
`ui/welcome/{art,sprite,wordmark}` 的帧数据与绘制原语（`Welcome` 结构体本身绑定 chat header，不复用）。
`s` 保存后：网关转入正常模式（`setup_mode_exited` 或复检 `status.valid`）→ 背板显示
`✓ 配置就绪，正在启动…` → 返回 `Ready`，启动链继续；否则留在问题清单。
`Ctrl+C` 双击 / `q` → `Quit`，终端恢复后打印一行出路（`wing config doctor` / 直接编辑路径）。

stdio（`wing -p`）与 ACP（`wing acp`）不能开面板：同一个预检失败时把 problems 打到 **stderr**
并退出 **78**（`sysexits.h` 的 `EX_CONFIG`，让编排器判别"这是配置问题不是网络问题"）——
**stdout 一个字节都不许被污染**（那是协议帧通道）。

## 11. `wing config`（headless CLI）

设置面板只服务 TUI；`wing config` 是 stdio / ACP / CI / 编排器的入口，也是**配置坏掉时唯一还能用的**
诊断入口（setup mode 下照常工作）。全部读写走 Setting API，`cmd/config.rs` **零业务逻辑**
（参数解析 + 值强制转换 + 调用共享文档原语 + 输出格式化；稀疏文档上的结构编辑——
路径行走 / stub / 列表增删移——的唯一实现在 `shared/doc_edit.rs`，与设置面板共用），
gateway 不可达时**不自动拉起网关**（它常常正是在网关起不来时用的；自动拉起会掩盖问题）。

| 子命令 | 作用 |
|---|---|
| `doctor [--json]` | 校验并列出全部问题（setup mode 下也能用） |
| `list [--json] [--section S] [--only-overridden]` | 树形列印：路径 / 当前值 / 默认值 / 生效域 / 是否覆盖 / 问题标记 |
| `get <path> [--json]` | 单项：值 + 默认 + doc + 生效域 + 约束（`node` 递归原样复用协议类型） |
| `set <path> <value> [--json-value <raw>] [--force]` | 按 catalog `kind` 强制转换；`--force` = 跳过指纹检查 |
| `unset <path> [--force]` | 回到默认（从稀疏文档移除；本就不在 → 无操作短路，不发请求） |
| `add <path> [<value>] [--variant simple\|object] [--force]` | 列表追加（打印新增项路径） |
| `remove <path> [--force]` | 按下标删除列表项 |
| `move <path> <delta> [--force]` | 列表项移动（越界钳制；没动 → 无操作短路） |
| `path` | 打印 `config.yaml` 绝对路径（**唯一不联网**的子命令） |

退出码：**0** 成功（含无操作短路）· **1** 有问题 / 一般失败（`doctor` 的 problems；写命令 `ok=false`，
未写盘；5xx / 404 旧网关）· **2** 网关不可达 · **3** 乐观并发冲突（409，重跑即可）·
**4** 用法错误（路径文法 / 目录寻址 / 值转换 / 参数互斥）。clap 自身的解析错误沿用 2（发生在
dispatch 之前）。传输 / 协议级错误一律 stderr，`--json` 只影响"命令拿到了结构化结果"的输出路径。

值强制转换（`set` / `add` 共用）：`bool`（`true/1/yes/on` · `false/0/no/off`，大小写不敏感）、
`int`、`float`（`300` → `300.0`）、`str` / `secret` 原样、`enum` 必须精确 ∈ `choices[].value`、
`map` / `object` / `list` 收合法 JSON；任意 kind + `nullable` 的 `null` / `~` / 空串 → JSON `null`
（先于 kind 判定）；`--json-value` 是逃生舱（原样 JSON、不做 kind 检查，与位置参数互斥）。
自由 map 的**内部**不可寻址（catalog 只声明到 map 这一层）——整份 map 用 JSON 文本改
（`set providers[0].extra_body '{"thinking":{…}}'`），与面板的单行 JSON 编辑器同口径。

已知边界（后端契约的推论，不是 CLI 的取舍）：保存是**全有或全无**，所以配置当前非法时任何单条写命令
都会被拒绝（一个字节都不写）；多问题修复要用 TUI 面板（本地累积编辑 + 一次保存）。

## 12. `settings_changed` 事件

`POST /api/settings/set` 成功后广播 `SettingsChangedEvent`：
`{changed, restart_required, setup_mode_exited, fingerprint}`，`target = global`（所有已连接客户端），
`persist = False` 且**不进 `FACT_EVENTS`**——它是网关级**时点通知**（没有 `session_id`、不进任何会话链），
重连后的正解是重新 `GET /api/settings/status`，重放一条旧通知只会误导。

消费侧**不判断"这是不是我自己刚触发的那一次"**：比指纹就够了——自己的保存会更新本地指纹，
指纹相同即忽略；不同则面板出横幅 / 缓存的 `(values, fingerprint)` 直接作废（避免"旧文档 + 新指纹"
被乐观并发当成 base，造成丢失更新）。

## 13. 已知边界

| 边界 | 说明 |
|---|---|
| `extra_body` 只有单行 JSON 编辑器 | freeform map 不能编辑多行 YAML 块、不能带注释；CLI 侧同样不可寻址其内部键 |
| YAML 锚点 / 别名会丢失 | 解析后写回规范形，锚点不保留（emitter 是"声明 → 文本"的函数） |
| 首次经面板保存会换掉手写注释 | 注释来自声明、永远更新；`.bak` 兜底 |
| 生成模板里"取消注释"对自己是误导的 | 注释行的形态对**单行标量**成立；嵌套块（`colors:` 等）取消注释没有意义——推荐用面板的"复位为默认"逐项调整（TUI 模板文件头的那句提示只对单行成立） |
| 两个 `config.yaml` 的注释语言不一致 | Gateway 侧全中文（面板直接展示这些文案）；TUI 侧字段 `doc` 沿用既有的英文、新增 `notes` 用中文 |
| `--dump-config` **默认掩码**，掩码后的输出不是 round-trip artifact | `wing tui --dump-config` 打印的是**当前文件**的规范形（本 PR 之前它打印的是默认模板，从不带用户密钥），因此密文叶子默认写成 `api_key: null` + 一行指向真值的注释；round-trip 要显式 `--show-secrets`。**掩码输出重定向回文件会让密钥变成"缺席"**，这是默认掩码的必然代价。保存路径用的是 `DumpMode::Raw`（写真值），**别把两者"顺手统一"** |
| 颜色槽"注释掉的默认值"是 `preset: wing` 下的值 | 改 `preset` 后它们仅供参考（不随预设重算） |
| setup mode 下非 loopback 无法修复 | 刻意的安全姿态（§8）：免 key 还要求**网关自身绑定 loopback**，所以把网关绑到 `0.0.0.0` 时连本机来源也不放行（防本机转发洗白）。修法：把 `gateway.host` 改回 `127.0.0.1` 重启，或手工编辑 `config.yaml` |
| 重命名列表项时密钥靠「长度相等的下标回落」保留 | 身份（`name`）变了但列表长度没变、且该下标**没被身份配对认领** ⇒ 按位置配对（§6 第 2 条）+ **回执有一条「按位置保留」的说明**（AD18）。长度变了 / 槽位已被认领则宁可丢弃 + 警告重填。删除 / 前插 / 互换都按身份精确配对，不受影响 |
| 密钥跟**名字**而非**行**（身份 = 名字的固有语义） | ① 删 p2 再把 p1 改名成 p2 ⇒ 改名后的项配到 `p2`、拿到**已删项 p2 的**密钥（`changed` 会报 api_key 值翻转，不静默，但用户下次调用可能 401）；② 整表替换（全删全加、长度相等）⇒ 新项按位置继承旧项密钥——**现在都会出声**（按位置保留的警告，§6 第 2 条） |
| `gateway.auth.keys`（无身份字段）：长度一变整表丢密钥；等长**重排**时 role 跟行 | 元素里没有可用的非密文唯一标量（`role` 不唯一、`key` 是密文）⇒ 无法按身份配对：长度变了整表落「不猜」（丢弃 + 警告 + 重填）；长度不变按位置保留（密钥随位置搬走、**不会串到别的 key 上**，但重排时两把 key 的 role 会静默互换）。等长保存**每次都会出声**（按位置保留的警告，重排与否都提醒） |
| 未知键**非字符串**（`1: oops`）保存后被字符串化（`"1": oops`） | 位置与值不变、**键的标量类型**变。取舍成立：`Config(**raw)` 与 Rust serde 都只接受字符串键（AD12 守卫），保真 int 键连 emitter 都出不去，且会让「面板保存修好坏配置」这条路径死掉。D17 的承诺对象是"新版本写的键"，而 wing 只写字符串键 |
| 列表元素模板的 `apply` 恒为 `hot` | 合成节点不继承字段的 apply（§7 末）；判定请用字段级声明 |
| 搜索按子串命中，会命中 `notes` 里的路径文本 | 例如搜 `providers[].models` 会先命中 `providers` 节点的 doc；不是 bug，是"文档也参与匹配"的直接后果 |

## 14. 改它要注意什么（速查）

- 加 / 改一个配置字段：只动 `config/models.py` 的 `S(...)` 声明（+ 必要时的跨字段检查），
  模板 / 目录 / 面板 / CLI / 校验会跟着变；跑 `test_config_spec.py` 与 `test_config_emit.py`。
  **新的顶层键还要在 `config/groups.py` 的 `SETTING_GROUPS` 里归一个组**——不归就是
  `build_groups()` 当场抛（界面里它会无处可去），`test_config_groups.py` 也会红。
- 改界面分类（加组 / 并组 / 改名 / 调顺序）：只动 `SETTING_GROUPS` 一张表（§1.1）；
  `config.yaml` 零迁移（顶层键不动，只有分隔注释跟着变）。前端**不许**出现组名字面量——
  左栏锚点全部来自 `schema.groups[]`（Interface 的那一份在 `config/catalog.rs::interface_groups()`）。
- **给 LIST 字段加密文叶子前先想配对**：只要元素子树里有可达的 `secret=True` 叶子，就必须同时声明
  `identity_field`（元素里那个唯一的非密文标量字段名）——否则列表结构一变，`null` 哨兵配不上，
  密钥只能被丢弃（回执警告 + 用户重填），或者更糟：**按下标错配**（§6 的不变量）。
  声明错（字段不存在 / 指向密文 / 非标量）会被 `test_config_spec.py` 的 `identity_field` 门禁当场拒绝。
- 给 `AppConfig`（TUI 配置）加字段：同时改 `config/catalog.rs::interface_catalog()`——
  否则 Rust 侧双向对账门禁变红。
- 改稀疏文档上的编辑（路径行走 / 目录 stub / 列表增删移 / 下标重排）：唯一实现是
  `shared/doc_edit.rs`——`wing config`（`Policy::Strict`，用法错误文案与 exit 4 都在那里）
  与设置面板（`Policy::Lenient`，按需创建、不报错）各自选策略消费，不许再写第二份。
- 改保存 / 生效语义：唯一写盘路径是 `runtime.apply_settings`，唯一热重载实现是 `system.reload_system`
  （其逐项名字序是对外契约，只许在末尾追加）；面板 / CLI / curl 三个入口都在它们之上，不许各自写第二份。
- 改 `SETUP_ALLOWED_PATHS`：它是"修复模式能做什么"的全部定义——加一个路径前先问"配置坏掉时它真的可用吗"。
- 改键位：`shared/panels/settings/mod.rs` 的 `handle_*_key` 是唯一实现（模块 doc 的表要与它同步改），
  `ui/settings/**` 只渲染；`r`/`R`/`Ctrl+R` 的三分是刻意设计（复位 / 重载 / 重启），别再合成一个键。
  **两栏共用的键**（`s` / `R` / `Ctrl+R` / `/` / `p` / `?` / `Tab` / `Esc`）在焦点分派**之前**处理，
  加新键时先想清楚它属于哪一栏。
- 改面板几何：`ui/settings/mod.rs` 的 `card_area`（浮层尺寸，**只有 App 与测试用**——setup 向导有自己的
  三块布局，把面板整块渲染进 `panel_area`，不是浮层）与 `Regions::new`（内部切分，三个入口共用）；
  `tree_viewport_rows` / `anchors_viewport_rows` / `has_anchor_column` 必须与实际画出来的东西一致
  （翻页步长与"左栏存不存在"），每帧喂回面板（`set_viewport_rows` / `set_anchor_viewport_rows` /
  `set_anchors_visible`），`the_viewport_contract_holds_at_every_size` 钉住这条。
- 排查用户现场：`wing config doctor`（不需要网关正常）→ `wing config list` → `~/.wing/core/logs/`
  的 `settings changed:` 行（只有路径，没有值）；两个 `.bak` 是最近一次保存前的原文。
