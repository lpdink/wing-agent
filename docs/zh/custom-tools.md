# 自定义工具与 Hooks

wing-agent 提供两种扩展机制：**自定义工具**（给 agent 添加新能力）和 **Hooks**（拦截和修改现有行为）。

## 自定义工具

### 工具的工作原理

工具是通过 `@tool_registry.register` 装饰器注册到 `tool_registry` 的 Python 函数。agent 通过名称和指定参数调用它们。工具结果作为对话上下文返回给 agent。

### 编写自定义工具

```python
from wing.tool_registry import tool_registry
import httpx

@tool_registry.register(
    name="Weather",
    description="Get current weather for a city",
)
async def get_weather(city: str, unit: str = "celsius") -> str:
    """获取天气数据并返回可读的摘要。

    Args:
        city: 城市名 (如 "Beijing", "Tokyo")
        unit: 温度单位, "celsius" 或 "fahrenheit"
    """
    async with httpx.AsyncClient() as client:
        resp = await client.get(f"https://api.weather.com/{city}")
        data = resp.json()
        temp = data["temperature"]
        return f"Weather in {city}: {temp}°{'C' if unit == 'celsius' else 'F'}, {data['condition']}"
```

### 关键规则

1. **类型标注必须** — wing 从 Python 类型标注生成工具 schema。支持的类型：`str`、`int`、`float`、`bool`、`list[str]`、`Optional[...]`。
2. **Docstring 成为工具描述** — LLM 根据描述决定何时调用该工具。
3. **参数文档** — 在 docstring 的 `Args:` 部分描述每个参数。
4. **返回字符串** — 返回值作为工具结果发送给 agent。
5. **优先使用异步函数** — I/O 密集型工具使用 `async def`。

### 加载自定义工具

自定义工具通过 **hook 文件** 加载。当 wing 加载 hook 文件时（通过 `hooks` 配置），文件中的 `import` 语句会触发 `@tool_registry.register` 装饰器作为副作用执行。

创建一个 hook 文件来导入你的工具模块：

```python
# ~/.wing/hooks/my_tools.py
import my_custom_tools.weather  # 触发 @tool_registry.register
```

然后在配置中添加 hook 路径并引用工具名称：

```yaml
# ~/.wing/core/config.yaml
hooks:
  - "~/.wing/hooks/*.py"

agents:
  - name: default
    model: "gpt-4"
    tools: [Bash, Read, Write, Edit, Glob, Grep, Weather]
```

### 工具参数

你可以覆盖自动生成的参数元数据：

```python
from wing.schema import ToolParam

@tool_registry.register(
    name="Search",
    description="按内容搜索文件",
    params=[
        ToolParam(name="query", type="string", description="搜索查询"),
        ToolParam(name="path", type="string", description="搜索目录", default="."),
        ToolParam(name="limit", type="integer", description="最大结果数", default=20),
    ],
)
async def search_files(query: str, path: str = ".", limit: int = 20) -> str:
    ...
```

## Hooks

### 扩展点

Hooks 在定义的扩展点拦截 wing 的行为。每个扩展点有特定的签名：

| 扩展点 | 触发时机 | 签名 |
|--------|---------|------|
| `before_session_start` | Session 创建或加载时 | `(session, **ctx) -> None` |
| `before_user_message` | 处理用户消息前 | `(content: str \| None, **ctx) -> str \| None` |
| `before_tool_call` | 工具执行前 | `(tc: ToolCall \| None, **ctx) -> ToolCall \| None` |
| `after_tool_call` | 工具执行后 | `(result: str \| None, tool_name: str, **ctx) -> str \| None` |

Hook handler 组成管道：每个 handler 接收上一个 handler 的输出值。返回 `None` 保持当前值不变，返回新值则替换。

### 编写 Hook

在 `hooks` 配置的 glob 模式匹配的路径下创建文件：

```python
# ~/.wing/hooks/audit_tools.py
from wing.hook_registry import hooks

@hooks.on("after_tool_call")
def log_tool_results(result, tool_name=None, **ctx):
    """将每个工具结果记录到文件。"""
    import datetime
    with open("/tmp/wing_tool_audit.log", "a") as f:
        f.write(f"[{datetime.datetime.now()}] {tool_name}: {str(result)[:200]}\n")
    return result  # 始终返回结果（可以修改）
```

### 注册 Hooks

在后端配置中添加 glob 模式：

```yaml
# ~/.wing/core/config.yaml
hooks:
  - "~/.wing/hooks/*.py"
```

### 官方 Hooks（wing-hooks 包）

安装官方 hooks：

```bash
pip install wing-hooks
```

可用 hooks：
- **prefix_timestamp** — 给用户消息添加时间戳前缀（`before_user_message`）
- **truncate_tool_result** — 截断超长工具结果以节省上下文（`after_tool_call`）
- **audit_edit_write** — 记录所有文件编辑以供审查（`before_tool_call`）
- **workspace_env_inject** — 注入工作目录路径到环境变量（`before_session_start`）
