"""跨 provider 的同名调用名 —— ``id`` 消歧，provider 不再是引用词。

两个 provider 声明**同一个调用名**（``shared``）但给了不同的 ``id``；id 全局唯一
所以配置合法，而"发给上游的名字"是同名——这正是旧世界靠 ``(provider, model)``
配对、外部只能发一个字段时无法表达的场景。钉的不变量：

- 配置加载期：id 跨 provider 全局唯一，同名调用名（不同 id）合法共存；
- 运行期：``update {model_id}`` 单键查表，切换后请求打到**该 id 所属 provider 的
  端点**（请求留档的 ``path``），上游收到的仍是调用名 ``shared``（id 不外发）；
- 展示分组跟随 id 映射：``session_state_changed`` 的 ``provider_name`` 与
  ``model_id`` 同刻换组；
- 负面：**同一个 id** 声明在两个 provider → 网关**降级启动**（setup mode，D1），
  设置端点精确报出问题（两个 provider 名 + 修复示例，唯一的修复动作是加显式 id）
  ——坏配置不再让进程退出（04 的行为变更）。
"""

from __future__ import annotations

from pathlib import Path

import pytest

from wing_probe import DriverHttp, DriverHttpError, Probe, Turn
from wing_probe.env import ProbeEnv

#: 两个 provider 共用的调用名（发给上游的值，剧本按它路由）。
SHARED_NAME = "shared"

#: provider 静态声明：同名调用名、不同 id（id 是引用词）。
P1_SPEC = {"id": "p1-shared", "name": SHARED_NAME, "display_name": "P1 Shared"}
P2_SPEC = {"id": "p2-shared", "name": SHARED_NAME, "display_name": "P2 Shared"}

#: 主 provider 的端点路径（附加 provider 是 ``/probe2/v1/chat/completions``）。
P1_PATH = "/v1/chat/completions"
P2_PATH = "/probe2/v1/chat/completions"


@pytest.mark.probe_env(
    models=[P1_SPEC],
    extra_providers=[{"name": "probe2", "models": [P2_SPEC]}],
)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_model_id_picks_the_provider_endpoint(probe: Probe) -> None:
    """同名调用名跨 provider：按 id 切换 → 请求落到对应端点（上游只见调用名）。"""
    probe.register(
        SHARED_NAME, Turn.of(text="r1"), Turn.of(text="r2"), Turn.of(text="r3")
    )
    http = probe.driver_required.http

    session = await probe.session(model="p1-shared")
    await session.chat("one")

    # ① p1 的 id → p1 的端点；上游收到调用名（不是 id），展示分组是 probe。
    first = probe.request(SHARED_NAME, 0)
    assert first.model == SHARED_NAME, first.describe()
    assert first.body["model"] == SHARED_NAME, first.body
    assert first.path == P1_PATH, first.path

    info = await http.get_session_info(session.session_id)
    assert info["model_id"] == "p1-shared", info
    assert info["model"] == SHARED_NAME, info
    assert info["provider_name"] == "probe", info
    assert info["model_display_name"] == "P1 Shared", info

    # ② 切到另一个 provider 的同名模型：id 单键查表，状态三件套一起换组。
    await http.update_session(session.session_id, model_id="p2-shared")
    changed = await session.watch.expect("session_state_changed", within=5.0)
    assert changed.data["model"] == SHARED_NAME, changed.data
    assert changed.data["model_id"] == "p2-shared", changed.data
    assert changed.data["provider_name"] == "probe2", changed.data
    assert changed.data["model_display_name"] == "P2 Shared", changed.data

    # ③ 同名模型的下一次调用打到 **p2 的端点**——这是"不再靠 provider 猜测"的
    #    唯一的运行期证据（id 决定映射，映射决定 provider）。
    await session.chat("two")
    second = probe.request(SHARED_NAME, 1)
    assert second.model == SHARED_NAME, second.describe()
    assert second.path == P2_PATH, second.path

    info = await http.get_session_info(session.session_id)
    assert info["model_id"] == "p2-shared", info
    assert info["provider_name"] == "probe2", info

    # ④ 新会话直接用 p2 的 id 建：从第一帧起就落在 p2 的端点（订阅重放的 agent
    #    快照同样是 p2 的分组）。
    fresh = await probe.session(model="p2-shared")
    sync = await fresh.watch.expect("sync_session", within=5.0)
    assert sync.data["agent"]["model_id"] == "p2-shared", sync.data["agent"]
    assert sync.data["agent"]["provider_name"] == "probe2", sync.data["agent"]
    await fresh.chat("three")
    third = probe.request(SHARED_NAME, 2)
    assert third.path == P2_PATH, third.path


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_same_id_across_providers_boots_into_setup_mode(
    tmp_path: Path,
) -> None:
    """同一个 id 声明在两个 provider → 网关**降级启动**并在问题清单里精确报出（AD11）。

    行为变更（04）：「坏配置 ⇒ 进程拒绝启动」已死——配置写坏时网关降级启动（setup mode），
    唯一修复路径是设置端点。所以这条负面用例验的东西从「进程拒绝启动」换成
    「进程降级启动 + 精确告诉用户哪里错了」：后者是更强的产品保证（用户被指路，
    而不是面对 traceback）。

    ``env`` 被持有到 ``finally``（teardown），否则网关子进程会泄漏（AD17）。
    """
    env = await ProbeEnv.start(
        tmp_path / "duplicate-id",
        models=[{"id": "shared", "name": "p1-upstream"}],
        extra_providers=[
            {"name": "probe2", "models": [{"id": "shared", "name": "p2-upstream"}]}
        ],
    )
    http = DriverHttp(env.gateway_url, started_at=env.started_at)
    try:
        # ① 降级启动：进程活着、health 通（不是秒退，也不是拒绝启动）。
        assert env.process is not None and env.process.poll() is None
        health = await http.health()
        assert health["status"] == "ok", health

        # ② status：valid=false + setup_mode=true + 问题文案含两个 provider 与修复示例。
        status = await http.request("GET", "/api/settings/status")
        assert status["valid"] is False, status
        assert status["setup_mode"] is True, status
        report = "\n".join(problem["message"] for problem in status["problems"])
        assert "duplicate model id 'shared'" in report, report
        assert "provider 'probe' and provider 'probe2'" in report, report
        # 修复示例按「后声明的 provider × 调用名」生成（一次改对）。
        assert "- id: probe2-p2-upstream" in report, report
        assert "name: p2-upstream" in report, report

        # ③ 降级面成立：会话端点 503 setup_mode（不是「拒绝启动」也不是 500）。
        with pytest.raises(DriverHttpError) as failure:
            await http.request("POST", "/api/session/create", body={})
        assert failure.value.status == 503, failure.value.call.render()
        assert failure.value.call.response["error"] == "setup_mode", (
            failure.value.call.response
        )
        assert "duplicate model id 'shared'" in failure.value.call.response["detail"], (
            failure.value.call.response
        )
    finally:
        await http.close()
        await env.stop()
