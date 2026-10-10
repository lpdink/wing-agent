"""会话列表条目的**模型四件套**（``GET /api/session/list``）。

列表是跨会话的唯一视图（``wing ps`` / 会话面板都读它）：「这个会话在跑什么模型」
必须随条目一起下发，而不是逐会话去拉 ``/api/session/info``（N+1）。口径与
``/api/session/info`` 逐字段一致——``model_id`` 是身份（引用词）、``model_name``
是发给上游的调用名、``provider_name`` 是承载它的 provider、
``model_display_name`` 是声明里的展示名（未声明 = null，展示层回落调用名）。

覆盖的形态：

- **active / inactive 给同一个答案**：同一个会话在内存时取 live agent、逐出后按
  resume 链解析盘上记录，两次列表的模型四件套必须逐字段相同（并且与 resume 之后
  ``/api/session/info`` 相同——列表说 A、resume 真跑 B 是同一件事的两种说法）；
- **旧记录**（无 ``model_id``）：快照兜底 + ``Config.identify`` 反查补 id；
- **解析不出的降级**：id 与调用名都不在声明里 → ``model_id`` 为 null、调用名照旧
  下发（旧 id 与快照是恢复线索，不能被展示层当"没有模型"）；记录整体缺失 →
  模板默认（与 resume 同一条出口）。

记录形态按真实产物构造：跑一个真会话 → 逐出（磁盘成为唯一事实来源）→ 直接改
``metadata.json``。
"""

from __future__ import annotations

import asyncio
import json
import time
from pathlib import Path

import pytest

from wing_probe import Probe, Session, Turn

#: 逐出观察预算（release 是显式动作，留足轮询余量）。
POLL_DEADLINE = 20.0
POLL_INTERVAL = 0.2

#: 主场景：id ≠ 调用名，且声明了展示名（四个字段各自可分辨）。
MODEL_ID = "list-id"
MODEL_NAME = "probe/list-model"
MODEL_SPEC = {"id": MODEL_ID, "name": MODEL_NAME, "display_name": "List Model"}

#: 解析不出的降级场景：记录里的 id / 调用名都不在声明里。
GHOST_ID = "ghost-id"
GHOST_NAME = "probe/ghost-model"

#: probe 生成的默认模板模型（`@pytest.mark.probe_env(models=…)` 未覆盖它，会被
#: 追加成字符串声明）：记录整体缺失时"模板默认"那一级的答案。
TEMPLATE_MODEL = "probe/default"


def _session_dir(probe: Probe, session_id: str) -> Path:
    return probe.env.session_dir(session_id)


def _rewrite_metadata(probe: Probe, session_id: str, **changes: object) -> dict:
    """改盘上的 metadata.json（``None`` = 删掉该键），返回改后的记录。"""
    path = _session_dir(probe, session_id) / "metadata.json"
    record = json.loads(path.read_text(encoding="utf-8"))
    for key, value in changes.items():
        if value is None:
            record.pop(key, None)
        else:
            record[key] = value
    path.write_text(json.dumps(record, ensure_ascii=False, indent=2), encoding="utf-8")
    return record


async def _entries(probe: Probe) -> list[dict]:
    payload = await probe.driver_required.http.request("GET", "/api/session/list")
    rows = payload.get("sessions")
    assert isinstance(rows, list), payload
    return rows


async def _entry(probe: Probe, session_id: str) -> dict:
    for row in await _entries(probe):
        if row.get("id") == session_id:
            return row
    raise AssertionError(f"session {session_id} not in list: {await _entries(probe)}")


def _quartet(entry: dict) -> tuple:
    """条目的模型四件套（身份 + 运行期事实 + 展示素材）。"""
    return (
        entry.get("model_id"),
        entry.get("model_name"),
        entry.get("provider_name"),
        entry.get("model_display_name"),
    )


def _info_quartet(info: dict) -> tuple:
    """``/api/session/info`` 的同义四件套（字段名不同：``model`` 是调用名）。"""
    return (
        info.get("model_id"),
        info.get("model"),
        info.get("provider_name"),
        info.get("model_display_name"),
    )


async def _evict(probe: Probe, session: Session) -> None:
    """退订 → release → 等 ``inactive``（磁盘成为唯一事实来源）。"""
    driver = probe.driver_required
    await driver.http.unsubscribe(session.session_id, driver.client_id)
    payload = await driver.http.request(
        "POST", "/api/session/release", body={"session_id": session.session_id}
    )
    assert payload == {"ok": True, "released": True, "detail": "released"}, payload
    deadline = time.monotonic() + POLL_DEADLINE
    observed: str | None = None
    while time.monotonic() < deadline:
        observed = (await _entry(probe, session.session_id)).get("status")
        if observed == "inactive":
            return
        await asyncio.sleep(POLL_INTERVAL)
    raise AssertionError(f"session {session.session_id} stayed in status {observed!r}")


async def _resubscribe(probe: Probe, session_id: str) -> None:
    driver = probe.driver_required
    await driver.http.subscribe(session_id, driver.client_id)


@pytest.mark.probe_env(models=[MODEL_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_list_model_survives_eviction_and_matches_info(probe: Probe) -> None:
    """active 与 inactive 两种形态给同一个答案，且等于 info / resume 的口径。"""
    probe.register(MODEL_NAME, Turn.of(text="reply one"), Turn.of(text="reply two"))
    http = probe.driver_required.http

    session = await probe.session(model=MODEL_ID)
    await session.chat("alpha")

    # ① active：四件套取自 live agent（与 info 逐字段一致）。
    active = await _entry(probe, session.session_id)
    assert active.get("status") != "inactive", active
    assert _quartet(active) == (MODEL_ID, MODEL_NAME, "probe", "List Model"), active
    assert _quartet(active) == _info_quartet(
        await http.get_session_info(session.session_id)
    ), active

    # ② 盘上记录就是三元组本身（下一步的逐出只依赖它，不依赖内存态）。
    recorded = probe.history(session).metadata() or {}
    assert (
        recorded.get("model_id"),
        recorded.get("model_name"),
        recorded.get("provider_name"),
    ) == (MODEL_ID, MODEL_NAME, "probe"), recorded

    # ③ inactive：逐出后同一个答案（按 resume 链解析盘上记录）。
    await _evict(probe, session)
    inactive = await _entry(probe, session.session_id)
    assert inactive.get("status") == "inactive", inactive
    assert _quartet(inactive) == _quartet(active), (active, inactive)

    # ④ resume 之后照旧：列表说的模型就是真跑起来的模型（不存在"看一眼是 A、
    #    接着干变成 B"）。
    resumed = await probe.resume(session.session_id)
    await _resubscribe(probe, session.session_id)
    assert _quartet(await _entry(probe, session.session_id)) == _quartet(active)
    assert _info_quartet(await http.get_session_info(session.session_id)) == _quartet(
        active
    )
    result = await resumed.chat("beta")
    assert result.data["subtype"] == "success", result.data
    assert probe.requests.count(MODEL_NAME) == 2, probe.requests.summary()


@pytest.mark.probe_env(models=[MODEL_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_legacy_record_backfills_the_id_in_the_list(probe: Probe) -> None:
    """旧记录（无 ``model_id``）：列表与 resume 一样经反查补出引用词。"""
    probe.register(MODEL_NAME, Turn.of(text="reply"))
    http = probe.driver_required.http

    session = await probe.session(model=MODEL_ID)
    await session.chat("alpha")
    await _evict(probe, session)

    # 构造"引入 id 之前"的记录：删掉 model_id，快照留原样。
    stripped = _rewrite_metadata(probe, session.session_id, model_id=None)
    assert "model_id" not in stripped, stripped

    entry = await _entry(probe, session.session_id)
    assert _quartet(entry) == (MODEL_ID, MODEL_NAME, "probe", "List Model"), entry

    # 与 resume 的口径对账（resume 也走同一条反查补 id）。
    await probe.resume(session.session_id)
    assert _info_quartet(await http.get_session_info(session.session_id)) == _quartet(
        entry
    )


@pytest.mark.probe_env(models=[MODEL_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_missing_record_falls_back_to_the_template(probe: Probe) -> None:
    """记录整体缺失：列表落模板默认——并与 resume 真跑的模型一致。"""
    probe.register(MODEL_NAME, Turn.of(text="reply"))
    http = probe.driver_required.http

    session = await probe.session(model=MODEL_ID)
    await session.chat("alpha")
    await _evict(probe, session)

    stripped = _rewrite_metadata(
        probe, session.session_id, model_id=None, model_name=None, provider_name=None
    )
    assert "model_name" not in stripped, stripped

    entry = await _entry(probe, session.session_id)
    assert _quartet(entry) == (TEMPLATE_MODEL, TEMPLATE_MODEL, "probe", None), entry

    await probe.resume(session.session_id)
    assert _info_quartet(await http.get_session_info(session.session_id)) == _quartet(
        entry
    )


@pytest.mark.probe_env(models=[MODEL_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_unresolvable_record_degrades_the_id_slot_only(probe: Probe) -> None:
    """声明里查不到（id 与调用名都失效）：id 位降级为 null，调用名照旧下发。

    旧 id 与快照是恢复线索（resume 同样保留记录继续跑），列表据此仍能说清
    "这个会话跑的是什么"——把整条记录当成"没有模型"是信息丢失，不是降级。
    """
    probe.register(MODEL_NAME, Turn.of(text="reply"))
    http = probe.driver_required.http

    session = await probe.session(model=MODEL_ID)
    await session.chat("alpha")
    await _evict(probe, session)

    ghost = _rewrite_metadata(
        probe,
        session.session_id,
        model_id=GHOST_ID,
        model_name=GHOST_NAME,
        provider_name="probe",
    )
    assert ghost["model_name"] == GHOST_NAME, ghost

    entry = await _entry(probe, session.session_id)
    assert _quartet(entry) == (None, GHOST_NAME, "probe", None), entry

    await probe.resume(session.session_id)
    assert _info_quartet(await http.get_session_info(session.session_id)) == _quartet(
        entry
    )
