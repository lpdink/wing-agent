# experimental 命名域：行锚编辑（hashline 实验）

`libs/core/wing/tools/experimental/` 是实验性工具的落点（注册命名域 `experimental`），
当前内容是一套借鉴 [oh-my-pi](https://github.com/can1357/oh-my-pi) hashline 格式的
行锚编辑，两个工具：

| 注册名（引用） | LLM 可见名 | 作用 |
|---|---|---|
| `experimental.Read` | `HashlineRead` | 读文件：`[hashline: PATH#TAG \| ...]` 头部 + `N→text` 行号行 |
| `experimental.Edit` | `HashlineEdit` | 写文件：`[path#TAG]` 段 + `PUT/CUT/REM/MV` 操作 |

核心设计前提：**TAG 与内容一起到达模型**——模型只能引用它真正看过的版本；应用前
与磁盘现状对账，文件变过即拒绝（不按过期认知写文件）。这也是实验域自带读工具、
而不是改主 `Read` 的原因。

## 启用

注册随 `import wing.tools` 发生（所有内置工具同一条安装链），但默认不在任何 agent
的工具集里。会话经 `agents[].tools` 或 `POST /api/session/update` 的 `tools` 显式
启用，引用形如 `experimental.Read` / `experimental.Edit`；LLM 可见名与主
`Read` / `Edit` 不撞名，两套可以并存于同一会话。

## 格式

```text
*** Begin Patch               ← 包装可选（裸段序列也收）
[notes.txt#A1B2]              ← 段头：path + 4 位十六进制 tag（来自 HashlineRead）
PUT 2.=2:                     ← 原文第 2 行换成下面的 + 行
+BETA
CUT 1.=1 @keep                ← 删除第 1 行并捕获到会话级命名寄存器
PUT >$ @keep                  ← 文件尾粘贴寄存器
MV renamed.txt                ← 段内编辑完成后移动
*** End Patch
```

操作面：`PUT N.=M:`（范围替换）、`PUT <N:` / `PUT >N:` / `PUT >$:`（间隙插入）、
`CUT N.=M [@name]`（删除并捕获）、寄存器粘贴（间隙可匿名；范围必须 `@name`）、
`REM`（删文件，独占一段）、`MV DEST`（拒绝已存在目标、同文件、载荷内冲突与不存在
的目标目录；纯移动走 rename——保留 inode / 权限位 / 行尾字节，源不可写也可移动；
带内容变更的移动要求源可写、写新并复制源权限位）。行号一律相对 read 时的原文快照，与同载荷内先前操作无关；同一路径
（按 realpath 归一的真实身份——符号链接 / `..` 别名不绕过）的重复段必须连续、归并
为一组按载荷顺序应用（交错写法显式报错）；只有段头、零操作的段报错。

v1 明确不支持块定位（`N*` / `>N*`）——上游靠 tree-sitter 解析语法块，本实验没有
语法解析依赖，遇到即显式报错（改用显式行范围）。

## 语义要点（与主工具对齐的取舍）

- **tag**：LF 规范化后 sha256 前 4 位十六进制（大写）；行尾风格不属于内容同一性。
- **行尾**：读写两侧 CRLF/CR 归一化为 LF；产生净变化的补丁按 LF 写回（无净变化的
  补丁不写回、回执标 `no change`、不发 diff 事件）——与主 Edit 文本模式的行为一致
  （主工具经 Python 文本模式隐式完成了同样的归一化）。
- **符号链接**：按写穿政策作用于真实目标（与 Write/Edit 相同）；`REM`/`MV` 保留
  链接本身（可能悬空）。
- **寄存器**：命名寄存器会话级（进程内、不落盘，网关重启即失忆）；匿名寄存器
  载荷级。载荷内前序段捕获对后续段可见（"top-to-bottom across sections"）。寄存
  表有两级水位：跟踪会话数 ≤ 128（按首次使用先后 FIFO 淘汰最老条目）、每会话命名
  寄存器 ≤ 64（超限显式报错，不静默淘汰）。
- **写入事务**：校验（存在性 / tag / 应用可行性 / MV 目标）全部通过后才落盘；
  落盘中途失败对已写段尽力回滚（回滚失败照实上报）、寄存器不提交、零事件。
- **事件**：产生净变化的文件按操作逐条发窗口化 `diff_content`（persist 事实事件），
  `tool_call_id` 关联本次调用；MV 的窗口仍锚源路径。
- **回执**：按文件（归并后）给出新 TAG——后续补丁可直接引用，不必重读。

## 测试

- 单测：`libs/core/tests/test_experimental_hashline.py`（解析 / 应用 / 窗口纯逻辑）、
  `libs/core/tests/test_experimental_tools.py`（注册身份 / 工具层契约 / 寄存器 /
  回滚 / CRLF / 符号链接）。
- 整机：`libs/wing-probe/scenarios/test_experimental_edit.py`——真网关 + 假 Provider
  走公开协议，覆盖 read → edit → stale 拒绝 → 重读 → CUT 寄存器 → 粘贴 + MV 全链路，
  并锁定 TAG 契约（场景侧独立实现 4 位指纹）。
