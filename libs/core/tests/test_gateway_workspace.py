# tests/test_gateway_workspace.py
"""Workspace 文件浏览 API —— 服务层 + 端点 + 网关照装。

三层覆盖：

1. **service**（``wing/gateway/workspace/service.py``）：纯函数语义——路径归一 /
   排序 / 分页 / 符号链接 / 只读常规文件的守卫（FIFO 不进 read）；
2. **router**（独立挂载）：wire 形状、状态码、Range / Content-Disposition；
3. **gateway 装配**：真 ``GatewayServer`` + tmp WING_HOME——路由已注册、错误形状
   统一为 ``ErrorResponse``、RBAC 对新端点默认关闭（``tool_runtime`` 403）。
"""

from __future__ import annotations

import contextlib
import os
from collections.abc import Iterator
from pathlib import Path

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from wing.config import reset_config
from wing.gateway.workspace import service
from wing.gateway.workspace.routes import router as workspace_router

#: 含 cors 段的合法配置（网关装配测试用；providers/agents 非空以免降级启动）。
VALID_YAML = """\
providers:
  - name: local
    base_url: https://api.example.com
    api_key: sk-test
    models: [m1]
agents:
  - name: default
    model: m1
gateway:
  cors:
    origins: ["https://ui.example.com"]
"""

ADMIN_KEY = "admin-key-123456"
TOOL_KEY = "tool-key-123456"


# ─────────────────────────────────────────────────────────────────────────────
# 通用素材
# ─────────────────────────────────────────────────────────────────────────────


@pytest.fixture
def tree(tmp_path: Path) -> Path:
    """一棵固定的目录树：目录 / 文件 / 隐藏文件 / 链接 / 悬空链接。"""
    (tmp_path / "bdir").mkdir()
    (tmp_path / "bdir" / "inner.txt").write_text("inner", encoding="utf-8")
    (tmp_path / "adir").mkdir()
    (tmp_path / "zebra.txt").write_text("hello world", encoding="utf-8")
    (tmp_path / "Apple.md").write_text("# title", encoding="utf-8")
    (tmp_path / ".hidden").write_text("dot", encoding="utf-8")
    os.symlink("zebra.txt", tmp_path / "link.txt")
    os.symlink("missing.txt", tmp_path / "dangling")
    return tmp_path


@pytest.fixture
def client() -> Iterator[TestClient]:
    """只挂 workspace router 的独立 app（不依赖 server / runtime）。"""
    app = FastAPI()
    app.include_router(workspace_router)
    with TestClient(app) as tc:
        yield tc


@pytest.fixture
def gateway_app(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Iterator[TestClient]:
    """真 ``GatewayServer`` + tmp WING_HOME（真 load_config，不 mock 配置）。"""
    monkeypatch.setenv("WING_HOME", str(tmp_path))
    monkeypatch.setenv("WING_SESSIONS_PATH", str(tmp_path / "sessions"))
    config_path = tmp_path / "core" / "config.yaml"
    config_path.parent.mkdir(parents=True)
    config_path.write_text(VALID_YAML, encoding="utf-8")
    reset_config()

    from wing.gateway.server import GatewayServer

    server = GatewayServer()
    try:
        with TestClient(server._app) as tc:
            yield tc
    finally:
        reset_config()


@contextlib.contextmanager
def _auth_gateway(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Iterator[TestClient]:
    """开启鉴权（admin / tool_runtime 两把 key）的真网关。"""
    monkeypatch.setenv("WING_HOME", str(tmp_path))
    monkeypatch.setenv("WING_SESSIONS_PATH", str(tmp_path / "sessions"))
    config_path = tmp_path / "core" / "config.yaml"
    config_path.parent.mkdir(parents=True)
    config_path.write_text(
        VALID_YAML
        + "  auth:\n"
        + "    enabled: true\n"
        + "    keys:\n"
        + f'      - {{key: "{ADMIN_KEY}", role: admin}}\n'
        + f'      - {{key: "{TOOL_KEY}", role: tool_runtime}}\n',
        encoding="utf-8",
    )
    reset_config()

    from wing.gateway.server import GatewayServer

    server = GatewayServer()
    try:
        with TestClient(server._app) as tc:
            yield tc
    finally:
        reset_config()


# ─────────────────────────────────────────────────────────────────────────────
# service：路径
# ─────────────────────────────────────────────────────────────────────────────


class TestNormalizePath:
    def test_relative_is_rejected(self) -> None:
        with pytest.raises(service.WorkspaceError) as exc:
            service.normalize_path("foo/bar")
        assert exc.value.status == 400
        assert "absolute" in exc.value.detail

    def test_empty_is_rejected(self) -> None:
        with pytest.raises(service.WorkspaceError) as exc:
            service.normalize_path("")
        assert exc.value.status == 400

    def test_nul_byte_is_rejected(self) -> None:
        with pytest.raises(service.WorkspaceError) as exc:
            service.normalize_path("/tmp/a\x00b")
        assert exc.value.status == 400
        assert "NUL" in exc.value.detail

    def test_dot_segments_are_lexically_normalized(self) -> None:
        assert service.normalize_path("/a/b/../b/./c") == "/a/b/c"
        assert service.normalize_path("/a//b") == "/a/b"
        assert service.normalize_path("/..") == "/"

    def test_symlinks_are_not_resolved(self, tmp_path: Path) -> None:
        target = tmp_path / "real"
        target.mkdir()
        link = tmp_path / "link"
        os.symlink(target, link)
        assert service.normalize_path(str(link)) == str(link)


def test_wire_text_rejects_lone_surrogate() -> None:
    assert service._wire_text("plain") == "plain"
    assert service._wire_text("emoji 🐦") == "emoji 🐦"
    assert service._wire_text("bad\udcff") is None


# ─────────────────────────────────────────────────────────────────────────────
# service：列举
# ─────────────────────────────────────────────────────────────────────────────


class TestListDir:
    def test_order_dirs_first_then_casefold_name(self, tree: Path) -> None:
        listing = service.list_dir(str(tree))
        assert [e.name for e in listing.entries] == [
            "adir",
            "bdir",
            ".hidden",
            "Apple.md",
            "dangling",
            "link.txt",
            "zebra.txt",
        ]
        assert listing.total == 7
        assert listing.next_offset is None

    def test_entry_metadata(self, tree: Path) -> None:
        by_name = {e.name: e for e in service.list_dir(str(tree)).entries}
        zebra = by_name["zebra.txt"]
        assert (zebra.type, zebra.size, zebra.mime) == ("file", 11, "text/plain")
        assert zebra.mtime > 0
        assert (zebra.symlink_target, zebra.broken) == (None, False)

        dir_entry = by_name["adir"]
        assert (dir_entry.type, dir_entry.size, dir_entry.mime) == ("dir", None, None)

        link = by_name["link.txt"]
        assert (link.type, link.size, link.mime) == ("symlink", None, None)
        assert (link.symlink_target, link.broken) == ("zebra.txt", False)

        dangling = by_name["dangling"]
        assert dangling.type == "symlink"
        assert (dangling.symlink_target, dangling.broken) == ("missing.txt", True)

    def test_pagination_window(self, tree: Path) -> None:
        first = service.list_dir(str(tree), offset=0, limit=3)
        assert [e.name for e in first.entries] == ["adir", "bdir", ".hidden"]
        assert first.total == 7
        assert first.next_offset == 3

        second = service.list_dir(str(tree), offset=3, limit=3)
        assert [e.name for e in second.entries] == ["Apple.md", "dangling", "link.txt"]
        assert second.next_offset == 6

        last = service.list_dir(str(tree), offset=6, limit=3)
        assert [e.name for e in last.entries] == ["zebra.txt"]
        assert last.next_offset is None

    def test_offset_beyond_total_is_empty(self, tree: Path) -> None:
        listing = service.list_dir(str(tree), offset=99, limit=10)
        assert listing.entries == ()
        assert listing.total == 7
        assert listing.next_offset is None

    def test_path_echo_is_normalized(self, tree: Path) -> None:
        listing = service.list_dir(str(tree / "bdir" / ".."))
        assert listing.path == str(tree)

    def test_missing_dir_is_404(self, tmp_path: Path) -> None:
        with pytest.raises(service.WorkspaceError) as exc:
            service.list_dir(str(tmp_path / "nope"))
        assert exc.value.status == 404

    def test_file_is_not_a_directory(self, tree: Path) -> None:
        with pytest.raises(service.WorkspaceError) as exc:
            service.list_dir(str(tree / "zebra.txt"))
        assert exc.value.status == 400
        assert "not a directory" in exc.value.detail

    def test_dir_without_search_permission_is_403(self, tree: Path) -> None:
        if hasattr(os, "geteuid") and os.geteuid() == 0:
            pytest.skip("root bypasses the directory's read bits")
        locked = tree / "locked"
        locked.mkdir()
        locked.chmod(0o000)
        try:
            with pytest.raises(service.WorkspaceError) as exc:
                service.list_dir(str(locked))
            assert exc.value.status == 403
        finally:
            locked.chmod(0o700)

    def test_names_not_representable_as_utf8_are_skipped(self, tmp_path: Path) -> None:
        try:
            with open(os.fsencode(tmp_path) + b"/bad-\xff", "wb"):
                pass
        except OSError:
            pytest.skip("filesystem rejects non-UTF-8 file names")
        (tmp_path / "ok.txt").write_text("x", encoding="utf-8")
        listing = service.list_dir(str(tmp_path))
        assert [e.name for e in listing.entries] == ["ok.txt"]
        assert listing.total == 1

    def test_fifo_is_typed_other(self, tmp_path: Path) -> None:
        if not hasattr(os, "mkfifo"):
            pytest.skip("no FIFO on this platform")
        os.mkfifo(tmp_path / "pipe")
        entry = service.list_dir(str(tmp_path)).entries[0]
        assert (entry.type, entry.size, entry.mime) == ("other", None, None)


# ─────────────────────────────────────────────────────────────────────────────
# service：stat / read / roots
# ─────────────────────────────────────────────────────────────────────────────


class TestStatPath:
    def test_file(self, tree: Path) -> None:
        entry = service.stat_path(str(tree / "Apple.md"))
        assert (entry.name, entry.type, entry.size, entry.mime) == (
            "Apple.md",
            "file",
            7,
            "text/markdown",
        )

    def test_symlink_stays_a_symlink(self, tree: Path) -> None:
        entry = service.stat_path(str(tree / "link.txt"))
        assert (entry.type, entry.symlink_target, entry.broken) == (
            "symlink",
            "zebra.txt",
            False,
        )

    def test_root_name_falls_back_to_path(self) -> None:
        entry = service.stat_path("/")
        assert entry.name == "/"
        assert entry.type == "dir"

    def test_missing_is_404(self, tmp_path: Path) -> None:
        with pytest.raises(service.WorkspaceError) as exc:
            service.stat_path(str(tmp_path / "nope"))
        assert exc.value.status == 404


class TestResolveRead:
    def test_regular_file(self, tree: Path) -> None:
        target = service.resolve_read(str(tree / "zebra.txt"))
        assert target.name == "zebra.txt"
        assert target.stat.st_size == 11

    def test_symlink_follows_to_target(self, tree: Path) -> None:
        target = service.resolve_read(str(tree / "link.txt"))
        assert target.stat.st_size == 11

    def test_broken_symlink_is_404(self, tree: Path) -> None:
        with pytest.raises(service.WorkspaceError) as exc:
            service.resolve_read(str(tree / "dangling"))
        assert exc.value.status == 404

    def test_directory_is_400(self, tree: Path) -> None:
        with pytest.raises(service.WorkspaceError) as exc:
            service.resolve_read(str(tree / "adir"))
        assert exc.value.status == 400
        assert "is a directory" in exc.value.detail

    def test_fifo_is_rejected_before_open(self, tmp_path: Path) -> None:
        if not hasattr(os, "mkfifo"):
            pytest.skip("no FIFO on this platform")
        os.mkfifo(tmp_path / "pipe")
        with pytest.raises(service.WorkspaceError) as exc:
            service.resolve_read(str(tmp_path / "pipe"))
        assert exc.value.status == 400
        assert "not a regular file" in exc.value.detail

    def test_missing_is_404(self, tmp_path: Path) -> None:
        with pytest.raises(service.WorkspaceError) as exc:
            service.resolve_read(str(tmp_path / "nope"))
        assert exc.value.status == 404


class TestRoots:
    def test_home_and_cwd(self) -> None:
        found = {root.kind: root for root in service.roots()}
        assert "home" in found and "cwd" in found
        for root in found.values():
            assert os.path.isabs(root.path)
            assert root.name == os.path.basename(root.path)

    def test_unresolvable_home_is_skipped(
        self, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
    ) -> None:
        monkeypatch.setattr(service.os.path, "expanduser", lambda _: "~nobody")
        monkeypatch.setattr(service.os, "getcwd", lambda: str(tmp_path))
        roots = service.roots()
        assert [root.kind for root in roots] == ["cwd"]

    def test_home_equal_to_cwd_is_deduped(
        self, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
    ) -> None:
        monkeypatch.setattr(service.os.path, "expanduser", lambda _: str(tmp_path))
        monkeypatch.setattr(service.os, "getcwd", lambda: str(tmp_path))
        roots = service.roots()
        assert len(roots) == 1
        assert roots[0].kind == "home"

    def test_missing_cwd_is_skipped(self, monkeypatch: pytest.MonkeyPatch) -> None:
        def _boom() -> str:
            raise FileNotFoundError("cwd gone")

        monkeypatch.setattr(service.os, "getcwd", _boom)
        monkeypatch.setattr(
            service.os.path, "expanduser", lambda _: "/nonexistent-wing-probe-home"
        )
        assert service.roots() == ()


# ─────────────────────────────────────────────────────────────────────────────
# 独立 router：wire 形状与 HTTP 语义
# ─────────────────────────────────────────────────────────────────────────────


class TestRouterWire:
    def test_list_wire_shape(self, client: TestClient, tree: Path) -> None:
        resp = client.get("/api/workspace/list", params={"path": str(tree), "limit": 2})
        assert resp.status_code == 200
        body = resp.json()
        assert body["path"] == str(tree)
        assert body["total"] == 7
        assert body["next_offset"] == 2
        assert [e["name"] for e in body["entries"]] == ["adir", "bdir"]
        assert set(body["entries"][0]) == {
            "name",
            "path",
            "type",
            "size",
            "mtime",
            "mime",
            "symlink_target",
            "broken",
        }

    def test_stat_wire_shape(self, client: TestClient, tree: Path) -> None:
        resp = client.get(
            "/api/workspace/stat", params={"path": str(tree / "zebra.txt")}
        )
        assert resp.status_code == 200
        body = resp.json()
        assert body["type"] == "file"
        assert body["size"] == 11
        assert body["mime"] == "text/plain"
        assert body["path"] == str(tree / "zebra.txt")

    def test_roots_wire_shape(self, client: TestClient) -> None:
        resp = client.get("/api/workspace/roots")
        assert resp.status_code == 200
        roots = resp.json()["roots"]
        assert roots and {r["kind"] for r in roots} <= {"home", "cwd"}
        assert all(set(r) == {"path", "kind", "name"} for r in roots)

    def test_read_serves_bytes_with_mime_and_disposition(
        self, client: TestClient, tree: Path
    ) -> None:
        resp = client.get(
            "/api/workspace/read", params={"path": str(tree / "zebra.txt")}
        )
        assert resp.status_code == 200
        assert resp.content == b"hello world"
        assert resp.headers["content-type"] == "text/plain; charset=utf-8"
        assert resp.headers["content-disposition"] == 'inline; filename="zebra.txt"'
        assert resp.headers["accept-ranges"] == "bytes"

    def test_read_head_returns_headers_only(
        self, client: TestClient, tree: Path
    ) -> None:
        resp = client.head(
            "/api/workspace/read", params={"path": str(tree / "zebra.txt")}
        )
        assert resp.status_code == 200
        assert resp.content == b""
        assert resp.headers["content-length"] == "11"
        assert resp.headers["content-type"] == "text/plain; charset=utf-8"

    def test_read_range_returns_206(self, client: TestClient, tree: Path) -> None:
        resp = client.get(
            "/api/workspace/read",
            params={"path": str(tree / "zebra.txt")},
            headers={"Range": "bytes=0-4"},
        )
        assert resp.status_code == 206
        assert resp.content == b"hello"
        assert resp.headers["content-range"] == "bytes 0-4/11"

    def test_read_unsatisfiable_range_returns_416(
        self, client: TestClient, tree: Path
    ) -> None:
        resp = client.get(
            "/api/workspace/read",
            params={"path": str(tree / "zebra.txt")},
            headers={"Range": "bytes=100-200"},
        )
        assert resp.status_code == 416
        assert resp.headers["content-range"] == "bytes */11"

    def test_read_unknown_extension_is_octet_stream(
        self, client: TestClient, tmp_path: Path
    ) -> None:
        blob = tmp_path / "data.bin-x"
        blob.write_bytes(b"\x00\x01\x02")
        resp = client.get("/api/workspace/read", params={"path": str(blob)})
        assert resp.status_code == 200
        assert resp.headers["content-type"] == "application/octet-stream"

    def test_read_non_ascii_filename_disposition(
        self, client: TestClient, tmp_path: Path
    ) -> None:
        target = tmp_path / "报告.md"
        target.write_text("# ok", encoding="utf-8")
        resp = client.get("/api/workspace/read", params={"path": str(target)})
        assert resp.status_code == 200
        assert resp.headers["content-disposition"].startswith(
            "inline; filename*=utf-8''"
        )

    def test_status_codes(self, client: TestClient, tree: Path) -> None:
        bad_path = client.get("/api/workspace/list", params={"path": "relative"})
        assert bad_path.status_code == 400
        missing_dir = client.get(
            "/api/workspace/list", params={"path": str(tree / "nope")}
        )
        assert missing_dir.status_code == 404
        missing_entry = client.get(
            "/api/workspace/stat", params={"path": str(tree / "nope")}
        )
        assert missing_entry.status_code == 404
        dir_read = client.get(
            "/api/workspace/read", params={"path": str(tree / "adir")}
        )
        assert dir_read.status_code == 400
        missing_read = client.get(
            "/api/workspace/read", params={"path": str(tree / "nope")}
        )
        assert missing_read.status_code == 404

    def test_validation_errors(self, client: TestClient, tree: Path) -> None:
        assert client.get("/api/workspace/list").status_code == 422  # path 缺失
        too_big = client.get(
            "/api/workspace/list", params={"path": str(tree), "limit": 10_000}
        )
        assert too_big.status_code == 422
        negative = client.get(
            "/api/workspace/list", params={"path": str(tree), "offset": -1}
        )
        assert negative.status_code == 422


# ─────────────────────────────────────────────────────────────────────────────
# 真网关装配：注册 / 错误形状 / RBAC
# ─────────────────────────────────────────────────────────────────────────────


class TestGatewayWiring:
    def test_route_registered_and_serves(
        self, gateway_app: TestClient, tmp_path: Path
    ) -> None:
        resp = gateway_app.get("/api/workspace/list", params={"path": str(tmp_path)})
        assert resp.status_code == 200
        assert resp.json()["path"] == str(tmp_path)

    def test_error_shape_is_unified(
        self, gateway_app: TestClient, tmp_path: Path
    ) -> None:
        resp = gateway_app.get(
            "/api/workspace/stat", params={"path": str(tmp_path / "nope")}
        )
        assert resp.status_code == 404
        body = resp.json()
        assert body["error"] == "not_found"
        assert "no such entry" in body["detail"]

    def test_rbac_tool_runtime_is_denied(self, tmp_path: Path, monkeypatch) -> None:
        with _auth_gateway(tmp_path, monkeypatch) as tc:
            resp = tc.get(
                "/api/workspace/list",
                params={"path": str(tmp_path)},
                headers={"Authorization": f"Bearer {TOOL_KEY}"},
            )
            assert resp.status_code == 403
            assert resp.json()["error"] == "forbidden"

    def test_rbac_admin_allowed_and_anonymous_401(
        self, tmp_path: Path, monkeypatch
    ) -> None:
        with _auth_gateway(tmp_path, monkeypatch) as tc:
            ok = tc.get(
                "/api/workspace/list",
                params={"path": str(tmp_path)},
                headers={"Authorization": f"Bearer {ADMIN_KEY}"},
            )
            assert ok.status_code == 200
            assert tc.get("/api/workspace/roots").status_code == 401
