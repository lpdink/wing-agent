"""tests/test_gateway_usage.py — ``GET /api/usage`` 的线格式契约。

用 ``TestClient``（真实 app + 真实 ``WingRuntime``）：这个端点的契约一半在模型上
（11 个字段的形状）、一半在状态码上（语义非法 400 / 未知会话 404 / 越界 422），
两者都只在 HTTP 边界上成立。

数据播种走生产写入路径（HTTP 建会话 + ``AuditRecorder`` 事件），断言只钉**相对
关系与契约形状**，不钉具体 token 数。
"""

from __future__ import annotations

from collections.abc import Iterator
from datetime import date, timedelta
from pathlib import Path
from typing import Any

import pytest
from fastapi.testclient import TestClient

from wing.db import get_database
from wing.metrics import MEASURE_FIELDS

from _support import (
    gateway_server,
    local_time,
    seed_llm_call,
    seed_tool_call,
    seed_user_message,
)

TODAY = date.today()
YESTERDAY = TODAY - timedelta(days=1)
SINCE = (TODAY - timedelta(days=6)).isoformat()
UNTIL = TODAY.isoformat()

#: 线格式里 Row 的「非度量」键（其余键必须正好是 11 个度量）。
ROW_EXTRA_KEYS = {"key", "label", "parent_session_id"}


class TestUsageEndpoint:
    @pytest.fixture
    def client(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> Iterator[TestClient]:
        """真实 app + 真实 runtime（鉴权关闭；sessions 根独立）。"""
        with gateway_server(tmp_path, monkeypatch) as server:
            with TestClient(server._app) as test_client:
                yield test_client

    def _create(self, client: TestClient, **body: Any) -> dict[str, Any]:
        response = client.post("/api/session/create", json=body)
        assert response.status_code == 200, response.text
        return response.json()

    def test_response_shape(self, client: TestClient) -> None:
        """200 的形状：range / group_by / totals / rows / truncated，度量一个不少。"""
        self._create(client, session_id="shape-1")
        seed_llm_call("shape-1", local_time(YESTERDAY))
        seed_tool_call("shape-1", local_time(YESTERDAY))
        seed_user_message("shape-1", local_time(TODAY))

        body = client.get("/api/usage", params={"since": SINCE, "until": UNTIL}).json()

        # 窗口三键：`data_since` = 审计流水最早的一天（本次播种里最早的那条）。
        assert body["range"] == {
            "since": SINCE,
            "until": UNTIL,
            "data_since": YESTERDAY.isoformat(),
        }
        assert body["group_by"] == "day"
        assert body["truncated"] is False
        assert set(body["totals"]) == set(MEASURE_FIELDS)
        # day 稠密：窗口内每天都有一行，且每行都带齐 11 个度量
        assert [row["key"] for row in body["rows"]] == [
            (date.fromisoformat(SINCE) + timedelta(days=offset)).isoformat()
            for offset in range(7)
        ]
        for row in body["rows"]:
            assert set(row) == set(MEASURE_FIELDS) | ROW_EXTRA_KEYS
        assert body["totals"]["llm_calls"] == 1
        assert body["totals"]["tool_calls"] == 1
        assert body["totals"]["user_messages"] == 1
        assert body["totals"]["sessions_created"] == 1
        assert body["totals"]["prompt_tokens"] == 100

    def test_origin_rows_are_fixed_and_grouped(self, client: TestClient) -> None:
        self._create(client, session_id="root-1")
        self._create(client, session_id="child-1", parent_session_id="root-1")
        seed_llm_call("root-1", local_time(YESTERDAY), prompt=10)
        seed_llm_call("child-1", local_time(YESTERDAY), prompt=90)

        body = client.get(
            "/api/usage",
            params={"since": SINCE, "until": UNTIL, "group_by": "origin"},
        ).json()

        assert [row["key"] for row in body["rows"]] == ["me", "agents"]
        assert body["rows"][0]["prompt_tokens"] == 10
        assert body["rows"][1]["prompt_tokens"] == 90
        assert body["totals"]["prompt_tokens"] == 100

    def test_session_scope_with_tree(self, client: TestClient) -> None:
        self._create(client, session_id="root-1")
        self._create(client, session_id="child-1", parent_session_id="root-1")
        seed_llm_call("root-1", local_time(YESTERDAY), prompt=10)
        seed_llm_call("child-1", local_time(YESTERDAY), prompt=90)

        alone = client.get(
            "/api/usage",
            params={
                "since": SINCE,
                "until": UNTIL,
                "group_by": "session",
                "session_id": "root-1",
            },
        ).json()
        tree = client.get(
            "/api/usage",
            params={
                "since": SINCE,
                "until": UNTIL,
                "group_by": "session",
                "session_id": "root-1",
                "tree": True,
            },
        ).json()

        assert [row["key"] for row in alone["rows"]] == ["root-1"]
        assert {row["key"] for row in tree["rows"]} == {"root-1", "child-1"}
        assert tree["totals"]["prompt_tokens"] == 100
        assert tree["rows"][0]["parent_session_id"] in (None, "root-1")

    def test_unknown_session_is_404(self, client: TestClient) -> None:
        """未知会话不能静默给全 0——0 是一个读数。"""
        response = client.get("/api/usage", params={"session_id": "nope-1"})

        assert response.status_code == 404
        body = response.json()
        assert body["error"] == "not_found"
        assert "nope-1" in body["detail"]

    def test_tree_without_session_is_400(self, client: TestClient) -> None:
        response = client.get("/api/usage", params={"tree": "true"})

        assert response.status_code == 400
        assert response.json()["error"] == "bad_request"
        assert "tree requires session_id" in response.json()["detail"]

    def test_unknown_group_by_is_400(self, client: TestClient) -> None:
        response = client.get("/api/usage", params={"group_by": "week"})

        assert response.status_code == 400
        assert "unknown group_by" in response.json()["detail"]

    def test_bad_date_is_400(self, client: TestClient) -> None:
        response = client.get("/api/usage", params={"since": "2026-13-40"})

        assert response.status_code == 400
        assert "invalid since" in response.json()["detail"]

    def test_since_after_until_is_400(self, client: TestClient) -> None:
        response = client.get(
            "/api/usage", params={"since": "2026-03-05", "until": "2026-03-01"}
        )

        assert response.status_code == 400
        assert "is after until" in response.json()["detail"]

    @pytest.mark.parametrize("limit", ["0", "201", "-1"])
    def test_limit_out_of_range_is_422(self, client: TestClient, limit: str) -> None:
        """形态越界交给 FastAPI（422）——与「语义非法 400」是两条路。"""
        response = client.get("/api/usage", params={"limit": limit})

        assert response.status_code == 422
        assert response.json()["error"] == "validation_error"

    def test_since_all_spans_the_whole_history(self, client: TestClient) -> None:
        early = TODAY - timedelta(days=30)
        self._create(client, session_id="old-1")
        seed_llm_call("old-1", local_time(early))

        body = client.get(
            "/api/usage", params={"since": "all", "until": UNTIL, "group_by": "session"}
        ).json()

        # 目录行的创建时刻是「现在」，所以最早的一天可能来自审计行（30 天前）
        assert body["range"]["since"] == early.isoformat()
        assert body["range"]["until"] == UNTIL
        assert body["totals"]["llm_calls"] == 1

    def test_truncated_when_limit_cuts_rows(self, client: TestClient) -> None:
        for index in range(3):
            session_id = f"trunc-{index}"
            self._create(client, session_id=session_id)
            seed_llm_call(session_id, local_time(YESTERDAY), prompt=10 * (index + 1))

        body = client.get(
            "/api/usage",
            params={"since": SINCE, "until": UNTIL, "group_by": "session", "limit": 2},
        ).json()

        assert len(body["rows"]) == 2
        assert body["truncated"] is True
        # totals 仍是全量真值（行和 ≠ totals 是截断的正常表现）
        assert body["totals"]["sessions"] == 3
        assert body["totals"]["prompt_tokens"] == 60

    def test_empty_window_is_200_with_zeros(self, client: TestClient) -> None:
        """空数据永远 200（不是 404、也不是 500）。"""
        long_ago = TODAY - timedelta(days=400)
        body = client.get(
            "/api/usage",
            params={
                "since": long_ago.isoformat(),
                "until": (long_ago + timedelta(days=1)).isoformat(),
            },
        ).json()

        assert len(body["rows"]) == 2
        assert all(body["totals"][name] == 0 for name in MEASURE_FIELDS)
        assert body["range"]["data_since"] is None, "没有审计行 → null（不是空串）"

    def test_scoped_request_with_degraded_database_stays_200(
        self,
        client: TestClient,
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        """库降级：带 `session_id` 也照常 200（读不到 ≠ 不存在）。"""
        from wing.db import Database

        blocked = tmp_path / "degraded.db"
        blocked.mkdir()
        degraded = Database(blocked)
        assert not degraded.ready
        monkeypatch.setattr("wing.metrics.store.get_database", lambda: degraded)

        response = client.get(
            "/api/usage", params={"session_id": "shape-1", "group_by": "session"}
        )

        assert response.status_code == 200
        assert [row["key"] for row in response.json()["rows"]] == ["shape-1"]

    def test_create_writes_the_session_catalog(self, client: TestClient) -> None:
        """建会话 → 目录行（创建时刻 / workspace / 模板 / 模型引用词）。"""
        created = self._create(
            client, session_id="cat-1", workspace="/proj/x", tags=["usage-probe"]
        )
        assert created["session_id"] == "cat-1"

        rows = get_database().query("SELECT * FROM session_catalog")
        assert len(rows) == 1
        row = rows[0]
        assert row["session_id"] == "cat-1"
        assert row["created_source"] == "create"
        assert row["workspace"] == "/proj/x"
        assert row["template_name"] is not None

    def test_adoption_does_not_rewrite_the_catalog_row(
        self, client: TestClient
    ) -> None:
        """create-or-adopt 命中既有会话 = resume 语义：目录行是创建时的事实。"""
        self._create(client, session_id="cat-2", workspace="/proj/first")
        before = dict(get_database().query("SELECT * FROM session_catalog")[0])
        client.post("/api/session/release", json={"session_id": "cat-2"})

        self._create(client, session_id="cat-2", workspace="/proj/second")

        after = dict(get_database().query("SELECT * FROM session_catalog")[0])
        assert after == before

    def test_fork_writes_a_catalog_row(self, client: TestClient) -> None:
        """fork 也是新建会话：写一行（``forked_from`` 是内容血统，父血缘不写）。"""
        self._create(client, session_id="src-1")
        seed_llm_call("src-1", local_time(YESTERDAY))
        branches = client.get("/api/session/branches", params={"session_id": "src-1"})
        assert branches.status_code == 200
        uuid = branches.json()["targets"][0]["uuid"]

        forked = client.post(
            "/api/session/fork",
            json={"source_session_id": "src-1", "target_uuid": uuid},
        )
        assert forked.status_code == 200, forked.text
        child = forked.json()["session_id"]

        rows = {
            row["session_id"]: row
            for row in get_database().query("SELECT * FROM session_catalog")
        }
        assert rows[child]["forked_from"] == "src-1"
        assert rows[child]["parent_session_id"] is None

    def test_usage_does_not_hydrate_the_session(self, client: TestClient) -> None:
        """只读库：调端点不把已逐出的会话读回内存（``release`` 之后仍 inactive）。"""
        self._create(client, session_id="cold-1", tags=["usage-probe"])
        seed_llm_call("cold-1", local_time(YESTERDAY))
        assert (
            client.post(
                "/api/session/release", json={"session_id": "cold-1"}
            ).status_code
            == 200
        )

        assert (
            client.get(
                "/api/usage", params={"session_id": "cold-1", "group_by": "session"}
            ).status_code
            == 200
        )

        listed = client.get("/api/session/list").json()["sessions"]
        assert [row["status"] for row in listed if row["id"] == "cold-1"] == [
            "inactive"
        ]
