"""自定义 session id（create-or-adopt）与 resume 覆盖子集（步骤 01）。

被守的语义：

- ``POST /api/session/create`` 的 ``session_id`` 是 **create-or-adopt**：
  不存在 → 以该 id 建会话（**id 精确一致**，编排方自己生成的 UUID 就此生效）；
  已存在 → **收养**既有会话（不产生第二个会话，链路连续，覆盖按 resume 子集）；
- ``POST /api/session/resume`` 的 ``agent`` 只应用 **model / provider / effort /
  tools** 子集；``system_prompt`` / ``append_system_prompt`` / ``max_turns``
  一律不应用（不改链上前缀 / 不改会话既有限额）——给了也不该出现在请求体里；
- 闸门只防穿越与卫生：跨越型 id（``../escape``、含 ``/``）、点开头（``.media``
  是媒体池）、超长一律 400，且**磁盘零痕迹**；任意安全字符串（UUID、``team.run-7``）
  照常可用。

"id 生效"与"覆盖生效"都是**可观测事实**：前者看响应与 ``history.jsonl`` 的目录名，
后者看假 Provider 留档的请求体（模型名 / tools 声明 / system 段）与落盘 metadata。
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from wing_probe import Probe, Turn
from wing_probe.driver import DriverHttpError
from wing_probe.driver.session import Driver

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
CREATED_MODEL = "probe/custom-id-created"
ADOPTED_MODEL = "probe/custom-id-adopted"
RESUMED_MODEL = "probe/custom-id-resumed"

#: 编排方自带的 id（Claude Agent SDK 形态；旧闸门会拒绝的形态）。
CUSTOM_ID = "3f2b9d1e-6c1a-4f2b-9d3e-1a2b3c4d5e6f"

#: 只用于"必须在请求体里搜不到"的唯一标记（唯一化，避免被别的字段撞上）。
SYSTEM_PROMPT_MARKER = "PROBE-SYSTEM-PROMPT-MUST-NOT-APPLY-9c41"


def _driver(probe: Probe) -> Driver:
    return probe.driver_required


def _session_dirs(probe: Probe) -> list[str]:
    """sessions root 下的会话目录名（跳过存储自己的点命名空间，如 ``.media``）。"""
    root = probe.env.sessions_path
    if not root.exists():
        return []
    return sorted(
        entry.name
        for entry in root.iterdir()
        if entry.is_dir() and not entry.name.startswith(".")
    )


async def _create(
    probe: Probe,
    *,
    session_id: str | None = None,
    model: str | None = None,
    workspace: str | None = None,
    agent: dict | None = None,
    tags: list[str] | None = None,
) -> dict:
    """``POST /api/session/create``（留档调用；非 2xx 抛 ``DriverHttpError``）。"""
    body: dict = {}
    if session_id is not None:
        body["session_id"] = session_id
    if workspace is not None:
        body["workspace"] = workspace
    if tags is not None:
        body["tags"] = tags
    overrides: dict = dict(agent or {})
    if model is not None:
        overrides["model"] = model
    if overrides:
        body["agent"] = overrides
    return await _driver(probe).http.request("POST", "/api/session/create", body=body)


async def _resume(probe: Probe, session_id: str, *, agent: dict | None = None) -> dict:
    """``POST /api/session/resume``（可带覆盖）。"""
    body: dict = {"session_id": session_id}
    if agent is not None:
        body["agent"] = agent
    return await _driver(probe).http.request("POST", "/api/session/resume", body=body)


async def _listed_ids(probe: Probe) -> list[str]:
    payload = await _driver(probe).http.request("GET", "/api/session/list")
    return [entry["id"] for entry in payload.get("sessions", [])]


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_custom_id_create_then_adopt_is_one_session(probe: Probe) -> None:
    """给定 id 建会话 → id 精确一致；同 id 二次 create → 收养同一个会话。"""
    probe.register(
        CREATED_MODEL,
        Turn.of(text="first reply"),
        Turn.of(text="second reply"),
    )

    created = await _create(probe, session_id=CUSTOM_ID, model=CREATED_MODEL)
    assert created["session_id"] == CUSTOM_ID, created
    assert _session_dirs(probe) == [CUSTOM_ID], _session_dirs(probe)

    session = await _driver(probe).attach(CUSTOM_ID, workspace=probe.workspace)
    result = await session.chat("first")
    assert result.data["subtype"] == "success", result.data
    records_before = len(session.history.records)

    # 二次 create 同一个 id：收养（不是新会话）。
    adopted = await _create(probe, session_id=CUSTOM_ID, model=CREATED_MODEL)
    assert adopted["session_id"] == CUSTOM_ID, adopted
    assert _session_dirs(probe) == [CUSTOM_ID], _session_dirs(probe)
    assert await _listed_ids(probe) == [CUSTOM_ID]
    assert len(session.history.records) == records_before, "收养不得改动历史"

    # 链路连续：下一轮请求看得见上一轮的消息。
    result = await session.chat("second")
    assert result.data["subtype"] == "success", result.data
    follow_up = probe.context(CREATED_MODEL, 1)
    assert follow_up.texts("user") == ["first", "second"], follow_up.describe()
    assert follow_up.role_sequence() == ["user", "assistant", "user"], (
        follow_up.role_sequence()
    )


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_resume_override_switches_model_and_keeps_prefix(probe: Probe) -> None:
    """resume 的 agent 覆盖：model/effort 生效，system_prompt 不生效，前缀不变。"""
    probe.register(
        CREATED_MODEL, Turn.of(text="alpha reply"), Turn.of(text="beta reply")
    )
    probe.register(RESUMED_MODEL, Turn.of(text="switched reply"))

    created = await _create(
        probe, session_id=CUSTOM_ID, model=CREATED_MODEL, workspace=str(probe.workspace)
    )
    session = await _driver(probe).attach(
        created["session_id"], workspace=probe.workspace
    )
    result = await session.chat("alpha")
    assert result.data["subtype"] == "success", result.data
    before = probe.context(CREATED_MODEL, 0)

    resumed = await _resume(
        probe,
        session.session_id,
        agent={
            "model": RESUMED_MODEL,
            "effort": "high",
            # 以下三个字段是创建期语义：必须被忽略（不是"尽量应用"）。
            "system_prompt": SYSTEM_PROMPT_MARKER,
            "append_system_prompt": SYSTEM_PROMPT_MARKER,
            "max_turns": 1,
        },
    )
    assert resumed["session_id"] == session.session_id, resumed

    result = await session.chat("beta")
    assert result.data["subtype"] == "success", result.data

    after = probe.context(RESUMED_MODEL, 0)
    assert after.model == RESUMED_MODEL
    # 上下文连续（既有链原样带过去），system 段逐项未变。
    assert after.role_sequence() == ["user", "assistant", "user"], after.role_sequence()
    assert after.system == before.system, "resume 覆盖不得改写系统提示词"
    assert SYSTEM_PROMPT_MARKER not in json.dumps(after.body, ensure_ascii=False), (
        "被忽略的字段不得出现在请求体里",
        after.body,
    )
    # effort 属于覆盖子集：本次请求体带上了它。
    assert after.body.get("reasoning_effort") == "high", after.body

    # 覆盖随 metadata 落盘（重启 / 逐出后水合仍生效），被忽略的字段不落记录。
    metadata = probe.history(session).metadata() or {}
    assert metadata.get("model_name") == RESUMED_MODEL, metadata
    assert metadata.get("reasoning_effort") == "high", metadata
    assert "system_prompt" not in metadata, metadata
    assert "max_turns" not in metadata, metadata


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_adopt_with_custom_id_applies_resume_subset(probe: Probe) -> None:
    """收养既有 id 时的覆盖 = resume 子集：model/tools 生效并落盘。"""
    probe.register(
        CREATED_MODEL, Turn.of(text="alpha reply"), Turn.of(text="beta reply")
    )
    probe.register(ADOPTED_MODEL, Turn.of(text="adopted reply"))

    created = await _create(
        probe, session_id=CUSTOM_ID, model=CREATED_MODEL, workspace=str(probe.workspace)
    )
    session = await _driver(probe).attach(
        created["session_id"], workspace=probe.workspace
    )
    result = await session.chat("alpha")
    assert result.data["subtype"] == "success", result.data
    declared_before = probe.context(CREATED_MODEL, 0).tool_names

    adopted = await _create(
        probe,
        session_id=CUSTOM_ID,
        agent={"model": ADOPTED_MODEL, "tools": ["Read", "Glob"]},
    )
    assert adopted["session_id"] == CUSTOM_ID, adopted
    assert _session_dirs(probe) == [CUSTOM_ID], _session_dirs(probe)

    result = await session.chat("beta")
    assert result.data["subtype"] == "success", result.data

    after = probe.context(ADOPTED_MODEL, 0)
    assert after.model == ADOPTED_MODEL
    # 上下文连续：收养不是"新会话"。
    assert after.texts("user")[0] == "alpha", after.describe()
    # tools 覆盖走**运行时热切换**同一路径：链非空 → 声明集冻结（KV cache 保护，
    # 请求里的 tools 仍是老集合），改动以 System Reminder 告知模型。
    assert after.tool_names == declared_before, after.tool_names
    reminder = "\n".join(after.texts("user"))
    assert "[System Reminder]" in reminder, reminder

    metadata = probe.history(session).metadata() or {}
    assert metadata.get("model_name") == ADOPTED_MODEL, metadata
    assert metadata.get("tools") == ["Read", "Glob"], metadata


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_invalid_session_id_is_rejected_without_traces(probe: Probe) -> None:
    """越界 id → 400 且磁盘零痕迹；任意安全 id 照常可用（闸门只防穿越与卫生）。"""
    driver = _driver(probe)
    for bad in (
        "../escape",
        "/tmp/absolute",
        "a/b",
        ".media",
        "a..b",
        "x" * 129,  # 超字节上限（ASCII）
        "收" * 128,  # 384 字节：按字符限长会误放行，Linux 上 mkdir ENAMETOOLONG
        # 孤立代理字符（合法 JSON / 非法 UTF-8）走不了这里：httpx 的 json= 在
        # **客户端**就编码不了它（UnicodeEncodeError）。那条路径由原始字节体的
        # 路由测试覆盖：libs/core/tests/test_gateway_http.py 的
        # TestRealRuntimeIdGate（必须 400，不是 500）。
    ):
        with pytest.raises(DriverHttpError) as failure:
            await _create(probe, session_id=bad)
        assert failure.value.status == 400, failure.value.call.render()
        assert "invalid session id" in json.dumps(failure.value.call.response), (
            failure.value.call.render()
        )

    # 零痕迹：没有会话目录、诱饵目录也没被创建。
    assert _session_dirs(probe) == [], _session_dirs(probe)
    assert not (probe.env.sessions_path.parent / "escape").exists()
    assert await _listed_ids(probe) == []

    # 放宽后的闸门照常接受"非时间戳形态"的任意安全 id。
    probe.register(CREATED_MODEL, Turn.of(text="ok"))
    created = await _create(probe, session_id="team.run-7", model=CREATED_MODEL)
    assert created["session_id"] == "team.run-7", created
    assert _session_dirs(probe) == ["team.run-7"], _session_dirs(probe)

    session = await driver.attach("team.run-7", workspace=probe.workspace)
    result = await session.chat("hello")
    assert result.data["subtype"] == "success", result.data
    assert (probe.env.session_dir("team.run-7") / "history.jsonl").exists()


def _fs_is_case_insensitive(base: Path) -> bool:
    """本机文件系统对大小写是否不敏感（决定两个变体是否解析到同一个目录）。"""
    probe_dir = base / "fs-case-probe"
    probe_dir.mkdir(parents=True, exist_ok=True)
    try:
        return (base / "FS-CASE-PROBE").is_dir()
    finally:
        probe_dir.rmdir()


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_case_variant_ids_never_share_one_directory(probe: Probe) -> None:
    """大小写 / 归一化变体：要么归一到同一会话，要么是两个独立目录——绝不混写。

    不变量（与文件系统类型无关）：**回报的 id 逐字对应一个磁盘目录**，且不存在
    "两个 id 一份 history.jsonl"。不敏感 FS（macOS APFS / Windows NTFS）上第二个
    请求被归一到第一个会话的真名（网关按磁盘真名回应）；敏感 FS（Linux）上两者
    是各自独立的会话。
    """
    probe.register(CREATED_MODEL, Turn.of(text="variant reply"))
    aliases = _fs_is_case_insensitive(probe.env.root)

    created = await _create(
        probe, session_id="Team-V", model=CREATED_MODEL, tags=["favorite"]
    )
    assert created["session_id"] == "Team-V", created

    variant = await _create(probe, session_id="team-v", model=CREATED_MODEL)
    if aliases:
        assert variant["session_id"] == "Team-V", variant  # 归一：真名
        assert _session_dirs(probe) == ["Team-V"], _session_dirs(probe)
    else:
        assert variant["session_id"] == "team-v", variant
        assert _session_dirs(probe) == ["Team-V", "team-v"], _session_dirs(probe)

    # 不变量：每个回报的 id 都能逐字寻址，且目录数 == 会话数（无共享目录）。
    expected = {created["session_id"], variant["session_id"]}
    assert _session_dirs(probe) == sorted(expected), _session_dirs(probe)
    for session_id in expected:
        assert (probe.env.sessions_path / session_id).is_dir(), session_id
    assert len(await _listed_ids(probe)) == len(expected)
