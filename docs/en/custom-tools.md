# Custom Tools & Hooks

wing-agent provides two extension mechanisms: **custom tools** (give the agent new capabilities) and **Hooks** (intercept and modify existing behavior).

## Custom Tools

### How Tools Work

Tools are Python functions registered with the `tool_registry` via the `@tool_registry.register` decorator. The agent calls them by name with the specified parameters. Tool results are returned to the agent as conversation context.

### Writing a Custom Tool

```python
from wing.tool_registry import tool_registry
import httpx

@tool_registry.register(
    name="Weather",
    description="Get current weather for a city",
)
async def get_weather(city: str, unit: str = "celsius") -> str:
    """Fetch weather data and return a human-readable summary.

    Args:
        city: City name (e.g. "Beijing", "Tokyo")
        unit: Temperature unit, "celsius" or "fahrenheit"
    """
    async with httpx.AsyncClient() as client:
        resp = await client.get(f"https://api.weather.com/{city}")
        data = resp.json()
        temp = data["temperature"]
        return f"Weather in {city}: {temp}°{'C' if unit == 'celsius' else 'F'}, {data['condition']}"
```

### Key Rules

1. **Type hints are required** — wing generates the tool schema from Python type hints. Supported types: `str`, `int`, `float`, `bool`, `list[str]`, `Optional[...]`.
2. **Docstring becomes the tool description** — the LLM uses this to decide when to call the tool.
3. **Parameter docstrings** — use `Args:` section in the docstring for per-parameter descriptions.
4. **Return a string** — the return value is sent back to the agent as the tool result.
5. **Async functions preferred** — use `async def` for I/O-bound tools.

### Loading Custom Tools

Custom tools are discovered through **hook files**. When wing loads hook files (via the `hooks` config), any `import` statements in those files trigger `@tool_registry.register` decorators as a side effect.

Create a hook file that imports your tool module:

```python
# ~/.wing/hooks/my_tools.py
import my_custom_tools.weather  # triggers @tool_registry.register
```

Then add it to your config and reference the tool name:

```yaml
# ~/.wing/core/config.yaml
hooks:
  - "~/.wing/hooks/*.py"

agents:
  - name: default
    model: "gpt-4"
    tools: [Bash, Read, Write, Edit, Glob, Grep, Weather]
```

### Tool Parameters

You can override auto-generated parameter metadata:

```python
from wing.schema import ToolParam

@tool_registry.register(
    name="Search",
    description="Search files by content",
    params=[
        ToolParam(name="query", type="string", description="Search query"),
        ToolParam(name="path", type="string", description="Directory to search", default="."),
        ToolParam(name="limit", type="integer", description="Max results", default=20),
    ],
)
async def search_files(query: str, path: str = ".", limit: int = 20) -> str:
    ...
```

### Tool Namespaces & LLM Names (optional)

`@tool_registry.register` also accepts two optional identity fields:

```python
@tool_registry.register(
    name="Bash",            # registry key (used in config + lookup)
    namespace="client-a",   # group by source (built-ins live in "default")
    llm_name="Shell",       # name the LLM sees (defaults to `name`)
)
async def run(cmd: str) -> str:
    ...
```

- **`namespace`** groups tools by source so same-named tools from different origins (e.g. multiple remote clients each registering a `Bash`) can coexist. A tool is referenced as `"namespace.name"` (e.g. `"client-a.Bash"`); a bare name like `"Bash"` resolves to the `default` namespace, so existing configs are unaffected.
- **`llm_name`** decouples the registry key from what the model sees in its function-calling schema and calls back with. The agent dispatches by the effective LLM name; binding two tools that claim the same LLM name into one agent raises an error.

Most custom tools don't need either field — they exist to support multi-source / remote tool registration.

## Hooks

### Extension Points

Hooks intercept wing's behavior at defined points. Each point has a specific signature:

| Hook Point | When | Signature |
|------------|------|-----------|
| `before_session_start` | When a session is created or loaded | `(session, **ctx) -> None` |
| `before_user_message` | Before each user message is processed | `(content: str \| None, **ctx) -> str \| None` |
| `before_tool_call` | Before tool execution | `(tc: ToolCall \| None, **ctx) -> ToolCall \| None` |
| `after_tool_call` | After tool execution | `(result: str \| None, tool_name: str, **ctx) -> str \| None` |

Hook handlers form a pipeline: each handler receives the value from the previous one. Return `None` to keep the current value unchanged, or return a new value to replace it.

### Writing a Hook

Create a file matched by your `hooks` config glob pattern:

```python
# ~/.wing/hooks/audit_tools.py
from wing.hook_registry import hooks

@hooks.on("after_tool_call")
def log_tool_results(result, tool_name=None, **ctx):
    """Log every tool result to a file."""
    import datetime
    with open("/tmp/wing_tool_audit.log", "a") as f:
        f.write(f"[{datetime.datetime.now()}] {tool_name}: {str(result)[:200]}\n")
    return result  # always return the result (possibly modified)
```

### Registering Hooks

Add glob patterns in your backend config:

```yaml
# ~/.wing/core/config.yaml
hooks:
  - "~/.wing/hooks/*.py"
```

### Built-in Hooks (wing-hooks package)

Install official hooks:

```bash
pip install wing-hooks
```

Available hooks:
- **prefix_timestamp** — Prepend timestamps to user messages (`before_user_message`)
- **truncate_tool_result** — Truncate very long tool results to save context (`after_tool_call`)
- **audit_edit_write** — Log all file edits for review (`before_tool_call`)
- **workspace_env_inject** — Inject workspace path into environment (`before_session_start`)
