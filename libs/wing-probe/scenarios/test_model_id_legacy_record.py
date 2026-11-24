"""旧 metadata 的恢复链 —— id 缺失经反查补全，id 命中则跟随当前映射。

``SessionMetadata`` 的模型身份从「(provider, name) 二元组」升级为
``(model_id, provider_name, model_name)`` 三元组。两类历史记录都要能恢复
（C5 恢复链，优先级固定、无候选集合、不猜测）：

- **旧记录**（有 ``model_name`` + ``provider_name``、无 ``model_id``）：resume 走
  快照路径，经 ``Config.identify(provider, name)`` 反查补 id，并把内存态与盘上
  记录对齐（一次性迁移，不是写噪声）；``/api/session/info`` 与
  ``sync_session`` 的 agent 快照从恢复那一刻起就带 id；
- **id 命中**：用**当前映射**（``name`` / ``provider`` 跟随配置演化）——声明里把
  ``id`` 的调用名改掉之后 resume，上游收到的必须是新调用名，而 ``model_id`` 不变。

记录形态按真实产物构造：先跑一个真会话 → 逐出（磁盘成为唯一事实来源）→ 直接改
``metadata.json``（裁掉 ``model_id``）/ 改 ``config.yaml`` + reload → resume。
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
import yaml
from _session_helpers import evict_and_wait_inactive, require_session_row

from wing_probe import Probe, Turn

#: 旧记录场景：id ≠ name（反查补 id 才不是平凡映射）。
LEGACY_ID = "legacy-id"
LEGACY_NAME = "probe/legacy-upstream"
LEGACY_SPEC = {
    "id": LEGACY_ID,
    "name": LEGACY_NAME,
    "display_name": "Legacy Upstream",
}

#: 映射演化场景：同一个 id 的调用名在配置里改名。
MAPPED_ID = "map-id"
OLD_NAME = "probe/map-v1"
NEW_NAME = "probe/map-v2"
MAPPED_SPEC = {"id": MAPPED_ID, "name": OLD_NAME}


def _metadata_path(probe: Probe, session_id: str) -> Path:
    return probe.env.session_dir(session_id) / "metadata.json"


def _strip_model_id(probe: Probe, session_id: str) -> dict:
    """把 metadata.json 改回"引入 id 之前"的形态（删 ``model_id``），返回改后内容。"""
    path = _metadata_path(probe, session_id)
    record = json.loads(path.read_text(encoding="utf-8"))
    assert "model_id" in record, record
    del record["model_id"]
    path.write_text(json.dumps(record, ensure_ascii=False, indent=2), encoding="utf-8")
    return record


@pytest.mark.probe_env(models=[LEGACY_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_legacy_record_backfills_model_id_on_resume(probe: Probe) -> None:
    """旧记录冷读可反查 id，显式恢复才将它写回并沿用原调用名。"""
    probe.register(LEGACY_NAME, Turn.of(text="reply one"), Turn.of(text="reply two"))
    driver = probe.driver_required
    http = driver.http

    session = await probe.session(model=LEGACY_ID)
    await session.chat("alpha")

    # ① 新会话写的是完整三元组（id ≠ name，三者分列）。
    created = probe.history(session).metadata() or {}
    assert created.get("model_id") == LEGACY_ID, created
    assert created.get("model_name") == LEGACY_NAME, created
    assert created.get("provider_name") == "probe", created

    await evict_and_wait_inactive(probe, session.session_id)

    # ② 构造旧记录：删掉 model_id，快照留原样；盘上确实没有 id 了（前提成立）。
    stripped = _strip_model_id(probe, session.session_id)
    assert "model_id" not in stripped, stripped
    assert stripped["model_name"] == LEGACY_NAME, stripped
    assert stripped["provider_name"] == "probe", stripped

    metadata = _metadata_path(probe, session.session_id)
    before_list = metadata.read_bytes()
    cold = require_session_row(await http.list_sessions(), session.session_id)
    assert cold["status"] == "inactive", cold
    fields = ("model_id", "model_name", "provider_name", "model_display_name")
    expected = (LEGACY_ID, LEGACY_NAME, "probe", "Legacy Upstream")
    assert tuple(cold[key] for key in fields) == expected, cold
    assert metadata.read_bytes() == before_list, metadata

    # ③ resume：恢复链走快照 + identify 反查补 id。
    resumed = await probe.resume(session.session_id)
    # 逐出后的 subscribe 也会水合，先检查 resume 自己完成了落盘。
    aligned = probe.history(resumed).metadata() or {}
    assert aligned.get("model_id") == LEGACY_ID, aligned
    assert aligned.get("model_name") == LEGACY_NAME, aligned
    assert aligned.get("provider_name") == "probe", aligned

    await http.subscribe(session.session_id, driver.client_id)

    info = await http.get_session_info(session.session_id)
    info_fields = ("model_id", "model", "provider_name", "model_display_name")
    assert tuple(info[key] for key in info_fields) == expected, info
    active = require_session_row(await http.list_sessions(), session.session_id)
    assert tuple(active[key] for key in fields) == expected, active

    sync = await resumed.watch.expect("sync_session", within=5.0)
    agent = sync.data["agent"]
    assert agent["model_id"] == LEGACY_ID, agent
    assert agent["model_name"] == LEGACY_NAME, agent
    assert agent["provider_name"] == "probe", agent

    # ④ 恢复后照常跑：请求打到原调用名（id 不外发）。
    result = await resumed.chat("beta")
    assert result.data["subtype"] == "success", result.data
    assert probe.requests.count(LEGACY_NAME) == 2, probe.requests.summary()
    assert probe.requests.count(LEGACY_ID) == 0, probe.requests.summary()


@pytest.mark.probe_env(models=[MAPPED_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_recorded_id_follows_the_current_mapping_on_resume(probe: Probe) -> None:
    """记录里的 id 命中 → 用**当前映射**：改名后 resume 打到新调用名，id 不变。"""
    probe.register(OLD_NAME, Turn.of(text="v1 reply"), Turn.of(text="v2 reply"))
    probe.register(NEW_NAME, Turn.of(text="renamed reply"))
    driver = probe.driver_required
    http = driver.http

    session = await probe.session(model=MAPPED_ID)
    await session.chat("alpha")
    assert probe.requests.count(OLD_NAME) == 1, probe.requests.summary()

    await evict_and_wait_inactive(probe, session.session_id)
    recorded = probe.history(session.session_id).metadata() or {}
    assert recorded.get("model_id") == MAPPED_ID, recorded
    assert recorded.get("model_name") == OLD_NAME, recorded

    # 配置演化：同一个 id 换个调用名（id 是引用词，name 只是发给上游的值）。
    config = yaml.safe_load(probe.env.config_path.read_text(encoding="utf-8"))
    models = config["providers"][0]["models"]
    target = next(
        item for item in models if isinstance(item, dict) and item["id"] == MAPPED_ID
    )
    target["name"] = NEW_NAME
    probe.env.config_path.write_text(
        yaml.safe_dump(config, sort_keys=False, allow_unicode=True), encoding="utf-8"
    )
    reload_result = await http.reload()
    assert reload_result.get("ok") is True, reload_result

    # resume：id 命中 → name / provider 跟随新映射（不是回落旧快照）。
    resumed = await probe.resume(session.session_id)
    await http.subscribe(session.session_id, driver.client_id)

    info = await http.get_session_info(session.session_id)
    assert info["model_id"] == MAPPED_ID, info
    assert info["model"] == NEW_NAME, info
    assert info["provider_name"] == "probe", info

    sync = await resumed.watch.expect("sync_session", within=5.0)
    assert sync.data["agent"]["model_id"] == MAPPED_ID, sync.data["agent"]
    assert sync.data["agent"]["model_name"] == NEW_NAME, sync.data["agent"]

    # 盘上记录对齐到新快照（id 不变、调用名跟着配置走）。
    aligned = probe.history(resumed).metadata() or {}
    assert aligned.get("model_id") == MAPPED_ID, aligned
    assert aligned.get("model_name") == NEW_NAME, aligned

    # 下一轮请求打到新调用名（旧名字的剧本不再被消费）。
    result = await resumed.chat("beta")
    assert result.data["subtype"] == "success", result.data
    assert probe.requests.count(NEW_NAME) == 1, probe.requests.summary()
    assert probe.requests.count(OLD_NAME) == 1, probe.requests.summary()
