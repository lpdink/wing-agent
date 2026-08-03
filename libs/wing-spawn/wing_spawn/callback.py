"""Completion callback for wing-spawn async mode.

When a task finishes, the background watcher POSTs the result to the configured
callback URL so the parent agent can be notified without polling.
"""

from __future__ import annotations

import logging

import httpx

log = logging.getLogger("wing-spawn.callback")


async def notify(callback_url: str, task_id: str, status: str, result: str = "") -> None:
    """POST the task outcome to the callback URL (best-effort, non-fatal)."""
    if not callback_url:
        return
    payload = {
        "task_id": task_id,
        "status": status,
        "result": result,
    }
    try:
        async with httpx.AsyncClient(timeout=30.0) as client:
            resp = await client.post(callback_url, json=payload)
            resp.raise_for_status()
        log.info("callback delivered to %s (%s)", callback_url, resp.status_code)
    except Exception as e:  # noqa: BLE001
        log.warning("callback to %s failed: %s", callback_url, e)