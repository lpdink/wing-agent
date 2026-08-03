"""Task state persistence for wing-spawn async mode.

Each submitted task is recorded as a JSON file under a state directory so the
parent agent can poll `wing-spawn status <id>` / `wing-spawn list` and observe
progress. The background watcher updates the same file as it runs.
"""

from __future__ import annotations

import json
import os
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path

# Task lifecycle states.
RUNNING = "running"
COMPLETED = "completed"
FAILED = "failed"
CANCELLED = "cancelled"

DEFAULT_STATE_DIR = os.environ.get(
    "WING_SPAWN_STATE_DIR", os.path.expanduser("~/.wing/spawn")
)


@dataclass
class TaskState:
    task_id: str
    client_id: str
    status: str = RUNNING
    prompt: str = ""
    model: str = ""
    gateway_url: str = ""
    callback_url: str = ""
    session_id: str = ""
    created_at: float = field(default_factory=time.time)
    updated_at: float = field(default_factory=time.time)
    result: str = ""
    exit_code: int | None = None
    error: str = ""

    def touch(self) -> None:
        self.updated_at = time.time()


def state_dir() -> Path:
    d = Path(DEFAULT_STATE_DIR)
    d.mkdir(parents=True, exist_ok=True)
    return d


def state_path(task_id: str) -> Path:
    return state_dir() / f"{task_id}.json"


def save(state: TaskState) -> None:
    state.touch()
    state_path(state.task_id).write_text(
        json.dumps(asdict(state), ensure_ascii=False, indent=2),
        encoding="utf-8",
    )


def load(task_id: str) -> TaskState | None:
    p = state_path(task_id)
    if not p.exists():
        return None
    try:
        data = json.loads(p.read_text(encoding="utf-8"))
        return TaskState(**data)
    except Exception:
        return None


def list_tasks() -> list[TaskState]:
    tasks: list[TaskState] = []
    for p in state_dir().glob("*.json"):
        try:
            data = json.loads(p.read_text(encoding="utf-8"))
            tasks.append(TaskState(**data))
        except Exception:
            continue
    tasks.sort(key=lambda t: t.created_at, reverse=True)
    return tasks


def delete(task_id: str) -> bool:
    p = state_path(task_id)
    if p.exists():
        p.unlink()
        return True
    return False


def update_status(
    task_id: str, status: str, *, result: str = "", exit_code: int | None = None, error: str = ""
) -> TaskState | None:
    state = load(task_id)
    if state is None:
        return None
    state.status = status
    if result:
        state.result = result
    if exit_code is not None:
        state.exit_code = exit_code
    if error:
        state.error = error
    save(state)
    return state