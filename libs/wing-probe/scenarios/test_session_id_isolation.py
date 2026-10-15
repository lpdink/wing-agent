"""Session ID 闸门场景：越界写防护 + 列表隔离（安全红线）。

被守的语义（``common.utils.is_valid_session_id`` → ``SessionManager._resolve_with_store``
→ ``FileSessionStore`` 路径拼接点，三道一致的硬闸门）：

- session id 由后端自生成或**由编排方自带**（create-or-adopt），闸门只防路径
  穿越与卫生：含 ``/`` / ``\\`` / ``..``、点开头（存储保留的 ``.media`` 媒体池）、
  ASCII 控制字符、空串、超长的值一律拒绝；不合规的值按"不存在"处理（404）——
  **绝不进入任何 store 路径拼接**；
- 同源证据：在 store root **之外**构造一个内容完整的诱饵会话目录
  （history.jsonl + metadata.json），用相对穿越（``../escape``）与绝对路径两种
  形态打 tag / resume / send 端点——请求一律 404，且诱饵文件逐字节不变
  （没有闸门时，tag 的 store 直写路径会解析到这个目录并**整体覆盖** metadata）；
- 不给探测反馈：闸门拒绝与真不存在无从区分（都是 404）；
- 列表隔离：sessions root 里**不过闸门**的杂物目录（哪怕带 history.jsonl）不会
  被 ``/api/session/list`` 列出，也不会让列表接口崩掉。
"""

from __future__ import annotations

import json

import pytest

from wing_probe import Probe
from wing_probe.driver import DriverHttpError

#: 不过闸门的杂物目录名（点开头 / 含 ``..``）——"任意安全字符串"现在是合法 id，
#: 所以这里必须用新闸门仍然拒绝的形态（见 ``test_session_id_validation.py`` 的矩阵）。
JUNK_NAMES = (".hidden-junk", "..junk", "x" * 129)


def _craft_decoy(probe: Probe) -> tuple[str, str]:
    """在 sessions root 的同级构造"可被穿越命中"的诱饵目录，返回 (相对路径形态, 绝对路径形态)。

    相对形态 ``../escape`` 从 sessions root 出发恰好解析到该目录——即旧实现
    里 tag 端点可越界覆盖的落点。
    """
    escape_dir = probe.env.sessions_path.parent / "escape"
    escape_dir.mkdir(parents=True, exist_ok=True)
    (escape_dir / "history.jsonl").write_text(
        json.dumps({"role": "user", "content": "decoy", "uuid": "decoy-u1"}) + "\n",
        encoding="utf-8",
    )
    (escape_dir / "metadata.json").write_text(
        json.dumps({"session_name": "decoy"}), encoding="utf-8"
    )
    return "../escape", str(escape_dir)


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_traversal_ids_are_rejected_without_side_effects(probe: Probe) -> None:
    """越界 id 一律 404 且零副作用：诱饵文件逐字节不变。

    WHEN 用 ``../escape``（相对穿越）与诱饵目录的绝对路径分别打 tag / resume / send
    THEN 全部 404；诱饵的 metadata.json / history.jsonl 内容不被读取路径改写
    （tag 的 store 直写若可达，会把 metadata 整体覆盖成 tags-only）。
    """
    relative, absolute = _craft_decoy(probe)
    metadata = probe.env.sessions_path.parent / "escape" / "metadata.json"
    history = probe.env.sessions_path.parent / "escape" / "history.jsonl"
    before_meta, before_hist = metadata.read_bytes(), history.read_bytes()

    driver = probe.driver_required
    for sid in (relative, absolute):
        with pytest.raises(DriverHttpError) as failure:
            await driver.http.request(
                "POST", "/api/session/tag", body={"session_id": sid, "add": ["pwned"]}
            )
        assert failure.value.status == 404, failure.value.call.render()

        with pytest.raises(DriverHttpError) as failure:
            await driver.http.request(
                "POST", "/api/session/resume", body={"session_id": sid}
            )
        assert failure.value.status == 404, failure.value.call.render()

        with pytest.raises(DriverHttpError) as failure:
            await driver.http.request(
                "POST", "/api/session/send", body={"session_id": sid, "content": "x"}
            )
        assert failure.value.status == 404, failure.value.call.render()

    assert metadata.read_bytes() == before_meta
    assert history.read_bytes() == before_hist


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_nonconforming_dirs_are_not_sessions(probe: Probe) -> None:
    """不过闸门的目录不是会话：不列出、可寻址性为零、不炸列表。"""
    for name in JUNK_NAMES:
        junk = probe.env.sessions_path / name
        junk.mkdir(parents=True, exist_ok=True)
        (junk / "history.jsonl").write_text(
            json.dumps({"role": "user", "content": "junk", "uuid": "junk-u1"}) + "\n",
            encoding="utf-8",
        )

    driver = probe.driver_required
    payload = await driver.http.request("GET", "/api/session/list")
    ids = [entry["id"] for entry in payload.get("sessions", [])]
    for name in JUNK_NAMES:
        assert name not in ids, ids

        # 寻址同样被闸门拒绝（404，而不是 500 / 也不命中杂物目录）
        with pytest.raises(DriverHttpError) as failure:
            await driver.http.request(
                "POST",
                "/api/session/tag",
                body={"session_id": name},
            )
        assert failure.value.status == 404, failure.value.call.render()

        # create-or-adopt 同样不得把这些名字变成会话（且不留痕迹）
        with pytest.raises(DriverHttpError) as failure:
            await driver.http.request(
                "POST",
                "/api/session/create",
                body={"session_id": name},
            )
        assert failure.value.status == 400, failure.value.call.render()
        assert (junk / "metadata.json").exists() is False


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_broken_metadata_directory_does_not_break_listing(probe: Probe) -> None:
    """坏 metadata（合法 JSON / 非法 schema）目录不能让 /api/session/list 500（r2 S-1）。

    WHEN sessions root 里手工放入两个合法目录名的坏目录：一个无 history 只有
    `{"tags": "abc"}`，一个带 history 但 `{"tags": 42}`
    THEN 列表接口 200：前者按"无内容"跳过、后者按"损坏降级"照常列出。
    """
    root = probe.env.sessions_path

    no_history = root / "20250101-000000-abcdef09"
    no_history.mkdir(parents=True, exist_ok=True)
    (no_history / "metadata.json").write_text(
        json.dumps({"tags": "abc"}), encoding="utf-8"
    )

    with_history = root / "20250101-000000-abcdef0a"
    with_history.mkdir(parents=True, exist_ok=True)
    (with_history / "history.jsonl").write_text(
        json.dumps({"role": "user", "content": "hello", "uuid": "h-u1"}) + "\n",
        encoding="utf-8",
    )
    (with_history / "metadata.json").write_text(
        json.dumps({"tags": 42}), encoding="utf-8"
    )

    driver = probe.driver_required
    payload = await driver.http.request("GET", "/api/session/list")
    ids = [entry["id"] for entry in payload.get("sessions", [])]
    assert "20250101-000000-abcdef09" not in ids, ids
    assert "20250101-000000-abcdef0a" in ids, ids
