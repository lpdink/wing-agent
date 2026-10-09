"""vulture 白名单 —— `make check-python` 里刻意保留的「未引用」符号。

门禁跑在 `--min-confidence 60`（`[tool.vulture]`，config 在 pyproject.toml）：
这一档会把「没人调用的函数 / 方法 / 属性 / 变量」全部报出来。能删的删掉
（这正是这档存在的意义），删不掉但确实活着的写在这里，并按理由分组。

维护提示：vulture 按**名字**匹配，这里的名字在整个扫描范围生效——把名字写
进来等于放弃对该名字的检查，所以每条都必须说清「谁在用」。新增条目前先问：
它真的有人用，还是我们只是懒得删？

注意：本文件是 vulture 的输入（`paths` 里显式列出），不参与 ruff / ty /
format（它们在 `libs/`、`scripts/` 范围内跑）。
"""

# ── wire / pydantic 模型的字段 ─────────────────────────────────────────
# Python 侧无人读：消费方是序列化之后的前端（Rust TUI / VSCode / SDK），
# 或 pydantic 的校验 / 序列化管线（如 `model_dump`）。vulture 看不到这些边。

# TUI / 前端消费的协议字段
api_url
workdir
skills_info
service
default_agent
# Setting API 的 setup_mode（get / status 两个响应模型的字段）：本期恒 False
# （网关只在配置合法时才活着），消费方是序列化之后的前端；04 让它变真。
setup_mode

# 链上事件与工具 schema 字段
created_at
error_code
tool_args
system_prompt_parts
multi_select

# audit 指标 entry：写入由 aggregate() 完成，读出经 model_dump
tokens_per_sec_total
tokens_per_sec_count

# ── 配置问题分类（wire 词表）──────────────────────────────────────────
# `ProblemKind` 是加载期 / 设置面板 / CLI 共用的分类词表（协议面）。
# `invalid_value` 的产出方是 03 的 `document.locate_problems`（字段级错误翻译）与
# 坏文档问题——它已被真实引用，**不再需要白名单**（名字进白名单 = 放弃对它的检查）。
# `unknown_key` 至今没有产出方：未知键是**前向兼容记录**（原样保留并写回），不是配置
# 错误，把它计入 problems 会让「新版本写的键 → 旧面板拒绝启动」（03 design.md
# Assumptions 8）。词表成员保留，白名单因此也保留。
UNKNOWN_KEY

# ── pytest 插件钩子 ────────────────────────────────────────────────────
# pytest 按名字调用，没有静态引用。
pytest_runtest_makereport
pytest_terminal_summary
pytest_sessionfinish
pytest_testnodedown

# ── 公开 API（仓外使用者）─────────────────────────────────────────────
# wing-sdk / wing-hooks 是发布给外部宿主与用户 hook 的包：这些符号在本仓库
# 内没有调用方，正是「给外部用」的意思。
send_message
get_commands
get_models
get_agents
register_standard_tools
register_workspace_env_inject
