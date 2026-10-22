"""文件上传（``POST /api/session/files``）单元测试：闸门 / 会话解析 / 路由映射。

各层各测自己那一层的契约：

- ``SessionManager.upload_file``：会话解析（**不水合**）+ 落盘——名字 / 非空 /
  ``files.max_bytes`` 三道闸门（全部在落盘之前，失败零副作用）、同名同字节幂等走
  store、别名拼写落**磁盘真名**目录、幽灵会话 404、归档会话让
  ``SessionArchivedError`` 逃逸；
- 路由：query / 原始字节体 → 200/400/404 映射与响应形状（真实 ``WingRuntime`` +
  ``TestClient``——mock 掉 runtime 就只剩路由胶水，证明不了"字节真的进了会话目录"），
  以及**收包顺序**（会话存在性 → 名字闸门 → ``Content-Length`` 预检 → 收包，用 ASGI
  读包计数器取证）。

整机链路（上传 → agent 用 Read 读绝对路径 → 内容 == 上传字节）由 probe 场景
``libs/wing-probe/scenarios/test_session_files_upload.py`` 钉住。
"""

from __future__ import annotations

from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from _support import BodyWatcher, gateway_server, sessions_root
from wing.common.utils import InvalidInputError
from wing.config import get_config
from wing.runtime import WingRuntime
from wing.schema import Message
from wing.store.base import FILE_NAME_MAX_BYTES

SID = "files-upload-session"


# ── fixtures（与媒体上传单测同款：真实 runtime，独立 sessions 根） ──


def files_dir(session_id: str) -> Path:
    return sessions_root() / session_id / "files"


def _seed_session(isolated_runtime: WingRuntime, session_id: str = SID) -> str:
    """建一个"是会话"的 id（有 metadata / history 才可被解析）。"""
    session = isolated_runtime.create_session(session_id=session_id)
    session.context_manager.add_message(Message(role="user", content="hi"))
    return session.session_id


# ── 1. manager 层 ──────────────────────────────────────────────


class TestManagerUploadFile:
    @pytest.mark.asyncio
    async def test_writes_file_and_returns_absolute_path(
        self, isolated_runtime: WingRuntime
    ) -> None:
        sid = _seed_session(isolated_runtime)

        ref = isolated_runtime.upload_file(sid, b"hello", name="notes.txt")

        assert ref.name == "notes.txt"
        assert ref.bytes == 5
        assert Path(str(ref.path)).is_absolute()
        assert ref.path == str(files_dir(sid) / "notes.txt")
        assert Path(str(ref.path)).read_bytes() == b"hello"

    @pytest.mark.asyncio
    async def test_second_upload_of_same_bytes_keeps_the_same_path(
        self, isolated_runtime: WingRuntime
    ) -> None:
        sid = _seed_session(isolated_runtime)
        first = isolated_runtime.upload_file(sid, b"same", name="notes.txt")
        before = Path(str(first.path)).stat()

        second = isolated_runtime.upload_file(sid, b"same", name="notes.txt")

        assert second == first
        after = Path(str(second.path)).stat()
        assert (after.st_ino, after.st_mtime_ns) == (
            before.st_ino,
            before.st_mtime_ns,
        ), "幂等命中重写了文件"

    @pytest.mark.asyncio
    async def test_conflicting_bytes_get_a_suffix(
        self, isolated_runtime: WingRuntime
    ) -> None:
        sid = _seed_session(isolated_runtime)
        isolated_runtime.upload_file(sid, b"one", name="report.pdf")
        second = isolated_runtime.upload_file(sid, b"two", name="report.pdf")
        assert second.name == "report-2.pdf"
        assert sorted(p.name for p in files_dir(sid).iterdir()) == [
            "report-2.pdf",
            "report.pdf",
        ]

    @pytest.mark.asyncio
    @pytest.mark.parametrize(
        "name",
        ["", ".", ".hidden", "../escape", "a/b", "a\\b", "x..y", "a\nb", "a" * 129],
        ids=[
            "empty",
            "dot",
            "leading-dot",
            "parent-ref",
            "slash",
            "backslash",
            "embedded-dotdot",
            "newline",
            "too-long",
        ],
    )
    async def test_invalid_names_are_rejected_without_touching_storage(
        self, isolated_runtime: WingRuntime, name: str
    ) -> None:
        sid = _seed_session(isolated_runtime)

        with pytest.raises(InvalidInputError):
            isolated_runtime.upload_file(sid, b"x", name=name)

        assert not files_dir(sid).exists(), "400 路径在存储上留下了痕迹"

    @pytest.mark.asyncio
    async def test_empty_body_is_rejected_without_touching_storage(
        self, isolated_runtime: WingRuntime
    ) -> None:
        sid = _seed_session(isolated_runtime)
        with pytest.raises(InvalidInputError):
            isolated_runtime.upload_file(sid, b"", name="notes.txt")
        assert not files_dir(sid).exists()

    @pytest.mark.asyncio
    async def test_oversize_body_is_rejected_against_config_limit(
        self, isolated_runtime: WingRuntime, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        sid = _seed_session(isolated_runtime)
        monkeypatch.setattr(get_config().files, "max_bytes", 4)

        with pytest.raises(InvalidInputError) as excinfo:
            isolated_runtime.upload_file(sid, b"12345", name="notes.txt")

        assert "files.max_bytes" in str(excinfo.value)
        assert not files_dir(sid).exists()

    @pytest.mark.asyncio
    async def test_exactly_at_the_limit_is_accepted(
        self, isolated_runtime: WingRuntime, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        sid = _seed_session(isolated_runtime)
        monkeypatch.setattr(get_config().files, "max_bytes", 4)
        assert isolated_runtime.upload_file(sid, b"1234", name="notes.txt").bytes == 4

    @pytest.mark.asyncio
    async def test_ghost_session_is_not_created(
        self, isolated_runtime: WingRuntime
    ) -> None:
        with pytest.raises(LookupError):
            isolated_runtime.upload_file("ghost-files-1", b"x", name="notes.txt")
        assert not (sessions_root() / "ghost-files-1").exists()

    @pytest.mark.asyncio
    async def test_traversal_session_id_is_not_created(
        self, isolated_runtime: WingRuntime
    ) -> None:
        with pytest.raises(LookupError):
            isolated_runtime.upload_file("../escape-files", b"x", name="notes.txt")
        assert not (sessions_root().parent / "escape-files").exists()

    @pytest.mark.asyncio
    async def test_memory_backend_has_no_path(
        self, isolated_runtime: WingRuntime
    ) -> None:
        session = isolated_runtime.create_session(backend="memory")
        session.context_manager.add_message(Message(role="user", content="hi"))

        ref = isolated_runtime.upload_file(
            session.session_id, b"data", name="notes.txt"
        )

        assert ref.path is None
        assert ref.name == "notes.txt" and ref.bytes == 4

    @pytest.mark.asyncio
    async def test_upload_does_not_hydrate_an_evicted_session(
        self, isolated_runtime: WingRuntime
    ) -> None:
        """上传不水合：被逐出的会话不该因为一次上传被换入内存。"""
        sid = _seed_session(isolated_runtime)
        released, _ = isolated_runtime.release_session(sid)
        assert released is True and isolated_runtime.get_session(sid) is None

        ref = isolated_runtime.upload_file(sid, b"bytes", name="notes.txt")

        assert Path(str(ref.path)).read_bytes() == b"bytes"
        assert isolated_runtime.get_session(sid) is None, "上传把已逐出的会话唤醒了"

    @pytest.mark.asyncio
    async def test_alias_id_writes_into_the_canonical_session_dir(
        self, isolated_runtime: WingRuntime, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """别名（大小写 / 归一化不敏感 FS）解析成磁盘真名：落真名目录、不另起一份。

        大小写敏感的 FS 上真别名不存在，这里照 ``test_session_media_upload.py``
        的做法**造一个**——要钉的是"写目标是解析出的键，不是请求字符串"。
        """
        sid = _seed_session(isolated_runtime)
        store = isolated_runtime.sm._stores["file"]
        original = store.resolve_stored_id

        def alias_resolve(session_id: str) -> str | None:
            return sid if session_id == "alias-of-files" else original(session_id)

        monkeypatch.setattr(store, "resolve_stored_id", alias_resolve)

        ref = isolated_runtime.upload_file("alias-of-files", b"x", name="notes.txt")

        assert ref.path == str(files_dir(sid) / "notes.txt")
        assert not (sessions_root() / "alias-of-files").exists()

    @pytest.mark.asyncio
    async def test_archived_session_raises_the_active_error(
        self, isolated_runtime: WingRuntime
    ) -> None:
        from wing.session import SessionArchivedError

        sid = _seed_session(isolated_runtime)
        assert isolated_runtime.archive_session(sid)[0] is True

        with pytest.raises(SessionArchivedError):
            isolated_runtime.upload_file(sid, b"x", name="notes.txt")


# ── 2. 路由层 ──────────────────────────────────────────────────


@pytest.fixture
def real_client(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """真实 WingRuntime + 独立 sessions 根 + TestClient（不起 uvicorn）。"""
    with gateway_server(tmp_path, monkeypatch) as server:
        watcher = BodyWatcher(server._app)
        with TestClient(watcher) as client:
            yield client, server.runtime, watcher


class TestUploadFileRoute:
    def _create_session(self, client: TestClient, runtime: WingRuntime) -> str:
        """建一个**落盘**的会话（空会话逐出后不可解析——加一条消息让它成为会话）。"""
        response = client.post("/api/session/create", json={})
        assert response.status_code == 200, response.text
        sid = str(response.json()["session_id"])
        session = runtime.get_session(sid)
        assert session is not None
        session.context_manager.add_message(Message(role="user", content="hi"))
        return sid

    def _release(self, client: TestClient, sid: str) -> None:
        response = client.post("/api/session/release", json={"session_id": sid})
        assert response.status_code == 200, response.text
        assert response.json()["released"] is True, response.text

    def test_upload_returns_file_and_persists_bytes(self, real_client) -> None:
        client, runtime, _ = real_client
        sid = self._create_session(client, runtime)

        response = client.post(
            "/api/session/files",
            params={"session_id": sid, "name": "报告.pdf"},
            content=b"%PDF-1.4 fake",
            headers={"content-type": "application/octet-stream"},
        )

        assert response.status_code == 200, response.text
        body = response.json()
        assert body["ok"] is True
        assert body["file"] == {
            "name": "报告.pdf",
            "path": str(files_dir(sid) / "报告.pdf"),
            "bytes": 13,
        }, body
        assert (files_dir(sid) / "报告.pdf").read_bytes() == b"%PDF-1.4 fake"

    def test_missing_name_is_400(self, real_client) -> None:
        client, runtime, _ = real_client
        sid = self._create_session(client, runtime)

        response = client.post(
            "/api/session/files", params={"session_id": sid}, content=b"x"
        )

        assert response.status_code == 400, response.text
        assert "file name is required" in response.json()["detail"]
        assert not files_dir(sid).exists()

    def test_empty_name_is_400(self, real_client) -> None:
        client, runtime, _ = real_client
        sid = self._create_session(client, runtime)

        response = client.post(
            "/api/session/files", params={"session_id": sid, "name": ""}, content=b"x"
        )

        assert response.status_code == 400, response.text
        assert not files_dir(sid).exists()

    @pytest.mark.parametrize(
        "name",
        ["../escape", ".hidden", "a/b", "a\x01b", "n" * (FILE_NAME_MAX_BYTES + 1)],
        ids=["parent-ref", "leading-dot", "slash", "control", "too-long"],
    )
    def test_dirty_names_are_400_without_side_effects(self, real_client, name) -> None:
        client, runtime, _ = real_client
        sid = self._create_session(client, runtime)

        response = client.post(
            "/api/session/files", params={"session_id": sid, "name": name}, content=b"x"
        )

        assert response.status_code == 400, response.text
        assert response.json()["error"] == "bad_request"
        assert not files_dir(sid).exists(), "400 路径在存储上留下了痕迹"

    def test_name_gate_rejects_before_the_body_is_read(self, real_client) -> None:
        """名字是 query 参数：不合规的名字不必先吞下整个 body。"""
        client, runtime, watcher = real_client
        sid = self._create_session(client, runtime)
        watcher.messages.clear()

        response = client.post(
            "/api/session/files",
            params={"session_id": sid, "name": "../escape"},
            content=b"x" * 4096,
        )

        assert response.status_code == 400, response.text
        assert watcher.request_bodies() == [], "名字闸门失败却读了请求体"

    def test_unknown_session_is_404(self, real_client) -> None:
        client, _, _ = real_client
        response = client.post(
            "/api/session/files",
            params={"session_id": "nope", "name": "a.txt"},
            content=b"x",
        )
        assert response.status_code == 404, response.text
        assert response.json() == {"error": "not_found", "detail": "session not found"}
        assert not (sessions_root() / "nope").exists()

    def test_ghost_session_404_precedes_the_name_gate(self, real_client) -> None:
        """存在性先于一切内容校验：幽灵会话 + 坏名字也是 404。"""
        client, _, watcher = real_client
        watcher.messages.clear()

        response = client.post(
            "/api/session/files",
            params={"session_id": "nope", "name": "../escape"},
            content=b"x",
        )

        assert response.status_code == 404, response.text
        assert response.json()["detail"] == "session not found"
        assert watcher.request_bodies() == []

    def test_archived_session_is_404_with_the_actionable_marker(
        self, real_client
    ) -> None:
        client, runtime, _ = real_client
        sid = self._create_session(client, runtime)
        archived = client.post(
            "/api/session/archive", json={"session_id": sid, "archived": True}
        )
        assert archived.status_code == 200, archived.text
        assert archived.json()["changed"] is True, archived.text

        response = client.post(
            "/api/session/files",
            params={"session_id": sid, "name": "a.txt"},
            content=b"x",
        )

        assert response.status_code == 404, response.text
        assert response.json()["error"] == "session_archived"
        assert "restore" in response.json()["detail"]
        assert not (sessions_root() / ".archive" / sid / "files").exists()

    def test_oversize_bytes_are_400_against_config_limit(
        self, real_client, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        client, runtime, _ = real_client
        sid = self._create_session(client, runtime)
        monkeypatch.setattr(get_config().files, "max_bytes", 4)

        response = client.post(
            "/api/session/files",
            params={"session_id": sid, "name": "a.txt"},
            content=b"12345",
        )

        assert response.status_code == 400, response.text
        assert "files.max_bytes" in response.json()["detail"]
        assert not files_dir(sid).exists()

    def test_declared_oversize_is_rejected_before_the_body_is_read(
        self, real_client, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        client, runtime, watcher = real_client
        sid = self._create_session(client, runtime)
        watcher.messages.clear()
        monkeypatch.setattr(get_config().files, "max_bytes", 1024)

        response = client.post(
            "/api/session/files",
            params={"session_id": sid, "name": "a.txt"},
            content=b"x" * 4096,
        )

        assert response.status_code == 400, response.text
        detail = response.json()["detail"]
        # 与字节闸门同一句文案（同一个拒绝不该有两套说法）。
        assert "file too large" in detail and "files.max_bytes" in detail, detail
        assert watcher.request_bodies() == [], "声明超限的请求体被读进来了"
        assert not files_dir(sid).exists()

    def test_empty_body_is_400(self, real_client) -> None:
        client, runtime, _ = real_client
        sid = self._create_session(client, runtime)

        response = client.post(
            "/api/session/files",
            params={"session_id": sid, "name": "a.txt"},
            content=b"",
        )

        assert response.status_code == 400, response.text
        assert not files_dir(sid).exists()

    def test_oversize_body_without_content_length_is_still_rejected(
        self, real_client, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """没有 ``Content-Length``（分块传输）时由字节闸门按真实字节兜底。"""
        client, runtime, _ = real_client
        sid = self._create_session(client, runtime)
        monkeypatch.setattr(get_config().files, "max_bytes", 1024)

        def chunks():
            yield b"x" * 2048

        response = client.post(
            "/api/session/files",
            params={"session_id": sid, "name": "a.txt"},
            content=chunks(),
        )

        assert response.status_code == 400, response.text
        assert "files.max_bytes" in response.json()["detail"]
        assert not files_dir(sid).exists()

    def test_missing_session_id_is_422(self, real_client) -> None:
        client, _, _ = real_client
        response = client.post(
            "/api/session/files", params={"name": "a.txt"}, content=b"x"
        )
        assert response.status_code == 422, response.text

    def test_upload_runs_off_the_event_loop_thread(
        self, real_client, monkeypatch
    ) -> None:
        """大文件 I/O 在线程池里执行（64 MiB 级读写不该卡住事件循环）。

        观测面：`require_live_session`（收包前的同步预检）跑在事件循环线程，
        `upload_file` 必须**不在**同一个线程上——否则它就在循环里做整读 / fsync。
        """
        import threading

        client, runtime, _ = real_client
        sid = self._create_session(client, runtime)
        threads: dict[str, int] = {}
        real_require = runtime.require_live_session
        real_upload = runtime.upload_file

        def spy_require(session_id: str) -> None:
            threads["loop"] = threading.get_ident()
            real_require(session_id)

        def spy_upload(session_id: str, data: bytes, *, name: str):
            threads["upload"] = threading.get_ident()
            return real_upload(session_id, data, name=name)

        monkeypatch.setattr(runtime, "require_live_session", spy_require)
        monkeypatch.setattr(runtime, "upload_file", spy_upload)

        response = client.post(
            "/api/session/files",
            params={"session_id": sid, "name": "a.txt"},
            content=b"x",
        )

        assert response.status_code == 200, response.text
        assert set(threads) == {"loop", "upload"}, threads
        assert threads["upload"] != threads["loop"], (
            "上传仍在事件循环线程上执行（大文件会把整个网关卡住）"
        )

    def test_upload_does_not_hydrate_an_evicted_session(self, real_client) -> None:
        client, runtime, _ = real_client
        sid = self._create_session(client, runtime)
        self._release(client, sid)
        assert runtime.get_session(sid) is None

        response = client.post(
            "/api/session/files",
            params={"session_id": sid, "name": "a.txt"},
            content=b"x",
        )

        assert response.status_code == 200, response.text
        assert runtime.get_session(sid) is None, "上传把已逐出的会话唤醒了"
