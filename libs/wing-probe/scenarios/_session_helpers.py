"""逐出场景的公共 HTTP 准备。"""

from __future__ import annotations

import asyncio
import time

from wing_probe import Probe


async def evict(probe: Probe, session_id: str) -> None:
    """退订当前 driver，再验证显式逐出的完整回执。"""
    driver = probe.driver_required
    await driver.http.unsubscribe(session_id, driver.client_id)
    payload = await driver.http.release_session(session_id)
    assert payload == {"ok": True, "released": True, "detail": "released"}, payload


async def evict_and_wait_inactive(probe: Probe, session_id: str) -> None:
    """退订 → release → 等 ``inactive``（磁盘成为唯一事实来源）。"""
    await evict(probe, session_id)
    http = probe.driver_required.http
    deadline = time.monotonic() + 20.0
    observed: str | None = None
    while time.monotonic() < deadline:
        listing = await http.list_sessions()
        for entry in listing.get("sessions", []):
            if entry.get("id") == session_id:
                observed = entry.get("status")
        if observed == "inactive":
            return
        await asyncio.sleep(0.2)
    raise AssertionError(f"session {session_id} stayed in status {observed!r}")
