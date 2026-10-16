"""文件上传端点（``POST /api/session/files``）probe 场景。

被守的语义（design.md §5 B2 / §4.3 的消费语境）：

- **成功四件套**：原始字节体上传 → 响应 ``{ok, file:{name, path, bytes}}`` 与磁盘事实
  一致（``path`` 是网关宿主机**绝对路径**、``bytes`` 是真实字节数）、对象落在
  ``<sessions-root>/<sid>/files/<名>``（**保留原名**，目录里只有这一份）；
- **agent 真读得动**（这条通道的意义）：把 ``path`` 写进消息正文 → 模型发起的 Read
  工具调用拿到文件内容——绝对路径在 agent 侧可直接读；
- **幂等与不覆盖**：同名同字节 → 同一路径（inode / mtime 不变）；同名异字节 →
  ``stem-2.ext`` 递增且**首份字节原样保留**（链上的 ``[file: …]`` 引用指向它）；
- **拒绝零副作用**：名字卫生（``../x`` / 点开头 / 分隔符 / 控制符 / 超长 / 缺省）/
  空 body / 超 ``files.max_bytes`` / 幽灵会话（404）——存储上不留任何痕迹
  （``<sid>/files/`` 不存在），拒绝之后合法上传照常成功；
- **收包顺序**：会话存在性 → 名字闸门 → ``Content-Length`` 预检 → 收包——声明超限的
  请求一个 body 字节都不发也拿得到 400（裸 socket 取证）；
- **归档冻结**：归档会话上传 → 404 + 结构化 ``error="session_archived"``；
- **memory 后端**：``path`` 为 null（没有本地文件，如实降级）。

断言锚定四个取证面：**响应字段**、**文件系统**（落点 / 目录清单 / 字节）、
**假 Provider 请求留档**（Read 工具真的读到了内容）、**HTTP 留档**（原始字节体通道）。
文件字节由场景现场生成，独立成文件、不 import 其它场景模块。
"""

from __future__ import annotations

import contextlib
import asyncio
from pathlib import Path
from typing import Any
from urllib.parse import quote, urlsplit

import pytest

from wing_probe import DriverHttpError, Probe, ToolCall, Turn

# ── 模型 / 环境旋钮（场景之间零共享） ────────────────────────────

FILES_MODEL = "probe/files-upload"

FILES_SPEC: dict[str, Any] = {
    "name": FILES_MODEL,
    "display_name": "Files Upload",
    "description": "file upload scenarios",
}

#: 超限场景的 files 配置：上限 16 B（小载荷仍可上传）。
SMALL_LIMIT_FILES: dict[str, Any] = {"max_bytes": 16}

#: 裸 socket 场景读响应的上限：拒绝要发生在**收包之前**（见 media 场景同款）。
NO_BODY_WAIT_SECONDS = 5.0

CONTENT = b"hello from probe\n"


# ── 助手（本文件自足） ──────────────────────────────────────────


def files_dir(probe: Probe, session_id: str) -> Path:
    """会话私有文件目录（未写过文件时**不存在**）。"""
    return probe.env.session_dir(session_id) / "files"


def file_names(probe: Probe, session_id: str) -> list[str]:
    directory = files_dir(probe, session_id)
    return [] if not directory.is_dir() else sorted(p.name for p in directory.iterdir())


async def upload(
    probe: Probe, session_id: str, data: bytes, *, name: str | None
) -> dict:
    """``POST /api/session/files``（原始字节体；非 2xx 抛 DriverHttpError）。"""
    params: dict[str, Any] = {"session_id": session_id}
    if name is not None:
        params["name"] = name
    return await probe.driver_required.http.request(
        "POST", "/api/session/files", params=params, content=data
    )


def fs_is_case_insensitive(base: Path) -> bool:
    """本机文件系统对大小写是否不敏感（决定"别名"是否解析到同一个目录项）。"""
    probe_dir = base / "fs-case-probe"
    probe_dir.mkdir(parents=True, exist_ok=True)
    try:
        return (base / "FS-CASE-PROBE").is_dir()
    finally:
        probe_dir.rmdir()


async def seed(probe: Probe) -> Any:
    """一个"是会话"的已订阅会话（有历史记录才算真的存在）。"""
    probe.register(FILES_MODEL, Turn.of(text="seeded"))
    session = await probe.session(model=FILES_MODEL)
    await session.chat("seed the history")
    return session


# ── 场景 1：成功四件套（响应 / 磁盘 / 通道） ─────────────────────


@pytest.mark.probe_env(models=[FILES_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_upload_returns_path_and_persists_bytes(probe: Probe) -> None:
    """上传一次：响应字段 == 文件事实；对象落盘、字节全等、目录只此一份。"""
    session = await seed(probe)

    response = await upload(probe, session.session_id, CONTENT, name="报告.txt")

    assert response["ok"] is True, response
    assert response["file"] == {
        "name": "报告.txt",
        "path": str(files_dir(probe, session.session_id) / "报告.txt"),
        "bytes": len(CONTENT),
    }, response
    assert Path(response["file"]["path"]).is_absolute()
    assert (files_dir(probe, session.session_id) / "报告.txt").read_bytes() == CONTENT
    assert file_names(probe, session.session_id) == ["报告.txt"]

    # 留档：走的是原始字节体通道（不是 JSON body），query 带业务参数。
    call = probe.driver_required.http.last_call(path="/api/session/files")
    assert call is not None and call.params == {
        "session_id": session.session_id,
        "name": "报告.txt",
    }, call
    assert call.body is None, "原始字节体不该被当 JSON 留档"
    assert call.detail == f"raw body: {len(CONTENT)} byte(s)", call.detail
    session.watch.assert_never("error")


# ── 场景 2：agent 真读得动（这条通道的意义） ────────────────────


@pytest.mark.probe_env(models=[FILES_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_agent_reads_the_uploaded_file_by_returned_path(probe: Probe) -> None:
    """把返回的绝对路径写进消息正文 → 模型发起的 Read 工具真的读到了内容。"""
    # 建会话不调 LLM：脚本可以等到路径落定之后再注册（工具参数里要用它）。
    session = await probe.session(model=FILES_MODEL)
    uploaded = await upload(probe, session.session_id, CONTENT, name="notes.txt")
    path = uploaded["file"]["path"]

    probe.register(
        FILES_MODEL,
        Turn.of(tool_calls=[ToolCall("Read", {"path": path})]),
        Turn.of(text="read it"),
    )
    await session.send(f"[file: notes.txt · {len(CONTENT)} bytes · {path}]")
    result = await session.watch.expect(["turn_result", "error"], within=30)
    assert result.type == "turn_result", result.data
    assert result.data["subtype"] == "success", result.data

    # 续轮请求（工具结果之后的第 2 次请求）里，tool 消息带着文件内容。
    view = probe.context(FILES_MODEL, 1)
    tool_messages = [m for m in view.body["messages"] if m.get("role") == "tool"]
    assert tool_messages, view.describe()
    assert any(
        "hello from probe" in str(m.get("content") or "") for m in tool_messages
    ), view.describe()
    session.watch.assert_never("error")


# ── 场景 3：幂等与不覆盖 ───────────────────────────────────────


@pytest.mark.probe_env(models=[FILES_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_same_name_never_overwrites_and_same_bytes_is_idempotent(
    probe: Probe,
) -> None:
    """同名同字节 → 同路径不重写；同名异字节 → 递增后缀且首份字节原样保留。"""
    session = await seed(probe)
    sid = session.session_id

    first = await upload(probe, sid, b"one", name="report.pdf")
    before = Path(first["file"]["path"]).stat()
    again = await upload(probe, sid, b"one", name="report.pdf")
    after = Path(again["file"]["path"]).stat()

    assert again["file"]["path"] == first["file"]["path"], again
    assert (after.st_ino, after.st_mtime_ns) == (
        before.st_ino,
        before.st_mtime_ns,
    ), "同名同字节重写了文件"

    second = await upload(probe, sid, b"two", name="report.pdf")
    third = await upload(probe, sid, b"three", name="report.pdf")

    assert [item["file"]["name"] for item in (first, second, third)] == [
        "report.pdf",
        "report-2.pdf",
        "report-3.pdf",
    ]
    assert (files_dir(probe, sid) / "report.pdf").read_bytes() == b"one", (
        "同名异字节覆盖了首份（链上的 [file: …] 引用会指向另一份字节）"
    )
    assert (files_dir(probe, sid) / "report-2.pdf").read_bytes() == b"two"
    assert (files_dir(probe, sid) / "report-3.pdf").read_bytes() == b"three"
    assert file_names(probe, sid) == ["report-2.pdf", "report-3.pdf", "report.pdf"]


# ── 场景 3b：文件名别名（大小写 / 归一化不敏感 FS） ─────────────


@pytest.mark.probe_env(models=[FILES_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_alias_spelling_resolves_to_the_on_disk_file_name(probe: Probe) -> None:
    """请求名是别名（大小写不敏感 FS）时，返回的 name/path 是**磁盘真名**。

    敏感 FS（CI 的 Linux）上别名不是同一个文件——按"两个名字"处理（各自落盘），
    同样不得把请求名当成磁盘事实。两个分支各自断言（比 skip 强：敏感 FS 也能守住
    "响应与磁盘一致"）。
    """
    session = await seed(probe)
    sid = session.session_id

    first = await upload(probe, sid, b"one", name="Report.pdf")
    assert first["file"]["name"] == "Report.pdf", first
    assert (files_dir(probe, sid) / "Report.pdf").read_bytes() == b"one"

    if not fs_is_case_insensitive(probe.env.sessions_path):
        # 敏感 FS：别名是另一个文件名 → 同名同字节但**不同名** → 新落一份。
        same_bytes = await upload(probe, sid, b"one", name="report.pdf")
        assert same_bytes["file"]["name"] == "report.pdf", same_bytes
        assert file_names(probe, sid) == ["Report.pdf", "report.pdf"]
        return

    # 不敏感 FS：解析到同一个目录项 → 幂等命中**磁盘真名**（不产生第二份、不谎报名字）。
    same_bytes = await upload(probe, sid, b"one", name="report.pdf")
    assert same_bytes["file"]["name"] == "Report.pdf", same_bytes
    assert same_bytes["file"]["path"] == first["file"]["path"], same_bytes

    # 别名 + 异字节 → 冲突递增（以请求名生成后缀），原文件字节原样保留。
    second = await upload(probe, sid, b"two", name="report.pdf")
    assert second["file"]["name"] == "report-2.pdf", second
    assert (files_dir(probe, sid) / "Report.pdf").read_bytes() == b"one"

    # 候选名的别名拼写同样解析到真名（不再产生第三份）。
    replayed = await upload(probe, sid, b"two", name="REPORT-2.PDF")
    assert replayed["file"]["name"] == "report-2.pdf", replayed
    assert file_names(probe, sid) == ["Report.pdf", "report-2.pdf"]


# ── 场景 4：拒绝零副作用 ────────────────────────────────────────


@pytest.mark.probe_env(models=[FILES_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_rejected_uploads_leave_no_trace(probe: Probe) -> None:
    """坏名字 / 空 body / 缺 name → 400，文件目录不存在；随后合法上传照常。"""
    session = await seed(probe)
    sid = session.session_id

    for bad, marker in (
        ("../escape", "invalid file name"),
        (".hidden", "invalid file name"),
        ("a/b", "invalid file name"),
        ("a\x01b", "invalid file name"),
        ("n" * 129, "invalid file name"),
    ):
        with pytest.raises(DriverHttpError) as failure:
            await upload(probe, sid, b"x", name=bad)
        call = failure.value.call
        assert call.status == 400, call.render()
        assert marker in str((call.response or {}).get("detail", "")), call.render()
        assert not files_dir(probe, sid).exists(), "400 路径在存储上留下了痕迹"

    # 缺 name（query 参数缺席）/ 空 body。
    with pytest.raises(DriverHttpError) as failure:
        await upload(probe, sid, b"x", name=None)
    assert failure.value.call.status == 400, failure.value.call.render()
    assert "file name is required" in str(failure.value.call.response["detail"])

    with pytest.raises(DriverHttpError) as failure:
        await upload(probe, sid, b"", name="empty.txt")
    assert failure.value.call.status == 400, failure.value.call.render()
    assert not files_dir(probe, sid).exists()

    # 闸门没有把会话 / 存储弄坏：合法字节随后照常入库。
    accepted = await upload(probe, sid, CONTENT, name="ok.txt")
    assert accepted["file"]["name"] == "ok.txt"
    assert file_names(probe, sid) == ["ok.txt"]

    # 路径穿越的名字没有在别处落下任何东西。
    assert not (probe.env.sessions_path.parent / "escape").exists()


# ── 场景 5：超限（真实字节 + Content-Length 预检） ──────────────


@pytest.mark.probe_env(models=[FILES_SPEC], files=SMALL_LIMIT_FILES)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_oversize_upload_is_rejected_by_config_limit(probe: Probe) -> None:
    """超 ``files.max_bytes``（注入 16 B）→ 400；小载荷仍可上传、目录不增文件。"""
    session = await seed(probe)
    small = b"tiny"  # 4 B
    big = b"x" * 100
    assert len(small) <= SMALL_LIMIT_FILES["max_bytes"] < len(big)

    accepted = await upload(probe, session.session_id, small, name="small.txt")
    assert file_names(probe, session.session_id) == ["small.txt"]

    with pytest.raises(DriverHttpError) as failure:
        await upload(probe, session.session_id, big, name="big.txt")
    call = failure.value.call
    assert call.status == 400, call.render()
    detail = str((call.response or {}).get("detail", ""))
    assert "file too large" in detail and "files.max_bytes" in detail, detail
    assert file_names(probe, session.session_id) == ["small.txt"], (
        "超限上传在会话文件目录里留下了文件"
    )
    assert accepted["file"]["bytes"] == len(small)


async def raw_upload_head(
    probe: Probe, session_id: str, name: str, declared: int
) -> tuple[int, str]:
    """只写请求头（声明 ``declared`` 字节）、一个 body 字节都不发的上传请求。

    这是"先校验后收包"的观测面：实现若先把 ``await request.body()`` 跑完
    （Starlette 的 ``body()`` 没有大小上限），这里会一直等一个永远不会来（也不该来）
    的 body——读超时即断言失败。
    """
    assert probe.env.port is not None, "网关还没起来"
    endpoint = urlsplit(probe.env.gateway_url)
    host = endpoint.hostname or "127.0.0.1"
    port = endpoint.port or probe.env.port
    reader, writer = await asyncio.open_connection(host, port)
    try:
        writer.write(
            (
                f"POST /api/session/files?session_id={quote(session_id)}"
                f"&name={quote(name)} HTTP/1.1\r\n"
                f"Host: {host}:{port}\r\n"
                "Content-Type: application/octet-stream\r\n"
                f"Content-Length: {declared}\r\n"
                "Connection: close\r\n"
                "\r\n"
            ).encode()
        )
        await writer.drain()
        try:
            raw = await asyncio.wait_for(reader.read(-1), timeout=NO_BODY_WAIT_SECONDS)
        except TimeoutError:
            pytest.fail("拒绝必须发生在收包之前：body 一个字节都没发，响应却迟迟不回来")
    finally:
        writer.close()
        with contextlib.suppress(Exception):
            await writer.wait_closed()
    text = raw.decode("utf-8", errors="replace")
    assert text.startswith("HTTP/"), f"响应不是 HTTP 报文：{text!r}"
    return int(text.split(" ", 2)[1]), text


@pytest.mark.probe_env(models=[FILES_SPEC], files=SMALL_LIMIT_FILES)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_upload_rejects_by_headers_before_reading_the_body(probe: Probe) -> None:
    """收包顺序（协议面）：会话存在性 → 名字闸门 → ``Content-Length`` 预检 → 收包。

    - 幽灵会话 + 声明超限 → **404**（存在性先于一切内容校验）；
    - 坏名字 + 声明超限 → **400**（名字是 query 参数，先于收包）；
    - 真实会话 + 声明超限 → **400**（按声明值拒绝，detail 与字节闸门同一句文案），
      存储零痕迹。

    三个请求都一个 body 字节都不发：响应只能来自"读包之前"的判定。
    """
    session = await seed(probe)
    declared = SMALL_LIMIT_FILES["max_bytes"] + 1

    ghost_status, ghost_raw = await raw_upload_head(
        probe, "ghost-files-upload", "a.txt", declared
    )
    assert ghost_status == 404, ghost_raw
    assert "session not found" in ghost_raw, ghost_raw
    assert not probe.env.session_dir("ghost-files-upload").exists()

    name_status, name_raw = await raw_upload_head(
        probe, session.session_id, "../escape", declared
    )
    assert name_status == 400, name_raw
    assert "invalid file name" in name_raw, name_raw

    status, raw = await raw_upload_head(probe, session.session_id, "a.txt", declared)
    assert status == 400, raw
    assert "file too large" in raw and "files.max_bytes" in raw, raw
    assert not files_dir(probe, session.session_id).exists(), (
        "预检 400 在存储上留下了痕迹"
    )


# ── 场景 6：幽灵会话 / 归档会话 ─────────────────────────────────


@pytest.mark.probe_env(models=[FILES_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_ghost_session_upload_is_404(probe: Probe) -> None:
    """会话不存在（含路径穿越式 id）→ 404，且不产生任何目录残留。"""
    for ghost in ("ghost-files-2", "../escape-files"):
        with pytest.raises(DriverHttpError) as failure:
            await upload(probe, ghost, CONTENT, name="a.txt")
        call = failure.value.call
        assert call.status == 404, call.render()
        assert (call.response or {}).get("detail") == "session not found", call.render()

    assert not probe.env.session_dir("ghost-files-2").exists()
    assert not (probe.env.sessions_path.parent / "escape-files").exists()


@pytest.mark.probe_env(models=[FILES_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_archived_session_upload_is_404_with_the_marker(probe: Probe) -> None:
    """归档会话上传 → 404 + ``error="session_archived"``（可行动提示），零痕迹。"""
    session = await seed(probe)
    http = probe.driver_required.http

    await http.unsubscribe(session.session_id, probe.driver_required.client_id)
    archived = await http.request(
        "POST",
        "/api/session/archive",
        body={"session_id": session.session_id, "archived": True},
    )
    assert archived["changed"] is True, archived

    with pytest.raises(DriverHttpError) as failure:
        await upload(probe, session.session_id, CONTENT, name="late.txt")
    call = failure.value.call
    assert call.status == 404, call.render()
    assert (call.response or {}).get("error") == "session_archived", call.render()
    assert "restore" in str(call.response.get("detail", "")), call.render()
    assert not (
        probe.env.sessions_path / ".archive" / session.session_id / "files"
    ).exists()

    # 恢复：同一会话照常可上传（归档没销毁任何东西）。
    restored = await http.request(
        "POST",
        "/api/session/archive",
        body={"session_id": session.session_id, "archived": False},
    )
    assert restored["changed"] is True, restored
    accepted = await upload(probe, session.session_id, CONTENT, name="late.txt")
    assert accepted["file"]["name"] == "late.txt"


# ── 场景 7：memory 后端（path 如实为 null） ─────────────────────


@pytest.mark.probe_env(models=[FILES_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_memory_backend_has_no_path(probe: Probe) -> None:
    """memory 后端没有本地文件 → ``path`` 为 null（如实降级，不编路径）。"""
    probe.register(FILES_MODEL, Turn.of(text="seeded"))
    session = await probe.session(model=FILES_MODEL, backend="memory")
    await session.chat("seed the history")

    response = await upload(probe, session.session_id, CONTENT, name="notes.txt")

    assert response["file"] == {
        "name": "notes.txt",
        "path": None,
        "bytes": len(CONTENT),
    }, response
    # 没有本地文件可查（会话目录不存在）——这不是缺陷，是 memory 后端的语义。
    assert not probe.env.session_dir(session.session_id).exists()
