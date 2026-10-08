"""注册表面与"存量配置容忍"场景（追加项 · 03 review [S]1）。

两条相关但**不同**的路径（design.md Assumption A1）：

- **配置路径**（`agents[].tools`，`AgentTemplate.from_agent_config`）：不可解析的名字
  **静默丢弃**——存量配置里留着已删除的工具名（历史包袱）不该让会话起不来，这是
  "删除工具"这一行为变化的**向后兼容契约**；声明集因此恒等于"配置名 ∩ 注册表名"，
  而注册表名由 `GET /api/tools` 实时给出（探针不写死任何名字清单）；
- **显式覆盖路径**（`AgentOverride.tools` → `WingAgent.set_tools`）：**严格**——不可解析
  的名字是 400（"你要的东西不存在"是调用方的错误，不能静默吞掉）。

断言面：`GET /api/tools`（注册表视图）、请求体声明集、HTTP 状态码、workspace 文件。

> **删除钉子（03 review [S]1 的"缺席"半边）已落地**：本场景在集成分支上运行
> （`03_delete_legacy_tools` 已并入），断言 `Explorer` / `BetterEdit` **不在**
> 注册表里——两个名字在删除前后分别是"可解析"与"不可解析"，本文件两个方向都钉。
"""

from __future__ import annotations

import json

import pytest

from wing_probe import DriverHttpError, Probe, ToolCall, Turn
from wing_probe.env import DEFAULT_AGENT_TOOLS

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
TOLERANT_MODEL = "probe/registry-legacy-names"
STRICT_MODEL = "probe/registry-unknown-override"

#: 配置里的"存量名字"：两个真实工具名（`Explorer` / `BetterEdit`——删除落地前后
#: 分别是"可解析"与"不可解析"，两种世界都要走通）+ 一个从来不存在的名字。
LEGACY_NAMES = ("Explorer", "BetterEdit", "GhostTool")
CONFIG_TOOLS = (*DEFAULT_AGENT_TOOLS, *LEGACY_NAMES)

#: 工具真的跑过（"其余工具照常生效"）的 workspace 证据。
WRITTEN_FILE = "registry-surface.txt"


async def _registry_names(probe: Probe) -> set[str]:
    """`GET /api/tools` 的 LLM 可见名集合（注册表的实时真相）。"""
    payload = await probe.driver_required.http.list_tools()
    return {str(tool["llm_name"]) for tool in payload["tools"]}


@pytest.mark.probe_env(models=[TOLERANT_MODEL], tools=list(CONFIG_TOOLS))
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_config_legacy_tool_names_are_tolerated(probe: Probe) -> None:
    """配置路径：不可解析的工具名静默忽略，其余工具照常生效（红线）。

    WHEN `agents[0].tools` 里混进 `Explorer` / `BetterEdit` / `GhostTool`
    THEN 建会话不 4xx/5xx、跑一轮正常收尾（`write` 真的写了文件）；请求体声明集
    **恰好等于**"配置名 ∩ 注册表名"（由 `GET /api/tools` 实时决定，两个方向都断言：
    可解析的一个不少、不可解析的一个不多）。
    """
    registry = await _registry_names(probe)
    assert {"Bash", "Read", "Write"} <= registry, registry
    # 删除钉子（03 review [S]1 的"缺席"半边）：两个已删工具名真的不在注册表里——
    # 与上面"配置里留着它们也不炸"的容忍契约是**两个方向**的断言。
    assert not ({"Explorer", "BetterEdit"} & registry), registry
    expected = sorted(name for name in CONFIG_TOOLS if name in registry)

    probe.register(
        TOLERANT_MODEL,
        Turn.of(
            tool_calls=[
                ToolCall("Write", {"path": WRITTEN_FILE, "content": "tolerated"})
            ]
        ),
        Turn.of(text="done"),
    )
    session = await probe.session(model=TOLERANT_MODEL)

    result = await session.chat("write the file")
    assert result.data["subtype"] == "success", result.data
    session.watch.assert_never("error")

    declared = probe.context(TOLERANT_MODEL, 0).tool_names
    assert declared == expected, (
        f"声明集必须与注册表实时对账（配置名 ∩ /api/tools）\n"
        f"config: {list(CONFIG_TOOLS)}\nregistry: {sorted(registry)}"
    )
    assert "GhostTool" not in declared, declared
    probe.files.assert_content(WRITTEN_FILE, equals="tolerated")

    # 会话自身可用（不可解析名没有把它留在"半初始化"状态）。
    info = await session.info()
    assert info["status"] == "idle", info
    probe.history(session).assert_tool_pairing()


@pytest.mark.probe_env(models=[STRICT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_explicit_override_with_unknown_name_is_rejected(probe: Probe) -> None:
    """对照：显式覆盖（`AgentOverride.tools`）是严格路径——400 且不留半成品会话。

    与配置路径的宽容相反：调用方点名要一个不存在的工具是**调用方的错误**，
    必须显式失败（静默吞掉会让前端/编排器拿到一个"少了一个工具"的会话却不自知）。
    """
    with pytest.raises(DriverHttpError) as failure:
        await probe.session(model=STRICT_MODEL, tools=["GhostTool"])

    assert failure.value.status == 400, failure.value.call.render()
    detail = json.dumps(failure.value.call.response, ensure_ascii=False)
    assert "cannot resolve tool reference" in detail, detail

    sessions = await probe.driver_required.http.list_sessions()
    assert sessions == {"sessions": []}, sessions
