"""Workspace 文件浏览 API 的整机场景 —— 真网关 + 公开 HTTP 协议。

`/api/workspace/*`（roots / list / stat / read）是异机 Web 前端的接入面，本场景
只经公开 HTTP 断言，钉住四件事：

1. **起点**：``roots`` 给出 home 与网关进程 cwd（= ``ProbeEnv`` 的 ``env.root``，
   子进程以它启动）——"工作区选择"的入口；
2. **列举**：排序（目录在前、组内按名）、分页（``limit`` / ``offset`` /
   ``next_offset``）、条目元数据（大小 / mtime / mime）、符号链接（``target`` /
   ``broken``，不解析目标）；
3. **读取**：字节流 + MIME + ``Content-Disposition: inline``；``Range`` → 206 +
   ``content-range``；符号链接跟随到目标；
4. **边界**：相对路径 400（协议面不暴露"网关进程 cwd"这类本地细节）、目录读取
   400、缺失路径 404（统一 ``error == "not_found"`` 形状）。

文件全部在 ``<env.root>/workspace`` 下创建——不触碰任何 ``~/.wing`` 活数据，
也不依赖会话（本场景不开会话）。
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from wing_probe import DriverHttp, DriverHttpError, Probe
from wing_probe.driver.http import HttpCall


async def _failed(
    http: DriverHttp,
    method: str,
    path: str,
    *,
    params: dict[str, str] | None = None,
) -> HttpCall:
    """发一个预期失败的请求，返回留档（非 2xx 抛错的既有语义不变）。"""
    with pytest.raises(DriverHttpError) as excinfo:
        await http.request(method, path, params=params)
    return excinfo.value.call


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_workspace_file_api(probe: Probe) -> None:
    """roots / list / stat / read 的 wire 契约与边界（见文件 docstring）。"""
    http = probe.driver_required.http

    base = probe.workspace / "browse"
    (base / "adir").mkdir(parents=True)
    (base / "adir" / "inner.txt").write_text("inner", encoding="utf-8")
    (base / "alpha.md").write_text("# alpha", encoding="utf-8")
    (base / "zebra.txt").write_text("z-last", encoding="utf-8")
    os.symlink("alpha.md", base / "link.md")
    os.symlink("missing.md", base / "dangling.md")

    # ── ① roots ────────────────────────────────────────────────
    roots = await http.request("GET", "/api/workspace/roots")
    by_kind = {root["kind"]: root for root in roots["roots"]}
    assert by_kind["cwd"]["path"] == str(probe.env.root), roots
    # 网关子进程继承测试进程的 HOME（ProbeEnv 只覆盖 WING_HOME / WING_SESSIONS_PATH）。
    assert by_kind["home"]["path"] == str(Path.home()), roots
    for root in roots["roots"]:
        assert os.path.isabs(root["path"])
        assert root["name"] == os.path.basename(root["path"])

    # ── ② list：全量 / 排序 / 元数据 / 链接 ─────────────────────
    listing = await http.request(
        "GET", "/api/workspace/list", params={"path": str(base)}
    )
    assert listing["path"] == str(base)
    assert listing["total"] == 5
    assert listing["next_offset"] is None
    assert [(e["name"], e["type"]) for e in listing["entries"]] == [
        ("adir", "dir"),
        ("alpha.md", "file"),
        ("dangling.md", "symlink"),
        ("link.md", "symlink"),
        ("zebra.txt", "file"),
    ]

    entries = {e["name"]: e for e in listing["entries"]}
    alpha = entries["alpha.md"]
    assert alpha["path"] == str(base / "alpha.md")
    assert (alpha["size"], alpha["mime"]) == (7, "text/markdown")
    assert alpha["mtime"] > 0
    assert (alpha["symlink_target"], alpha["broken"]) == (None, False)

    for name in ("adir", "link.md"):
        assert entries[name]["size"] is None, name  # 目录 / 链接没有字节数
    assert entries["link.md"]["symlink_target"] == "alpha.md"
    assert entries["link.md"]["broken"] is False
    assert entries["dangling.md"]["symlink_target"] == "missing.md"
    assert entries["dangling.md"]["broken"] is True

    # ── ③ list：分页窗口拼起来 == 全量（翻页稳定性） ────────────
    windows: list[list[str]] = []
    offset = 0
    while offset is not None:
        page = await http.request(
            "GET",
            "/api/workspace/list",
            params={"path": str(base), "offset": str(offset), "limit": "2"},
        )
        assert page["total"] == 5
        windows.extend(e["name"] for e in page["entries"])
        offset = page["next_offset"]
    assert windows == [e["name"] for e in listing["entries"]]

    # ── ④ stat：元数据 + 统一 404 形状 ─────────────────────────
    stat = await http.request(
        "GET", "/api/workspace/stat", params={"path": str(base / "zebra.txt")}
    )
    assert (stat["name"], stat["type"], stat["size"], stat["mime"]) == (
        "zebra.txt",
        "file",
        6,
        "text/plain",
    )
    missing = await _failed(
        http, "GET", "/api/workspace/stat", params={"path": str(base / "nope")}
    )
    assert missing.status == 404
    assert missing.response["error"] == "not_found"
    assert "no such entry" in missing.response["detail"]

    # ── ⑤ read：字节流 + MIME + disposition + Range ────────────
    call, body = await http.request_raw(
        "GET", "/api/workspace/read", params={"path": str(base / "alpha.md")}
    )
    assert call.status == 200
    assert body == b"# alpha"
    assert call.response_headers["content-type"] == "text/markdown; charset=utf-8"
    assert call.response_headers["content-disposition"] == 'inline; filename="alpha.md"'
    assert call.response_headers["accept-ranges"] == "bytes"

    ranged, chunk = await http.request_raw(
        "GET",
        "/api/workspace/read",
        params={"path": str(base / "alpha.md")},
        headers={"Range": "bytes=0-3"},
    )
    assert ranged.status == 206
    assert chunk == b"# al"
    assert ranged.response_headers["content-range"] == "bytes 0-3/7"

    # 符号链接跟随到目标（read 是 OS 语义；list / stat 是 lstat 口径）。
    _, linked = await http.request_raw(
        "GET", "/api/workspace/read", params={"path": str(base / "link.md")}
    )
    assert linked == b"# alpha"

    # ── ⑥ 边界：相对路径 / 目录 / 缺失 / 悬空链接 ───────────────
    relative = await _failed(
        http, "GET", "/api/workspace/list", params={"path": "browse"}
    )
    assert (relative.status, relative.response["error"]) == (400, "bad_request")

    as_directory = await _failed(
        http, "GET", "/api/workspace/read", params={"path": str(base / "adir")}
    )
    assert as_directory.status == 400
    assert "is a directory" in as_directory.response["detail"]

    absent = await _failed(
        http, "GET", "/api/workspace/read", params={"path": str(base / "nope.txt")}
    )
    assert absent.status == 404

    dangling = await _failed(
        http,
        "GET",
        "/api/workspace/read",
        params={"path": str(base / "dangling.md")},
    )
    assert dangling.status == 404
