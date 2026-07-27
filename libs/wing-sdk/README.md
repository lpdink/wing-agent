# wing-sdk

Wing Gateway SDK —— 远程工具注册与调用服务。

通过 WebSocket + HTTP 把本地函数注册为 Gateway 上的远程工具：
tool host 先建立 WS 连接（声明 `client_id`），再经 `/api/tools/register`
注册工具 schema，之后在 WS 上服务 `tool_call_request` 帧。

## 用法

### 装饰器注册

```python
import asyncio
from wing_sdk import ToolHost

host = ToolHost(client_id="my-host", gateway_url="http://127.0.0.1:32523")

@host.tool(name="Bash")
async def bash(command: str, timeout: int = 30) -> str:
    """Execute a shell command.

    Args:
        command: The command to execute.
        timeout: Maximum wait time in seconds.
    """
    ...  # 实现

asyncio.run(host.run())
```

参数 schema 从函数签名（名称、类型、默认值）与 Google-style docstring
的 Args section 推断；显式 `params=[ToolParam(...)]` 可覆盖。

### 标准工具

内置与核心内置工具同构的六个标准工具（Bash/Read/Write/Edit/Glob/Grep），
绑定 workspace 一键注册：

```python
from wing_sdk import ToolHost
from wing_sdk.tools import register_standard_tools

host = ToolHost(client_id="my-host")
register_standard_tools(host, workspace="/path/to/workdir")
```

路径类工具（Read/Write/Edit/Glob/Grep）受 workspace 约束（realpath 校验，
拒绝逃逸）；**这不是安全边界**——Bash 工具不受限，可访问宿主任意资源。
远程工具没有审批路径，只应在受信任的 Gateway 上运行。

### GatewayClient

`wing_sdk.http_client.GatewayClient` 覆盖 Gateway 会话与系统端点
（工具注册由 ToolHost 内部处理）：

```python
from wing_sdk.http_client import GatewayClient

async with GatewayClient("http://127.0.0.1:32523", api_key="secret") as client:
    session = await client.create_session(workspace="/path")
    await client.send_message(session["session_id"], "hello")
```

## 鉴权

Gateway 开启鉴权时，`api_key` 经 `Authorization: Bearer` 传递。
纯工具宿主可用 `tool_runtime` 角色 key；需要创建 session / 订阅事件的
客户端（如 wing-orch）必须用 `admin` 角色 key。

## 依赖

- `websockets>=13.0`（`connect()` 的 `additional_headers` 为 13+ API）
- `httpx>=0.27`

WS 消息上限放宽到 16MB（与 uvicorn 默认 `ws_max_size` 对齐），
大文件 Write / 大输出 Read 不受默认 1MB 限制。
