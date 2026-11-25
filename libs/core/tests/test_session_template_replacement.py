"""Template replacement with concurrent configuration changes over the gateway routes."""

from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import patch

import httpx
import pytest
import yaml

from wing.config import get_config_path, reset_config
from wing.gateway.server import GatewayServer
from wing_probe import FakeProvider, Script, Turn


@pytest.mark.asyncio
@pytest.mark.usefixtures("cleanup_event_bus")
@pytest.mark.parametrize("change", ["settings/set", "system/reload"])
@pytest.mark.parametrize("materialized", [False, True])
async def test_template_removal_during_shutdown_preserves_original_session(
    change: str, materialized: bool, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("WING_HOME", str(tmp_path))
    monkeypatch.setenv("WING_SESSIONS_PATH", str(tmp_path / "sessions"))
    monkeypatch.setenv("NO_PROXY", "127.0.0.1,localhost")
    path = get_config_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    fake = await FakeProvider().start()
    try:
        fake.register(
            "primary",
            Script(
                *([Turn.of(text="seed reply")] if materialized else []),
                Turn.of(text="old agent still works"),
            ),
        )
        fake.register("secondary", Script(Turn.of(text="replacement works")))
        original = {
            "providers": [
                {
                    "name": name,
                    "api_key": "test",
                    "base_url": fake.base_url,
                    "models": [name],
                }
                for name in ("primary", "secondary")
            ],
            "agents": [
                {"name": "default", "model": "primary", "default": True},
                {"name": "alternate", "model": "secondary"},
            ],
        }
        without_target = {key: items[:1] for key, items in original.items()}
        path.write_text(yaml.safe_dump(original))
        reset_config()
        server = GatewayServer()
        async with (
            server._app.router.lifespan_context(server._app),
            httpx.AsyncClient(
                transport=httpx.ASGITransport(app=server._app),
                base_url="http://127.0.0.1",
            ) as http,
        ):

            async def reconfigure(document: dict) -> None:
                if change == "settings/set":
                    current = await http.get("/api/settings/get")
                    applied = await http.post(
                        "/api/settings/set",
                        json={
                            "base": current.json()["fingerprint"],
                            "document": document,
                        },
                    )
                    assert (
                        applied.status_code == 200 and applied.json()["reload"]["ok"]
                    ), applied.text
                else:
                    path.write_text(yaml.safe_dump(document))
                    reloaded = await http.post("/api/system/reload")
                    assert reloaded.status_code == 200 and reloaded.json()["ok"], (
                        reloaded.text
                    )

            created = await http.post(
                "/api/session/create", json={"agent": {"effort": "high", "yolo": True}}
            )
            assert created.status_code == 200, created.text
            sid = created.json()["session_id"]
            session = server.runtime.get_session(sid)
            assert session is not None
            try:
                params = {"session_id": sid}
                assert (await http.get("/api/session/info", params=params)).json()[
                    "provider_name"
                ] == "primary"
                update = {**params, "agent": "alternate", "require_idle": True}
                late = {
                    **params,
                    "request_id": "late-request",
                    "content": "send after failure",
                }
                if materialized:
                    seed = await http.post(
                        "/api/session/send", json={**params, "content": "seed"}
                    )
                    assert seed.status_code == 200, seed.text
                    async with asyncio.timeout(5):
                        while session.last_turn_subtype != "success":
                            await asyncio.sleep(0.01)
                assert session.status == "idle"
                history = (await http.get("/api/session/get", params=params)).json()[
                    "messages"
                ]
                metadata = session.store.load_metadata(sid)
                assert (metadata is not None) is materialized
                old_agent = session.agent
                stop = old_agent._stop_worker
                reached, release = asyncio.Event(), asyncio.Event()

                async def hold_stop(*args, **kwargs):
                    reached.set()
                    await release.wait()
                    return await stop(*args, **kwargs)

                with patch.object(old_agent, "_stop_worker", side_effect=hold_stop):
                    async with asyncio.timeout(8), asyncio.TaskGroup() as group:
                        switching = group.create_task(
                            http.post("/api/session/update", json=update)
                        )
                        try:
                            await reached.wait()
                            refused = await http.post("/api/session/send", json=late)
                            assert refused.status_code == 409, refused.text
                            await reconfigure(without_target)
                        finally:
                            release.set()
                failed = switching.result()
                assert (
                    failed.status_code == 400 and "secondary" in failed.json()["detail"]
                ), failed.text
                info = await http.get("/api/session/info", params=params)
                assert info.status_code == 200, info.text
                assert (info.json()["template_name"], info.json()["status"]) == (
                    "default",
                    "idle",
                )
                assert session.agent is old_agent
                assert session.store.load_metadata(sid) == metadata
                assert (await http.get("/api/session/get", params=params)).json()[
                    "messages"
                ] == history

                send = await http.post("/api/session/send", json=late)
                assert send.status_code == 200, send.text
                async with asyncio.timeout(5):
                    while (
                        session.status != "idle"
                        or len(session.serialize_messages()) < len(history) + 2
                    ):
                        await asyncio.sleep(0.01)
                prior_to_retry = (
                    await http.get("/api/session/get", params=params)
                ).json()["messages"]
                assert prior_to_retry[: len(history)] == history
                assert prior_to_retry[-1]["content"] == "old agent still works"
                await reconfigure(original)
                retry = await http.post("/api/session/update", json=update)
                assert retry.status_code == 200, retry.text
                info = (await http.get("/api/session/info", params=params)).json()
                assert (info["template_name"], info["provider_name"]) == (
                    "alternate",
                    "secondary",
                )
                assert info["reasoning_effort"] is None
                record = session.store.load_metadata(sid)
                assert record is not None
                assert (record.template_name, record.model_id) == (
                    "alternate",
                    "secondary",
                )
                assert record.reasoning_effort is None and record.yolo is None
                assert (await http.get("/api/session/get", params=params)).json()[
                    "messages"
                ] == prior_to_retry
                sent = await http.post(
                    "/api/session/send", json={**params, "content": "after retry"}
                )
                assert sent.status_code == 200, sent.text
                async with asyncio.timeout(5):
                    while (
                        session.status != "idle"
                        or len(session.serialize_messages()) < len(history) + 4
                    ):
                        await asyncio.sleep(0.01)
                assert (
                    session.serialize_messages()[-1]["content"] == "replacement works"
                )
            finally:
                await session.aclose()
    finally:
        await fake.stop()
