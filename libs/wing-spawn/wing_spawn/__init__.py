"""wing-spawn — spawn a disposable tool container and run a goal against it.

`wing-spawn` is the parent-agent's tool for handing a task to a fresh,
ephemeral coding agent. It:

1. boots a throwaway tool container (`wing-devbox` image) that registers its
   standard tools to the gateway under a unique `client_id` namespace;
2. creates a gateway session bound to that container's tools (+ model);
3. sends the goal prompt and streams the resulting `turn_result` back;
4. tears the container down (unless `--keep`).

The container is a *pure executor*: no interpreter sits inside it, so the
parent agent survives the child's lifetime. Reusing the parent's network
stack means no extra network wiring is needed.

In `--async` mode the goal is submitted, a detached watcher waits for the
result, and the parent can observe progress via `wing-spawn status` /
`wing-spawn list` and be notified via a completion callback.
"""

from wing_spawn.containers import (
    cleanup_all,
    cleanup_container,
    spawn_tool_container,
    wait_for_tools,
)
from wing_spawn.runner import SpawnRunner

__all__ = [
    "SpawnRunner",
    "spawn_tool_container",
    "wait_for_tools",
    "cleanup_container",
    "cleanup_all",
]