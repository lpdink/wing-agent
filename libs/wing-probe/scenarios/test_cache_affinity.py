"""缓存亲和（``prompt_cache_key``）端到端红线：无状态化重构后存量行为逐点钉死。

provider 无状态化把 session id 从「实例构造参数」搬到了「每次调用的
RequestOptions」——本文件盯住搬运不丢：**该注入的调用点一个都不能少**。

- ReAct 主调用：每个请求体的 ``prompt_cache_key`` == 该会话自己的 session id；
- 压缩调用（手动 compact 的 LLM 请求）：与主调用同一 key（存量语义：压缩与
  主调用共享同一前缀 cache）；
- 会话隔离：不同会话的 key 各自独立（绝无串味）。

probe 配置本来就开 ``explicit_cache_mode: true``（见 ``env.render_config_yaml``）
——每个请求体都应携带该字段，这正是存量的线上行为。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
MAIN_MODEL = "probe/cache-affinity"
COMPACT_MODEL = "probe/cache-affinity-compact"

#: 压缩产物（格式契约见 compaction 的 <summary> 提取）。
SUMMARY = "Task: answer the user. State: two turns done. Next: continue."


def _cache_key(probe: Probe, model: str, index: int) -> str | None:
    """第 ``index`` 条请求的 prompt_cache_key（缺失即 None——注入丢失的可观测形态）。"""
    return probe.request(model, index).body.get("prompt_cache_key")


@pytest.mark.probe_env(models=[MAIN_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_react_requests_carry_own_session_id(probe: Probe) -> None:
    """ReAct 主调用：两会话的请求各带各自的 session id（亲和 + 隔离）。"""
    probe.register(
        MAIN_MODEL,
        Turn.of(text="reply one"),
        Turn.of(text="reply two"),
        Turn.of(text="reply three"),
    )
    s1 = await probe.session(model=MAIN_MODEL)
    s2 = await probe.session(model=MAIN_MODEL)

    await s1.chat("alpha")
    await s2.chat("beta")
    await s1.chat("gamma")

    assert _cache_key(probe, MAIN_MODEL, 0) == s1.session_id
    assert _cache_key(probe, MAIN_MODEL, 1) == s2.session_id
    assert _cache_key(probe, MAIN_MODEL, 2) == s1.session_id
    assert s1.session_id != s2.session_id


@pytest.mark.probe_env(models=[COMPACT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_compact_request_carries_session_id(probe: Probe) -> None:
    """压缩调用与主调用同 key（手动 compact 的 LLM 请求带本会话 id）。"""
    probe.register(
        COMPACT_MODEL,
        Turn.of(text="reply one"),
        Turn.of(text=f"<summary>{SUMMARY}</summary>"),
    )
    session = await probe.session(model=COMPACT_MODEL)

    await session.chat("alpha")
    await session.compact()

    assert _cache_key(probe, COMPACT_MODEL, 0) == session.session_id
    # compact 请求是第二条（非流式），同样带 key——缓存亲和不只在主路径上
    assert _cache_key(probe, COMPACT_MODEL, 1) == session.session_id
