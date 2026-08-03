# deploy — 生产部署（四容器）

```
  DingTalk ⇄ dingtalk ─┐
                       │ WS+HTTP (admin key)
  gateway ◄────────────┤          ┌── devbox（Rust/Go/Java/Py/Node/C++/gh/docker CLI）
  （wing-gateway）      │          │   远程工具宿主（tool_runtime key，挂宿主 docker.sock）
      │                │          └── workspace 卷（wing-agent clone，持久化）
      └── proxy（wing-ai-proxy，LLM 调用网关，SQLite 审计）
```

- 全部容器仅 compose 内网互通，**零端口对外**（钉钉走 Stream 出网）。
- 镜像由 CI（`.github/workflows/images.yml`）构建推阿里云；
  **基础镜像**（依赖层，tag = uv.lock 哈希）稳定少变，**应用镜像**只叠代码薄层。
  prod 上不 build，只 `pull + up`。

## 首部署（prod）

```bash
git clone https://github.com/lpdink/wing-agent.git /opt/wing-agent
cd /opt/wing-agent/deploy
cp .env.example .env            # 填入钉钉凭据/两把 gateway key/git 身份等
cp proxy/config.example.yaml proxy/config.yaml   # 填入上游 LLM key（与 PROXY_API_KEY 对应）
docker compose up -d
docker compose logs -f dingtalk
```

> gateway 的 config.yaml 由容器入口从 `.env` 渲染（`gateway/config.template.yaml`），
> 魔术命令目录为挂载卷 `wing-home` 的 `core/commands/`（可放入自定义 .md 命令）。

## 更新

push 到 `develop` 触发 CI 构建新镜像，然后：

```bash
cd /opt/wing-agent/deploy
git pull
docker compose pull
docker compose up -d
```

## 持久化

| 卷 | 内容 |
|---|---|
| `wing-home` | gateway 的 `$WING_HOME`（sessions、渲染后的 config、魔术命令） |
| `workspace` | devbox 工作仓库 + 构建产物（dingtalk 只读挂载，供 SendFile） |
| `dingtalk-state` | 钉钉会话 ↔ wing session 路由表 |
| `proxy-data` | wing-ai-proxy 审计 SQLite |

## 环境变量

见 `.env.example`。要点：

- `WING_ADMIN_KEY` / `WING_TOOL_KEY`：gateway auth 两把 key（admin = 钉钉前端，
  tool_runtime = devbox 工具宿主）。
- `PROXY_API_KEY` 必须与 `proxy/config.yaml` 的 `virtual_api_keys` 一致。
- `DINGTALK_ALLOWED_USERS`：白名单（staff_id 或昵称，逗号分隔）。

## wing-spawn（父 Agent 下发任务）

`wing-spawn`（`libs/wing-spawn`）是父 Agent 调用、把任务交给一个**一次性工具容器**
执行的 CLI。它复用 devbox 的 `wing-devbox` 镜像，在网关里注册一个独立 namespace
（`task-<hex>`）的工具宿主，建 session 下发 goal，并把 `turn_result` 回传，默认跑完
即删容器。

```bash
# 在 devbox 容器内（已挂 docker.sock + 环境变量）
wing-spawn "Implement X and write tests" --model deepseek-v4-flash-0731
wing-spawn --task-file task.md --keep          # 保留容器便于排查
wing-spawn cleanup                              # 清理残留容器
```

子容器与父容器**共享网络栈 + 持久 workspace 卷**（`_parent_mounts()` 探测复用），
因此无需额外网络/挂载配置。详见 `libs/wing-spawn/README.md`。
