# wing-spawn

Hand a goal to a fresh, disposable coding agent and get the result back.

`wing-spawn` is the tool a **parent agent** uses to dispatch work to a
throwaway *executor* container. It:

1. boots a disposable tool container (`wing-devbox` image) that registers its
   standard tools to the gateway under a unique `client_id` namespace;
2. creates a gateway session bound to that container's tools + model;
3. sends the goal prompt and streams the resulting `turn_result` back to stdout;
4. tears the container down (unless `--keep`).

The child is a *pure executor* — no interpreter sits inside it, so the parent
agent survives the child's lifetime. The child reuses the parent's network
stack and persistent workspace volume, so no extra wiring is needed.

## Usage

```bash
# Run a goal against a fresh container (auto-cleaned afterwards)
wing-spawn "Implement X and write tests"

# Long goal from a file, with a specific model
wing-spawn --task-file task.md --model deepseek-v4-flash-0731

# Keep the container around for inspection
wing-spawn --keep "Do something" --client-id my-task

# Clean up any leftover containers
wing-spawn cleanup
```

## Options

| Option | Description |
|--------|-------------|
| `prompt` / `--task-file` | Goal text (positional or from file) |
| `--gateway` | Gateway URL (default `$WING_GATEWAY_URL`) |
| `--api-key` | Admin key (default `$WING_ADMIN_KEY`) |
| `--tool-key` | Tool_runtime key for the container (default `$WING_TOOL_KEY`) |
| `--client-id` | Container namespace (default auto `task-<hex>`) |
| `--image` | Tool container image (default `$REGISTRY/wing-devbox:$TAG`) |
| `--workspace` | Workspace path (default `$WING_WORKSPACE`) |
| `--network` | Docker network (default: share the parent's stack) |
| `--model` | Executor model (default `$DEFAULT_MODEL`) |
| `--tools` | Comma-separated tool refs (default: `<client_id>.<standard tools>`) |
| `--append-system-prompt` | Extra text for the executor system prompt |
| `--timeout` | Seconds to wait for a result (default 1800) |
| `--keep` | Keep the container after the run |

## Exit codes

- `0` — goal completed
- `1` — error (gateway unreachable, tool registration failed, run errored)
- `130` — interrupted (SIGINT/SIGTERM)