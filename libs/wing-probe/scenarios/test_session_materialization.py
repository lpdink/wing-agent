"""会话物化：只打开不用的会话在磁盘上零痕迹。

被守的不变量（本次变更的核心；文档落点见 ``docs/dev/usage.md`` 与
``docs/dev/architecture.md`` 的会话生命周期一节）：

- **建会话不落盘**：``POST /api/session/create``（TUI 的形态）不建目录、不写
  ``metadata.json`` / ``history.jsonl``；创建期的覆盖（模型 / 工具 / 提示词）、
  血缘（``parent_session_id``）与打标都只是内存草稿；
- **第一条用户消息是出生时刻**：一条用户消息到来 → 目录 + ``metadata.json``
  （标题 / 血缘 / 模板 / last_interaction）+ ``history.jsonl`` 一次出现；
- **用量目录跟着物化走**：``session_catalog`` 也在此刻登记（空会话不写行）；
- **只读端点不发明行**：``/api/session/list`` 不因空会话多出一行。

禁止 import ``wing``（probe 门禁）：一切经公开 HTTP / WS 观测，落库事实用**只读**
sqlite3 连接交叉对账。
"""

from __future__ import annotations

import json

import pytest

from wing_probe import Probe, Turn, Usage, read_only_connect

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
MODEL = "probe/materialize"

#: ``history.jsonl`` 的落盘名（与 ``FileMessageLog`` 一致）。
_HISTORY = "history.jsonl"
_METADATA = "metadata.json"


def _catalog_rows(probe: Probe) -> list[dict]:
    """只读对账 ``session_catalog``（端点之外的第二视角；库还没建 = 没有行）。"""
    path = probe.env.wing_home / "core" / "wing.db"
    if not path.exists():
        return []
    conn = read_only_connect(path)
    try:
        return [
            dict(row)
            for row in conn.execute("SELECT * FROM session_catalog ORDER BY rowid")
        ]
    finally:
        conn.close()


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_an_unused_session_leaves_no_trace(probe: Probe) -> None:
    """建会话 + 订阅 + 读 info：磁盘上仍然什么都没有（空会话不是会话）。

    会话在内存里是**可用**的（``info`` 回得出它的模型），只是还没有落盘：
    TUI 开了不用就退出因此不留下目录 / metadata / history，也不进用量目录。
    """
    driver = probe.driver_required
    session = await probe.session(model=MODEL)
    session_dir = probe.env.session_dir(session.session_id)

    # 内存里可用：info 认得它（不是 404）
    info = await driver.http.request(
        "GET", "/api/session/info", params={"session_id": session.session_id}
    )
    assert info["model_id"] == MODEL

    # 磁盘上零痕迹
    assert not session_dir.exists(), session_dir
    assert not (session_dir / _METADATA).exists()
    assert not (session_dir / _HISTORY).exists()

    # 列表与用量目录都不认它（无名且无标的会话本来就不进列表；目录行跟着物化走）
    listed = await driver.http.request("GET", "/api/session/list")
    assert session.session_id not in {row["id"] for row in listed["sessions"]}
    assert _catalog_rows(probe) == []


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_explicit_actions_before_the_first_message_stay_in_memory(
    probe: Probe,
) -> None:
    """切换模型 / 打标 / 带父创建：全是草稿，第一条消息之前不落盘。

    这些动作本身是「显式」的，但它们作用在一个还没有出生的会话上——磁盘上
    没有可写的事实。第一条消息到来时整份 metadata 一次落盘（血缘与标签都在）。
    """
    driver = probe.driver_required
    created = await driver.http.request(
        "POST",
        "/api/session/create",
        body={"parent_session_id": "root-a", "agent": {"model_id": MODEL}},
    )
    session_id = created["session_id"]
    session = await driver.attach(session_id)

    assert (
        await driver.http.request(
            "POST",
            "/api/session/tag",
            body={"session_id": session_id, "add": ["draft"]},
        )
    )["tags"] == ["draft"]
    assert not probe.env.session_dir(session_id).exists()
    assert _catalog_rows(probe) == []

    probe.register(MODEL, Turn.of(text="ok", usage=Usage(prompt_tokens=10)))
    assert (await session.chat("first")).data["subtype"] == "success"

    session_dir = probe.env.session_dir(session_id)
    metadata = json.loads((session_dir / _METADATA).read_text(encoding="utf-8"))
    assert metadata["session_name"] == "first"
    assert metadata["parent_session_id"] == "root-a"
    assert metadata["tags"] == ["draft"]
    assert metadata["template_name"] is not None
    assert metadata["last_interaction"]

    catalog = _catalog_rows(probe)
    assert [row["session_id"] for row in catalog] == [session_id]
    assert catalog[0]["parent_session_id"] == "root-a"


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_the_first_message_persists_the_whole_session(probe: Probe) -> None:
    """第一条用户消息 → 目录 / metadata / history 一次出现，标题就是那条消息。"""
    probe.register(
        MODEL,
        Turn.of(text="hello back", usage=Usage(prompt_tokens=20, completion_tokens=2)),
    )
    session = await probe.session(model=MODEL, yolo=True)
    session_dir = probe.env.session_dir(session.session_id)
    assert not session_dir.exists()

    result = await session.chat("hello")
    assert result.data["subtype"] == "success", result.data

    assert session_dir.is_dir()
    metadata = json.loads((session_dir / _METADATA).read_text(encoding="utf-8"))
    assert metadata["session_name"] == "hello"
    assert metadata["last_interaction"]
    # 历史里有那条用户消息（物化与消息追加同源；首行可能是轮次事件）
    records = [
        json.loads(line)
        for line in (session_dir / _HISTORY).read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    user = [record for record in records if record.get("role") == "user"]
    assert user and user[0]["content"] == "hello"
