"""会话血缘（`parent_session_id`）数据面场景。

被守的契约（`docs/dev/session-lineage.md` 的 §1 / §2，实现落点
`libs/core/wing/{store/base.py,session/manager.py,gateway/**}`）：

- ``POST /api/session/create`` 的 ``parent_session_id`` 是**创建时刻的一次性
  事实**：带父创建把字段写进 ``metadata.json``（``read_metadata`` 可读），
  列表行（``GET /api/session/list``）与 ``GET /api/session/info`` 都投影它，
  响应的 ``parent_session_id`` 是**读回**（调用方据此对账）；
- **持久**：逐出（release）→ 列表行仍在（``status: inactive``）→ resume 水合
  后字段照旧；
- **弱引用**：只做格式卫生（同 session id 规则），不校验父会话存在性——幽灵父
  被接受，事实就是该值；与 ``session_id`` 字面相等（自指）→ 400；
- **子集语义**：``?parent=<sid>`` 只保留**直接子**（parent 逐字相等——不含父
  自己、不含孙辈），结果是全量列表的子序列（同样的行、同样的相对顺序）；
  词法非法的过滤值 → 400（空结果会掩盖笔误）；
- **收养路径忽略 parent**：create-or-adopt 命中已存在 id 时走 resume 语义，
  既有记录不被请求覆盖（响应仍反映既有记录）；
- **无父创建零字段**：``metadata.json`` 里没有该键，列表行为 null。（存量
  行为逐字不变。）

为什么要有它（动机）：Bash 工具给每个 agent 的 shell 注入
``WING_SESSION_ID``，CLI 把它填进创建请求——「Session B 由 Session A 创建」
是任务树 / 团队视图的唯一素材。这条链路的后端一半（持久 + 查询 + 投影）在
本场景里被钉死，CLI 侧的采集 / 展示是 02 / 03 lane 的域。
"""

from __future__ import annotations

import json
from collections.abc import Mapping

import pytest

from wing_probe import Probe, Turn, read_metadata
from wing_probe.driver import DriverHttpError

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
LINEAGE_MODEL = "probe/lineage"

#: 血缘字段名（线格式 / 落盘键，断言里反复出现）。
FIELD = "parent_session_id"

#: 让会话在列表里可见的标签（无名且无标的会话本就不进列表）。
VISIBLE_TAG = "lineage"


async def _create(
    probe: Probe,
    *,
    session_id: str | None = None,
    parent: str | None = None,
    model_id: str | None = None,
    tags: list[str] | None = None,
) -> dict:
    """``POST /api/session/create``（留档调用；非 2xx 抛 ``DriverHttpError``）。"""
    body: dict = {}
    if session_id is not None:
        body["session_id"] = session_id
    if parent is not None:
        body[FIELD] = parent
    if tags is not None:
        body["tags"] = tags
    if model_id is not None:
        body["agent"] = {"model_id": model_id}
    return await probe.driver_required.http.request(
        "POST", "/api/session/create", body=body
    )


async def _list(probe: Probe, **filters: str) -> list[dict]:
    """``GET /api/session/list``（可带子集过滤）；返回原始的 ``sessions`` 行列表。"""
    params: Mapping[str, str] = {k: v for k, v in filters.items() if v is not None}
    payload = await probe.driver_required.http.request(
        "GET", "/api/session/list", params=params or None
    )
    return payload["sessions"]


async def _info(probe: Probe, session_id: str) -> dict:
    """``GET /api/session/info``（要求会话在内存；已逐出时端点回 404）。"""
    return await probe.driver_required.http.request(
        "GET", "/api/session/info", params={"session_id": session_id}
    )


def _ids(rows: list[dict]) -> list[str]:
    return [row["id"] for row in rows]


def _rows_by_id(rows: list[dict]) -> dict[str, dict]:
    return {row["id"]: row for row in rows}


def _metadata(probe: Probe, session_id: str) -> dict:
    """会话 ``metadata.json`` 的解析结果（缺失即失败——断言不静默放行）。"""
    data = read_metadata(probe.env.session_dir(session_id))
    assert data is not None, f"metadata.json missing for {session_id!r}"
    return data


@pytest.mark.probe_env(models=[LINEAGE_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_parent_is_recorded_and_projected(probe: Probe) -> None:
    """带父创建：metadata 落盘 + 列表行与 info 都携带；无父创建零字段。

    WHEN 带父创建 child、不带父创建 root
    THEN 响应是**读回**、``metadata.json`` 有 / 无该键、列表行与 info 逐字一致。
    """
    root = await _create(probe, session_id="lineage-root", tags=[VISIBLE_TAG])
    assert root[FIELD] is None, root

    child = await _create(
        probe,
        session_id="lineage-child",
        parent="lineage-root",
        model_id=LINEAGE_MODEL,
        tags=[VISIBLE_TAG],
    )
    assert child["session_id"] == "lineage-child", child
    assert child[FIELD] == "lineage-root", child

    # 持久事实：创建即落盘（不依赖后续任何动作）。
    assert _metadata(probe, "lineage-child")[FIELD] == "lineage-root"
    assert FIELD not in _metadata(probe, "lineage-root")

    rows = _rows_by_id(await _list(probe))
    assert rows["lineage-child"][FIELD] == "lineage-root", rows["lineage-child"]
    assert rows["lineage-root"][FIELD] is None, rows["lineage-root"]

    info = await _info(probe, "lineage-child")
    assert info[FIELD] == "lineage-root", info


@pytest.mark.probe_env(models=[LINEAGE_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_parent_survives_eviction_and_resume(probe: Probe) -> None:
    """逐出 + 水合后血缘仍在：跑过一轮的 child 被 release，resume 后字段照旧。

    逐出只回收内存态（``metadata.json`` 不动）——列表行仍带字段（inactive），
    info 因不自动水合回 404，resume 之后同一个值再读回来。
    """
    probe.register(LINEAGE_MODEL, Turn.of(text="child reply"))
    driver = probe.driver_required
    created = await _create(
        probe,
        session_id="lineage-child",
        parent="lineage-root",
        model_id=LINEAGE_MODEL,
        tags=[VISIBLE_TAG],
    )
    assert created[FIELD] == "lineage-root", created

    session = await driver.attach("lineage-child", workspace=probe.workspace)
    result = await session.chat("spawned by the parent")
    assert result.data["subtype"] == "success", result.data

    # 逐出（订阅会钉住会话 → 先退订再 release）。
    await driver.http.unsubscribe("lineage-child", driver.client_id)
    released = await driver.http.release_session("lineage-child")
    assert released["released"] is True, released

    # 不在内存：info 不自动水合（404），列表行照旧携带血缘。
    with pytest.raises(DriverHttpError) as failure:
        await _info(probe, "lineage-child")
    assert failure.value.status == 404, failure.value.call.render()
    rows = _rows_by_id(await _list(probe))
    assert rows["lineage-child"][FIELD] == "lineage-root", rows["lineage-child"]
    assert rows["lineage-child"]["status"] == "inactive", rows["lineage-child"]

    # 水合（resume）后同一个事实再读回来。
    await driver.resume("lineage-child")
    assert (await _info(probe, "lineage-child"))[FIELD] == "lineage-root"
    assert _metadata(probe, "lineage-child")[FIELD] == "lineage-root"


@pytest.mark.probe_env(models=[LINEAGE_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_parent_filter_selects_direct_children(probe: Probe) -> None:
    """``?parent=`` = 直接子（不含父自己、不含孙辈），且是子集过滤。

    WHEN 建 A → B → C 三层
    THEN ``?parent=A`` 只有 B、``?parent=B`` 只有 C、``?parent=C`` 空；
    每个查询都逐行等于"全量 + 谓词"（同样的行序）；词法非法值 400。
    """
    await _create(probe, session_id="lineage-a", tags=[VISIBLE_TAG])
    await _create(probe, session_id="lineage-b", parent="lineage-a", tags=[VISIBLE_TAG])
    await _create(probe, session_id="lineage-c", parent="lineage-b", tags=[VISIBLE_TAG])

    full = await _list(probe)
    assert _ids(full) == ["lineage-a", "lineage-b", "lineage-c"], full

    assert _ids(await _list(probe, parent="lineage-a")) == ["lineage-b"]
    assert _ids(await _list(probe, parent="lineage-b")) == ["lineage-c"]
    assert _ids(await _list(probe, parent="lineage-c")) == []

    # 子集语义：结果 == 全量 + 谓词（逐行相等，含行序）；格式合法但不存在的父
    # 是空结果（弱引用 + 词表开放，不是错误）。
    for value in ("lineage-a", "lineage-b", "lineage-nowhere"):
        filtered = await _list(probe, parent=value)
        assert filtered == [row for row in full if row[FIELD] == value], value
    assert await _list(probe, parent="lineage-nowhere") == []

    # 与其它过滤组合即 AND。
    assert _ids(await _list(probe, parent="lineage-a", tag=VISIBLE_TAG)) == [
        "lineage-b"
    ]
    assert await _list(probe, parent="lineage-a", tag="no-such-tag") == []

    # 词法非法（空 / 穿越 / 点开头 / 超长）→ 400：这类值不可能命中任何行。
    for bad in ("", "../escape", "a/b", ".media", "x" * 129):
        with pytest.raises(DriverHttpError) as failure:
            await _list(probe, parent=bad)
        assert failure.value.status == 400, (bad, failure.value.call.render())
        assert "parent filter" in json.dumps(failure.value.call.response), (
            bad,
            failure.value.call.render(),
        )

    # 重复参数同样 400（FastAPI 默认 last-wins 会静默丢值）。
    with pytest.raises(DriverHttpError) as failure:
        await probe.driver_required.http.request(
            "GET",
            "/api/session/list",
            params={"parent": ["lineage-a", "lineage-b"]},
        )
    assert failure.value.status == 400, failure.value.call.render()


@pytest.mark.probe_env(models=[LINEAGE_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_invalid_parent_is_rejected_without_traces(probe: Probe) -> None:
    """非法 parent / 自指 → 400 且磁盘零痕迹；幽灵父（弱引用）被接受。"""
    for bad in (
        "../escape",
        "/tmp/absolute",
        "a/b",
        ".media",
        "a..b",
        "bad\x01id",  # ASCII 控制字符
        "x" * 129,  # 超字节上限（ASCII）
        "收" * 128,  # 384 字节：按字符限长会误放行
    ):
        with pytest.raises(DriverHttpError) as failure:
            await _create(probe, parent=bad)
        assert failure.value.status == 400, (bad, failure.value.call.render())
        assert "parent session id" in json.dumps(failure.value.call.response), (
            bad,
            failure.value.call.render(),
        )

    # 自指：session_id 与 parent 字面相等 → 400（不被 create-or-adopt 吞掉），
    # 同样零痕迹（闸门在任何写盘之前）。
    with pytest.raises(DriverHttpError) as failure:
        await _create(probe, session_id="lineage-self", parent="lineage-self")
    assert failure.value.status == 400, failure.value.call.render()

    assert sorted(entry.name for entry in probe.env.sessions_path.iterdir()) == []
    assert await _list(probe) == []

    # 弱引用：格式合法但**不存在**的父被接受，事实就是该值。
    ghost = await _create(
        probe,
        session_id="lineage-ghost",
        parent="lineage-nowhere",
        tags=[VISIBLE_TAG],
    )
    assert ghost[FIELD] == "lineage-nowhere", ghost
    assert _metadata(probe, "lineage-ghost")[FIELD] == "lineage-nowhere"
    assert _ids(await _list(probe, parent="lineage-nowhere")) == ["lineage-ghost"]


@pytest.mark.probe_env(models=[LINEAGE_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_adopt_ignores_parent_and_never_rewrites(probe: Probe) -> None:
    """create-or-adopt 命中已存在 id：请求里的 parent 被忽略，既有记录不被改写。

    收养走 resume 语义（既有会话的 metadata 是唯一事实）——无父的会话不会因为
    请求带了 parent 而"认父"，已有父的会话也不会被换父；响应是**读回**既有记录。
    忽略是显式的：日志里留一行 warning。
    """
    first = await _create(probe, session_id="lineage-root", tags=[VISIBLE_TAG])
    assert first[FIELD] is None, first

    adopted = await _create(
        probe, session_id="lineage-root", parent="lineage-other", tags=[VISIBLE_TAG]
    )
    assert adopted["session_id"] == "lineage-root", adopted
    assert adopted[FIELD] is None, adopted  # 读回既有记录（无父），而非请求值
    assert FIELD not in _metadata(probe, "lineage-root")

    child = await _create(
        probe,
        session_id="lineage-kid",
        parent="lineage-root",
        tags=[VISIBLE_TAG],
    )
    assert child[FIELD] == "lineage-root", child
    reparented = await _create(probe, session_id="lineage-kid", parent="lineage-other")
    assert reparented[FIELD] == "lineage-root", reparented
    assert _metadata(probe, "lineage-kid")[FIELD] == "lineage-root"

    rows = _rows_by_id(await _list(probe))
    assert rows["lineage-root"][FIELD] is None, rows["lineage-root"]
    assert rows["lineage-kid"][FIELD] == "lineage-root", rows["lineage-kid"]

    # 忽略必须留痕（编排方能在日志里看到 parent 没生效）。
    log = (probe.env.wing_home / "core" / "logs" / "new.log").read_text(
        encoding="utf-8"
    )
    assert "ignores parent session id" in log, log[-2000:]
