# wing-agent: 通用 agent 运行时

Monorepo，包含 Python 后端（runtime + gateway + hooks）和 Rust 客户端（CLI + TUI）。

## 架构

```
wing (Rust CLI)          wing.gateway (Python)         wing (Python core)
┌──────────────┐    WS     ┌──────────────────┐        ┌──────────────┐
│ GatewayClient├──────────►│  GatewayServer   │───────►│ WingRuntime  │
│ (read/write  │           │  (EventBus 路由)  │        │ SessionManager│
│  task pair)  │◄──────────┤                  │◄───────┤ EventBus     │
└──────┬───────┘  events   └──────────────────┘        └──────────────┘
       │ mpsc
┌──────▼───────┐
│     App      │  handle_event() → state mutation
│  (状态机)    │  draw() → ratatui render
└──────────────┘
```

## 项目结构

```
libs/
├── core/wing/                    Agent 运行时 + Gateway（单包: wing-agent）
│   ├── agent.py                  WingAgent — LLM 循环 + tool dispatch
│   ├── session.py                Session — 消息历史 + 模板
│   ├── session_manager.py        SessionManager — 多 session 管理
│   ├── event_bus.py              EventBus — 全局事件总线（单例）
│   ├── runtime.py                WingRuntime — 统一入口
│   ├── config.py                 配置加载 + WING_HOME 管理
│   ├── context_manager.py        上下文管理 + compaction
│   ├── tool_registry.py          工具注册表
│   ├── tools/                    内置工具 (Bash, Read, Write, Edit, ...)
│   ├── event/                    事件类型 (react, state_change, query_response)
│   ├── magic_command/            Slash 命令系统
│   └── gateway/                  WebSocket 服务器 (server, cli, protocol)
└── wing_hooks/wing_hooks/        官方 hooks

crates/wing/src/                  Rust CLI + TUI
├── main.rs                       clap 子命令分发
├── cmd/                          CLI 子命令 (start/stop/status/tui) + daemon 管理
├── gateway/client.rs             GatewayClient — WS 连接
├── protocol/                     WingEvent + ClientRequest + ConnectResponse
├── app/mod.rs                    App 状态机
├── ui/                           UI 组件
├── render/                       渲染 (markdown, syntax highlighting)
├── tui/                          终端抽象
└── config/                       YAML 配置
```

## PyPI 分发

`pip install wing-agent` 安装 Python 包（core + gateway）。
CLI 入口点：`wing-gateway`。

Rust TUI (`wing`) 通过 GitHub Release 分发预编译二进制。

## 数据目录

所有持久化数据统一在 `~/.wing/`：

```
~/.wing/
├── config.yaml         用户配置
├── state.json          gateway daemon 状态
├── gateway.log         gateway 日志
├── logs/               TUI 日志
├── sessions/           session 持久化
├── templates/          agent 模板
└── metrics.json        LLM 调用指标
```

环境变量：`WING_HOME` 覆盖默认路径。

## 开发

```bash
# Python
uv sync
uv run wing-gateway           # 启动 gateway
make test                      # pytest
make check                     # ruff + ty + vulture

# Rust
cargo build
cargo test
make check-rust                # fmt + clippy + test
```

## 文档

- `docs/zh/` — 中文文档
- `docs/en/` — 英文文档
