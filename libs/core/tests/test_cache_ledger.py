"""缓存账本（探针 + 账本 + 归因 + 端点）测试。

覆盖步骤任务书 `01_backend` 的 12 条必须测试，外加三条实现级契约：直播字段与
账本记录与直播同源、压缩调用同样进账本（§8.8）、`src` 元信息在加载期
采集（热路径不扫目录）。口径唯一来源是代码与 `docs/dev/cache-accounting.md`——
任何断言都不该比它更宽。

账本落在进程级 `wing.db`（`cache_call` / `cache_segment`）：断言尽量走
`CacheLedger.report()`（对外口径），只有「存储真的这样落」的那几条才直接读库。
"""

from __future__ import annotations

import asyncio
import json
import sqlite3
import threading
from datetime import datetime, timedelta
from pathlib import Path
from typing import Any, AsyncIterator, Literal, cast
from unittest.mock import MagicMock, patch

import pytest
from fastapi.testclient import TestClient

from wing.agent.event_sink import AgentEventSink
from wing.agent.react_loop import ReActLoop
from wing.chain import TrackedList
from wing.config import ModelSpec, ProviderConfig
from wing.context import Compactor, ContextManager
from wing.context.cache_ledger import (
    MAX_CALLS,
    _PrefixFingerprints,
    CacheLedger,
    build_report,
    empty_report,
)
from wing.db import get_database
from wing.event import LLMCallMetricsEvent
from wing.event_bus import event_bus
from wing.provider.anthropic.provider import AnthropicProvider
from wing.provider.base import ModelProvider, RequestOptions
from wing.provider.openai.provider import OpenAICompatProvider
from wing.schema import (
    CacheCall,
    CachePart,
    CacheProbe,
    CacheSource,
    ChainNode,
    LLMResponse,
    LLMUsage,
    Message,
    TextBlock,
    Tool,
    ToolParam,
)
from wing.schema.cache import fingerprint
from wing.store.file import FileMessageLog
from wing.store.memory import MemorySessionStore

# ─────────────────────────────────────────────────────────────────────────────
# 构造工具
# ─────────────────────────────────────────────────────────────────────────────


def cfg(protocol: Literal["openai", "anthropic"]) -> ProviderConfig:
    return ProviderConfig(
        name=f"t-{protocol}",
        protocol=protocol,
        base_url="https://api.example.com",
        api_key="sk-test",
        explicit_cache_mode=False,
        models=[ModelSpec(name="m")],
    )


def make_tool(name: str) -> Tool:
    return Tool(
        name=name,
        description=f"{name} tool",
        params=[ToolParam(name="q", type="string")],
        function=lambda: None,
    )


SYSTEM = Message(role="system", content="SYS-PROMPT", uuid="sys")
USER = Message(role="user", content="first ask", uuid="u1")
ASSISTANT = Message(role="assistant", content="first answer", uuid="a1")


def write_rules(directory: Path, name: str, text: str) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / name
    path.write_text(text, encoding="utf-8")
    return path


def write_skill(directory: Path, name: str, description: str = "does things") -> Path:
    skill_dir = directory / name
    skill_dir.mkdir(parents=True, exist_ok=True)
    (skill_dir / "SKILL.md").write_text(
        f"---\nname: {name}\ndescription: {description}\n---\n\nbody\n",
        encoding="utf-8",
    )
    return skill_dir


def make_cm(
    tmp_path: Path,
    *,
    session_id: str = "cache-cm",
    system_prompt: str = "SYS-PROMPT",
    rules: str | None = None,
    skills: str | None = None,
    compactor: Any = None,
) -> ContextManager:
    messages: TrackedList[ChainNode] = TrackedList(
        FileMessageLog(tmp_path / session_id)
    )
    return ContextManager(
        session_id=session_id,
        messages=messages,
        system_prompt=system_prompt,
        compactor=compactor,
        rules_patterns=[rules] if rules else None,
        skills_patterns=[skills] if skills else None,
        workspace=str(tmp_path),
    )


def parts(*labels: str) -> list[CachePart]:
    """一串固定文本段（label → 内容一一对应，内容与 label 绑定）。"""
    return [CachePart.text(label, f"content of {label}") for label in labels]


def db_rows(sql: str, params: tuple[Any, ...] = ()) -> list[Any]:
    return get_database().query(sql, params)


def recorded_calls(session_id: str) -> list[Any]:
    """库里该会话的调用行（新→旧），直接读表（只给存储级断言用）。"""
    return db_rows(
        "SELECT * FROM cache_call WHERE session_id = ? ORDER BY id DESC", (session_id,)
    )


async def build_body(provider_cls, messages, tools, options):
    provider = provider_cls(
        cfg("openai" if provider_cls is OpenAICompatProvider else "anthropic")
    )
    try:
        return json.loads(
            json.dumps(provider._build_body(messages, "m", tools, False, options))
        )
    finally:
        await provider.aclose()


# ─────────────────────────────────────────────────────────────────────────────
# 1. 请求体不变：开 / 关探针时序列化出的 body 完全相等
# ─────────────────────────────────────────────────────────────────────────────


class TestRequestBodyUnchanged:
    @pytest.fixture
    def messages(self) -> list[Message]:
        return [
            SYSTEM,
            USER,
            ASSISTANT,
            Message(role="user", content="second ask", uuid="u2"),
        ]

    @pytest.fixture
    def tools(self) -> list[Tool]:
        return [make_tool("Bash"), make_tool("Read")]

    @pytest.mark.asyncio
    async def test_openai_body_is_byte_identical(self, messages, tools):
        """探针开着时 body 与关着时逐字段相同（只观察，不改写）。"""
        off = await build_body(OpenAICompatProvider, messages, tools, RequestOptions())
        probe = CacheProbe()
        on = await build_body(
            OpenAICompatProvider,
            messages,
            tools,
            RequestOptions(cache_parts=tuple(parts("system:base")), cache_probe=probe),
        )
        assert off == on
        # 不是空转：探针确实收到了分段
        assert [segment.label for segment in probe.segments] == [
            "system:base",
            "system:wire",
            "tools:Bash",
            "tools:Read",
            "history:u1:user",
            "history:a1:assistant",
            "history:u2:user",
        ]

    @pytest.mark.asyncio
    async def test_anthropic_body_is_byte_identical(self, messages, tools):
        off = await build_body(AnthropicProvider, messages, tools, RequestOptions())
        probe = CacheProbe()
        on = await build_body(
            AnthropicProvider,
            messages,
            tools,
            RequestOptions(cache_parts=tuple(parts("system:base")), cache_probe=probe),
        )
        assert off == on
        assert [segment.label for segment in probe.segments] == [
            "system:base",
            "system:wire",
            "tools:Bash",
            "tools:Read",
            "history:u1:user",
            "history:a1:assistant",
            "history:u2:user",
        ]

    @pytest.mark.asyncio
    async def test_cache_control_does_not_leak_into_history_hashes(self, messages):
        """显式缓存模式的断点标记不进 history 段 hash（否则每次都判 prefix_broken）。"""
        provider = OpenAICompatProvider(
            cfg("openai").model_copy(update={"explicit_cache_mode": True})
        )
        try:
            first, second = CacheProbe(), CacheProbe()
            provider._build_body(
                messages,
                "m",
                None,
                False,
                RequestOptions(
                    cache_parts=(parts("system:base")[0],), cache_probe=first
                ),
            )
            provider._build_body(
                messages + [Message(role="user", content="next", uuid="u3")],
                "m",
                None,
                False,
                RequestOptions(
                    cache_parts=(parts("system:base")[0],), cache_probe=second
                ),
            )
        finally:
            await provider.aclose()
        # 首次请求整段是第二次的前缀（cache_control 的移动不算变化）——若断点标记
        # 进了 history 段 hash，第一次的最后一条消息会因标记漂移而对不上。
        assert [segment.sha for segment in second.segments[: len(first.segments)]] == [
            segment.sha for segment in first.segments
        ]


# ─────────────────────────────────────────────────────────────────────────────
# 2. 分段顺序（rules 两文件 + skills 两项 + 两个工具 + 三条消息）
# ─────────────────────────────────────────────────────────────────────────────


class TestSegmentOrder:
    def test_labels_follow_contract_order(self, tmp_path: Path):
        write_rules(tmp_path / "rules", "01-first.md", "rule one")
        write_rules(tmp_path / "rules", "02-second.md", "rule two")
        first_skill = write_skill(tmp_path / "skills", "alpha")
        second_skill = write_skill(tmp_path / "skills", "beta")
        cm = make_cm(tmp_path, rules="rules/*.md", skills="skills/*/SKILL.md")
        cm.add_messages(
            [USER, ASSISTANT, Message(role="user", content="again", uuid="u2")]
        )

        probe = CacheProbe()
        options = RequestOptions(
            cache_parts=cm._cache_parts_for(RequestOptions(cache_probe=probe)),
            cache_probe=probe,
        )
        provider = OpenAICompatProvider(cfg("openai"))
        provider._build_body(
            [cm.system_prompt] + cm.get_context_window(),
            "m",
            [make_tool("Bash"), make_tool("Read")],
            False,
            options,
        )

        assert [segment.label for segment in probe.segments] == [
            "system:base",
            f"rules:{tmp_path / 'rules' / '01-first.md'}",
            f"rules:{tmp_path / 'rules' / '02-second.md'}",
            "skills:alpha",
            "skills:beta",
            "system:wire",
            "tools:Bash",
            "tools:Read",
            "history:u1:user",
            "history:a1:assistant",
            "history:u2:user",
        ]
        # src 元信息只挂在 rules / skills 段上，且来自加载期快照
        by_label = {segment.label: segment for segment in probe.segments}
        rule_src = by_label[f"rules:{tmp_path / 'rules' / '01-first.md'}"].src
        assert rule_src is not None and rule_src.path.endswith("01-first.md")
        assert rule_src.size == len("rule one")
        skill_src = by_label["skills:alpha"].src
        assert skill_src is not None and skill_src.path == str(first_skill)
        assert by_label["system:wire"].src is None
        beta_src = by_label["skills:beta"].src
        assert beta_src is not None and beta_src.path == str(second_skill)

    def test_parts_are_cached_without_touching_the_disk(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """热路径零文件系统：加载后再算分段不 stat / 不读目录。"""
        write_rules(tmp_path / "rules", "one.md", "rule one")
        write_skill(tmp_path / "skills", "alpha")
        cm = make_cm(tmp_path, rules="rules/*.md", skills="skills/*/SKILL.md")

        def _boom(*args: Any, **kwargs: Any):
            raise AssertionError("hot path must not touch the filesystem")

        monkeypatch.setattr("wing.context.resources.os.stat", _boom)
        monkeypatch.setattr("wing.context.resources.glob.glob", _boom)
        monkeypatch.setattr("wing.context.resources.Path.read_text", _boom)
        segments = cm.cache_system_parts()
        assert [segment.label for segment in segments] == [
            "system:base",
            f"rules:{tmp_path / 'rules' / 'one.md'}",
            "skills:alpha",
        ]


class TestCachePartsPlumbing:
    @pytest.mark.asyncio
    async def test_get_messages_for_llm_hands_parts_only_when_probing(
        self, tmp_path: Path
    ):
        """探针关闭 = 空分段（零额外计算）；打开 = system 侧分段随结果交回调用方。"""
        write_rules(tmp_path / "rules", "one.md", "rule one")
        cm = make_cm(tmp_path, rules="rules/*.md")

        # 无 compactor 时 CM 不碰 provider —— 传 None 即可（cast 只为类型面）
        off = await cm.get_messages_for_llm(
            model="m",
            model_provider=cast("ModelProvider", None),
            current_tools=lambda: [],
            options=None,
        )
        assert off.cache_parts == ()

        on = await cm.get_messages_for_llm(
            model="m",
            model_provider=cast("ModelProvider", None),
            current_tools=lambda: [],
            options=RequestOptions(cache_probe=CacheProbe()),
        )
        assert [part.label for part in on.cache_parts] == [
            "system:base",
            f"rules:{tmp_path / 'rules' / 'one.md'}",
        ]


# ─────────────────────────────────────────────────────────────────────────────
# 3-8. 归因（首调用 cold / hit / modify / insert / tools / 欠兑现 / tol 边界）
# ─────────────────────────────────────────────────────────────────────────────


class TestAttribution:
    def test_first_call_is_cold_without_fork(self):
        ledger = CacheLedger("s1")
        observation = ledger.record(
            parts("system:base", "tools:Bash"),
            purpose="main",
            model="m",
            provider="p",
            prompt=1000,
            cached=0,
        )
        assert observation is not None
        assert observation.verdict == "cold"
        assert observation.expected is None
        assert observation.diverged == ""
        report = ledger.report()
        assert report["forks"] == []
        assert report["summary"]["calls"] == 1
        # 首调用无参照：Σexpected = 0，分母非 0 → 理论命中率 0.0（不是 null）
        assert report["summary"]["expected_tokens"] == 0
        assert report["summary"]["theory_rate"] == 0.0

    def test_second_call_with_same_prefix_and_full_cache_is_hit(self):
        ledger = CacheLedger("s2")
        base = parts("system:base", "tools:Bash")
        ledger.record(
            base, purpose="main", model="m", provider="p", prompt=1000, cached=0
        )
        observation = ledger.record(
            base + parts("history:u1:user"),
            purpose="main",
            model="m",
            provider="p",
            prompt=1200,
            cached=1000,
        )
        assert observation is not None
        assert observation.verdict == "hit"
        assert observation.expected == 1000
        assert observation.diverged == ""
        # 段落行按指纹各存一份：第一份 2 段、第二份 3 段（去重由
        # `test_shared_segment_lists_are_stored_once` 单独钉住）
        assert db_rows("SELECT COUNT(*) AS n FROM cache_segment", ())[0]["n"] == 5
        report = ledger.report()
        assert report["summary"]["calls"] == 2
        assert report["summary"]["expected_tokens"] == 1000
        assert report["summary"]["theory_rate"] == round(1000 / 2200, 6)
        assert report["summary"]["actual_rate"] == round(1000 / 2200, 6)

    def test_modified_rules_file_is_prefix_broken_and_names_the_path(
        self, tmp_path: Path
    ):
        rule_path = write_rules(tmp_path / "rules", "one.md", "OLD RULE")
        cm = make_cm(tmp_path, rules="rules/*.md")
        ledger = CacheLedger("s3")

        first = cm.cache_system_parts() + [
            CachePart.text("system:wire", "wire"),
            CachePart.text("tools:Bash", "tool"),
        ]
        ledger.record(
            first, purpose="main", model="m", provider="p", prompt=5000, cached=0
        )

        write_rules(tmp_path / "rules", "one.md", "NEW RULE")
        cm.reload_skills_and_rules()
        second = cm.cache_system_parts() + [
            CachePart.text("system:wire", "wire"),
            CachePart.text("tools:Bash", "tool"),
            CachePart.text("history:u1:user", "next"),
        ]
        observation = ledger.record(
            second, purpose="main", model="m", provider="p", prompt=6000, cached=1200
        )

        assert observation is not None
        assert observation.verdict == "prefix_broken"
        assert observation.expected is None
        assert observation.diverged == f"rules:{rule_path}"

        report = ledger.report()
        assert len(report["forks"]) == 1
        fork = report["forks"][0]
        assert fork["kind"] == "modify"
        assert fork["label"] == f"rules:{rule_path}"
        assert fork["index"] == 1
        assert fork["blame"] == "external"
        assert fork["avoidable"] is True
        assert fork["verdict"] == "prefix_broken"
        # 最近一条调用（分叉的对照侧）fork 点之后的段数：len(C) - k
        assert fork["segments_affected"] == len(first) - 1
        # 前缀彻底断裂 ⇒ 没有参照 ⇒ tokens_lost 为 null（展示为 `—`）
        assert fork["tokens_lost"] is None
        # 证据：能直接点名文件与两侧 sha
        assert fork["detail"]["path"] == str(rule_path)
        assert fork["detail"]["mtime"] is not None
        # 无参照（前缀被外部改动打断）时对照侧回落到最近一条调用：`--why` 才能
        # 给出 `sha_before → sha_after` 与「什么时候变的」——这是本功能的主用例。
        assert fork["detail"]["sha_before"] is not None
        assert fork["detail"]["sha_before"] != fork["detail"]["sha_after"]
        assert fork["detail"]["ref_ts"] is not None

    def test_inserted_rules_file_reports_tail_segment_count(self, tmp_path: Path):
        write_rules(tmp_path / "rules", "a-first.md", "A")
        write_rules(tmp_path / "rules", "c-third.md", "C")
        cm = make_cm(tmp_path, rules="rules/*.md")
        ledger = CacheLedger("s4")

        def segments() -> list[CachePart]:
            return cm.cache_system_parts() + [
                CachePart.text("system:wire", "wire"),
                CachePart.text("tools:Bash", "tool"),
                CachePart.text("history:u1:user", "ask"),
            ]

        first = segments()
        ledger.record(
            first, purpose="main", model="m", provider="p", prompt=4000, cached=0
        )

        write_rules(tmp_path / "rules", "b-second.md", "B")  # 插在中间
        cm.reload_skills_and_rules()
        second = segments()
        observation = ledger.record(
            second, purpose="main", model="m", provider="p", prompt=4500, cached=1000
        )
        assert observation is not None
        assert observation.verdict == "prefix_broken"
        new_label = f"rules:{tmp_path / 'rules' / 'b-second.md'}"
        assert observation.diverged == new_label

        fork = ledger.report()["forks"][0]
        assert fork["kind"] == "insert"
        assert fork["label"] == new_label
        assert fork["index"] == 2  # system:base / rules:a-first.md 之后
        # 对照侧尾部段数 = 总段数 - 公共前缀长度
        assert fork["segments_affected"] == len(first) - 2

    def test_tool_declaration_change_blames_tools(self):
        ledger = CacheLedger("s5")
        first = [
            CachePart.text("system:base", "S"),
            CachePart.text("system:wire", "wire"),
            CachePart.text("tools:Bash", "bash"),
            CachePart.text("tools:Read", "read"),
            CachePart.text("history:u1:user", "ask"),
        ]
        ledger.record(
            first, purpose="main", model="m", provider="p", prompt=3000, cached=0
        )
        second = [
            CachePart.text("system:base", "S"),
            CachePart.text("system:wire", "wire"),
            CachePart.text("tools:Bash", "bash"),
            CachePart.text("tools:Glob", "glob"),
            CachePart.text("history:u1:user", "ask"),
            CachePart.text("history:a1:assistant", "answer"),
        ]
        observation = ledger.record(
            second, purpose="main", model="m", provider="p", prompt=3600, cached=2000
        )
        assert observation is not None
        assert observation.verdict == "prefix_broken"
        assert observation.diverged == "tools:Glob"
        fork = ledger.report()["forks"][0]
        assert fork["kind"] == "insert"
        assert fork["label"] == "tools:Glob"
        assert fork["blame"] == "tools"
        assert fork["avoidable"] is True

    def test_zero_cached_with_identical_prefix_is_provider_underdelivered(self):
        ledger = CacheLedger("s6")
        base = parts("system:base", "system:wire", "tools:Bash", "history:u1:user")
        ledger.record(
            base, purpose="main", model="m", provider="p", prompt=10000, cached=0
        )
        observation = ledger.record(
            base + parts("history:a1:assistant"),
            purpose="main",
            model="m",
            provider="p",
            prompt=12000,
            cached=0,
        )
        assert observation is not None
        assert observation.verdict == "provider_underdelivered"
        assert observation.expected == 10000
        assert observation.diverged == ""
        report = ledger.report()
        # provider 欠账进汇总（forks 只收有分叉的调用）
        assert report["summary"]["provider_debt_rate"] == round(10000 / 22000, 6)
        assert report["forks"] == []

    @pytest.mark.parametrize(
        ("cached", "expected_verdict"),
        [
            (10000 - 256, "block_granularity"),  # diff == tol
            (10000 - 257, "provider_underdelivered"),  # diff == tol + 1
        ],
    )
    def test_tolerance_boundary(self, cached: int, expected_verdict: str):
        """tol = max(256, ceil(0.02 * ref.prompt))；边界两侧各一条。"""
        ledger = CacheLedger("s7")
        base = parts("system:base", "tools:Bash")
        ledger.record(
            base, purpose="main", model="m", provider="p", prompt=10000, cached=0
        )
        observation = ledger.record(
            base + parts("history:u1:user"),
            purpose="main",
            model="m",
            provider="p",
            prompt=12000,
            cached=cached,
        )
        assert observation is not None
        assert observation.verdict == expected_verdict
        assert observation.expected == 10000

    def test_reference_search_holds_up_as_the_session_grows(self):
        """段数一路增长的长会话：每一轮都拿上一轮当参照（`hit`），不回退成 cold。

        参照查找是「候选的段数 n + 前 n 段的指纹」——按需算前缀指纹的快路径若漏判，
        长会话里就会表现为命中率突然掉成 0（每一轮都判 cold / prefix_broken）。
        """
        ledger = CacheLedger("long-session")
        base = parts("system:base", "tools:Bash")
        for round_index in range(60):
            segments = base + [
                CachePart.text(f"history:{n}:user", f"ask-{n}")
                for n in range(round_index + 1)
            ]
            observation = ledger.record(
                segments,
                purpose="main",
                model="m",
                provider="p",
                prompt=1000 + round_index * 100,
                cached=1000 + (round_index - 1) * 100 if round_index else 0,
            )
            assert observation is not None
            assert observation.verdict == ("cold" if round_index == 0 else "hit"), (
                f"第 {round_index} 轮: {observation.verdict}"
            )
            if round_index:
                assert observation.expected == 1000 + (round_index - 1) * 100
        assert ledger.report(limit=1)["forks"] == []

    def test_twin_call_uses_identical_segments_as_reference(self):
        """段列表完全相同（重试 / 重放）也有参照——契约的严格前缀格子之外的补齐。"""
        ledger = CacheLedger("s8")
        base = parts("system:base", "tools:Bash")
        ledger.record(
            base, purpose="main", model="m", provider="p", prompt=5000, cached=0
        )
        observation = ledger.record(
            base, purpose="main", model="m", provider="p", prompt=5000, cached=5000
        )
        assert observation is not None
        assert observation.verdict == "hit"
        assert observation.expected == 5000
        assert observation.diverged == ""

    def test_expired_reference_is_cold_expired(self):
        """参照调用超过 TTL（72h）→ cold_expired，expected 归 null。"""
        ledger = CacheLedger("s9")
        base = parts("system:base", "tools:Bash")
        ledger.record(
            base,
            purpose="main",
            model="m",
            provider="p",
            prompt=1000,
            cached=0,
            now=datetime.now() - timedelta(hours=100),
        )
        observation = ledger.record(
            base + parts("history:u1:user"),
            purpose="main",
            model="m",
            provider="p",
            prompt=1300,
            cached=1000,
        )
        assert observation is not None
        assert observation.verdict == "cold_expired"
        assert observation.expected is None

    def test_compact_attribution_for_history_forks(self):
        """history 分叉 + 参照与本次之间存在 compact 调用 → blame=compact（不可避免）。

        构造：c1 是本次的严格前缀（参照），c2 是夹在中间的压缩调用（最近一条，
        因此分叉点按 c2 算），c3 回落到 c1 分支继续跑 —— 这正是"压缩后换分支"的
        形状：参照存在、但上一次调用的前缀没能延续。
        """
        ledger = CacheLedger("s10")
        ledger.record(
            parts("system:base", "system:wire", "history:u1:user"),
            purpose="main",
            model="m",
            provider="p",
            prompt=8000,
            cached=0,
        )
        ledger.record(
            parts("system:base", "system:wire", "history:u9:assistant"),
            purpose="compact",
            model="m",
            provider="p",
            prompt=8500,
            cached=1000,
        )
        observation = ledger.record(
            parts(
                "system:base",
                "system:wire",
                "history:u1:user",
                "history:a1:assistant",
                "history:u2:user",
            ),
            purpose="main",
            model="m",
            provider="p",
            prompt=3000,
            cached=1200,
        )
        assert observation is not None
        assert observation.verdict == "prefix_broken"
        assert observation.diverged == "history:u1:user"
        fork = ledger.report()["forks"][0]
        assert fork["label"] == "history:u1:user"
        assert fork["kind"] == "insert"
        assert fork["blame"] == "compact"
        assert fork["avoidable"] is False
        assert fork["tokens_lost"] is not None  # 有参照 → 估算可得
        assert fork["detail"]["ref_ts"] is not None


# ─────────────────────────────────────────────────────────────────────────────
# 8b. fork detail 的「对照侧」：说「这里变了」的那一行必须对得上证据（cloud S2/N4）
# ─────────────────────────────────────────────────────────────────────────────


class TestForkDetailSides:
    def test_reverted_rules_compare_against_the_intermediate_state(
        self, tmp_path: Path
    ):
        """改 rules → 改回：`sha_before` 必须是**中间那一版**。

        参照（第 2 轮）是本次的前缀，因此它在 rules 位上与本次同值——拿它当 `sha_before`
        只能得到「两个相同的 sha」，与「modify、有变化」自相矛盾（cloud S2 的复现）。
        时间戳显式给（毫秒级 `datetime.now()` 在快机器上会同刻），`ref_ts` 与 `sha` 是否
        指向同一条调用才可判。
        """
        rule_path = write_rules(tmp_path / "rules", "one.md", "V1 RULE")
        rule_label = f"rules:{rule_path}"
        cm = make_cm(tmp_path, rules="rules/*.md")
        ledger = CacheLedger("s-revert")

        def segments(*ask_uuids: str) -> list[CachePart]:
            return (
                cm.cache_system_parts()
                + [CachePart.text("system:wire", "wire")]
                + [CachePart.text(f"history:{uuid}:user", uuid) for uuid in ask_uuids]
            )

        first = segments("u1")
        second = segments("u1", "u2")
        write_rules(tmp_path / "rules", "one.md", "V2 RULE")
        cm.reload_skills_and_rules()
        changed = segments("u1", "u2", "u3")
        write_rules(tmp_path / "rules", "one.md", "V1 RULE")
        cm.reload_skills_and_rules()
        reverted = segments("u1", "u2", "u3", "u4")

        for index, (segments_, prompt, cached) in enumerate(
            [
                (first, 1000, 0),
                (second, 1500, 1000),
                (changed, 2000, 1500),
                (reverted, 2500, 2000),
            ]
        ):
            ledger.record(
                segments_,
                purpose="main",
                model="m",
                provider="p",
                prompt=prompt,
                cached=cached,
                now=datetime(2026, 10, 18, 10, index),
            )
        fork = ledger.report()["forks"][0]

        assert fork["label"] == rule_label
        assert fork["kind"] == "modify"
        sha_v1 = next(part.sha for part in first if part.label == rule_label)
        sha_v2 = next(part.sha for part in changed if part.label == rule_label)
        assert next(part.sha for part in reverted if part.label == rule_label) == sha_v1
        assert fork["detail"]["sha_before"] == sha_v2, (
            "对照侧必须是中间那一版："
            f"sha_before={fork['detail']['sha_before']} sha_v2={sha_v2}"
        )
        assert fork["detail"]["sha_after"] == sha_v1
        assert fork["detail"]["sha_before"] != fork["detail"]["sha_after"]
        # 时刻与 sha 同源：两者都指向对照侧（第 3 轮，带 V2 的那次调用），而不是参照（第 2 轮）。
        calls = recorded_calls("s-revert")
        assert calls[0]["ref_fp"] == calls[2]["fp"]
        assert fork["detail"]["ref_ts"] == calls[1]["ts"]
        assert calls[1]["ts"] != calls[2]["ts"]

    def test_reference_left_the_ledger_but_the_compared_call_is_still_named(self):
        """参照已滚出账本、对照侧（邻座）还在 → 时刻跟着对照侧，不指向无关的调用。"""
        base = CachePart.text("system:base", "base")
        old_rule = CachePart.text("rules:/r.md", "old")
        new_rule = CachePart.text("rules:/r.md", "new")
        calls = [
            _call_row(ts="2026-10-18T10:00:00.000", fp="old", verdict="cold"),
            _call_row(
                ts="2026-10-18T10:05:00.000",
                fp="new",
                verdict="prefix_broken",
                ref_fp="gone",
                fork=_fork_row(index=1),
            ),
        ]
        segment_lists = {"new": [base, new_rule], "old": [base, old_rule]}
        fork = build_report(
            "gone",
            list(reversed(calls)),
            limit=20,
            segment_at=lambda fp, index: _segment_of(segment_lists, fp, index),
        )["forks"][0]
        assert fork["detail"]["sha_before"] == old_rule.sha
        assert fork["detail"]["sha_before"] != fork["detail"]["sha_after"]
        # 对照侧是邻座那条（sha 与 ts 同源），而不是记在案却已不可定位的参照
        assert fork["detail"]["ref_ts"] == "2026-10-18T10:00:00.000"

    def test_comparison_side_gone_leaves_both_the_sha_and_the_ts_empty(self):
        """对照侧整个滚出账本（分叉行成了最旧一条）→ `sha_before` 与 `ref_ts` 一起缺省。

        `--why` 因此整行不显示 `ref`：不拿「紧邻的后来者」的 ts 冒充那个已经不可见的时刻。
        """
        new_rule = CachePart.text("rules:/r.md", "new")
        segments = [CachePart.text("system:base", "base"), new_rule]
        calls = [
            _call_row(
                ts="2026-10-18T10:05:00.000",
                fp="new",
                verdict="prefix_broken",
                ref_fp="gone",
                fork=_fork_row(index=1),
            )
        ]
        fork = build_report(
            "gone",
            calls,
            limit=20,
            segment_at=lambda fp, index: _segment_of({"new": segments}, fp, index),
        )["forks"][0]
        assert fork["detail"]["sha_before"] is None
        assert fork["detail"]["ref_ts"] is None
        assert fork["detail"]["sha_after"] == new_rule.sha


def _call_row(
    *,
    ts: str,
    fp: str,
    verdict: str,
    ref_fp: str | None = None,
    fork: Any = None,
    purpose: str = "main",
    prompt: int = 1000,
    cached: int = 0,
    expected: int | None = None,
) -> CacheCall:
    return CacheCall(
        ts=ts,
        fp=fp,
        purpose=purpose,
        model="m",
        provider="p",
        prompt=prompt,
        cached=cached,
        verdict=verdict,
        ref_fp=ref_fp,
        expected=expected,
        fork=fork,
    )


def _fork_row(index: int) -> Any:
    from wing.schema import CacheFork

    return CacheFork(
        kind="modify", index=index, label="rules:/r.md", segments_affected=1
    )


def _segment_of(lists: dict[str, list[CachePart]], fp: str | None, index: int):
    segments = lists.get(fp or "", [])
    return segments[index] if index < len(segments) else None


# ─────────────────────────────────────────────────────────────────────────────
# 9. 环形上界 + 段落行回收
# ─────────────────────────────────────────────────────────────────────────────


class TestRingBound:
    def test_calls_are_capped_and_orphan_segments_are_garbage_collected(self):
        ledger = CacheLedger("ring")
        base = CachePart.text("system:base", "S")
        for index in range(250):
            ledger.record(
                [base, CachePart.text(f"history:{index}:user", f"ask-{index}")],
                purpose="main",
                model="m",
                provider="p",
                prompt=100 + index,
                cached=0,
            )
        stored = recorded_calls("ring")
        assert len(stored) == MAX_CALLS == 200
        # 保留的是最近 200 条（最旧的 50 条被挤出）
        assert stored[-1]["prompt"] == 100 + 50
        assert stored[0]["prompt"] == 100 + 249
        # 无孤儿段落行：段落表里的指纹都被存活的调用引用
        alive = {row["fp"] for row in stored}
        orphans = db_rows(
            "SELECT DISTINCT fp FROM cache_segment WHERE session_id = ?", ("ring",)
        )
        assert {row["fp"] for row in orphans} == alive

    def test_ring_is_per_session(self):
        """裁剪只动本会话的行：另一个会话的调用与段落行一条都不许少。"""
        keep = CacheLedger("keep")
        owner = CacheLedger("owner")
        keep.record(
            parts("system:base"),
            purpose="main",
            model="m",
            provider="p",
            prompt=42,
            cached=0,
        )
        for index in range(MAX_CALLS + 5):
            owner.record(
                parts("system:base", f"history:{index}:user"),
                purpose="main",
                model="m",
                provider="p",
                prompt=100 + index,
                cached=0,
            )
        assert len(recorded_calls("owner")) == MAX_CALLS
        assert [row["prompt"] for row in recorded_calls("keep")] == [42]
        kept = db_rows(
            "SELECT COUNT(*) AS n FROM cache_segment WHERE session_id = ?", ("keep",)
        )
        assert kept[0]["n"] == 1

    def test_shared_segment_lists_are_stored_once(self):
        """同一份段列表被多条调用引用 → 段落行只存一份（内容寻址去重）。"""
        ledger = CacheLedger("dedup")
        base = parts("system:base", "tools:Bash")
        for _ in range(5):
            ledger.record(
                base, purpose="main", model="m", provider="p", prompt=100, cached=0
            )
        assert len(recorded_calls("dedup")) == 5
        rows = db_rows(
            "SELECT COUNT(*) AS n FROM cache_segment WHERE session_id = ?", ("dedup",)
        )
        assert rows[0]["n"] == len(base)


# ─────────────────────────────────────────────────────────────────────────────
# 10. best-effort：写库失败只 WARN，绝不影响 LLM 调用
# ─────────────────────────────────────────────────────────────────────────────


class _ExplodingDatabase:
    """永远失败 / 降级的库（模拟磁盘满、只读挂载、打不开）。"""

    ready = False

    def query(self, sql: str, params: Any = ()) -> list[Any]:
        return []

    def execute(self, sql: str, params: Any = ()) -> bool:
        return False

    def executemany(self, sql: str, rows: Any) -> bool:
        return False

    def insert(self, sql: str, params: Any = ()) -> int | None:
        return None

    def transaction(self):
        from contextlib import contextmanager

        @contextmanager
        def _degraded():
            yield False

        return _degraded()


class _FailingDelete:
    """`DELETE FROM cache_segment` 抛 IO 错的连接代理（模拟磁盘满）。"""

    def __init__(self, conn: Any) -> None:
        self._conn = conn

    def __getattr__(self, name: str) -> Any:
        return getattr(self._conn, name)

    def execute(self, sql: str, *args: Any) -> Any:
        if sql.lstrip().startswith("DELETE FROM cache_segment"):
            raise sqlite3.OperationalError("disk I/O error")
        return self._conn.execute(sql, *args)


class TestBestEffort:
    def test_write_failure_returns_none_and_warns(self):
        ledger = CacheLedger("boom", database=_ExplodingDatabase())  # ty: ignore[invalid-argument-type]
        assert (
            ledger.record(
                parts("system:base"),
                purpose="main",
                model="m",
                provider="p",
                prompt=10,
                cached=0,
            )
            is None
        )
        assert ledger.report()["calls"] == []

    def test_write_failure_inside_a_live_transaction_is_reported(
        self, wing_warnings: list[str]
    ):
        """库开着但写不进去（约束 / 磁盘满）→ 返回 None + 一条 WARNING，绝不上抛。"""
        database = get_database()
        with patch.object(database, "execute", return_value=False):
            ledger = CacheLedger("rejected", database=database)
            assert (
                ledger.record(
                    parts("system:base"),
                    purpose="main",
                    model="m",
                    provider="p",
                    prompt=10,
                    cached=0,
                )
                is None
            )
        assert any("cache ledger record lost" in message for message in wing_warnings)
        assert recorded_calls("rejected") == []

    def test_a_rolled_back_record_never_returns_an_observation(
        self, wing_warnings: list[str]
    ):
        """语句都返回了、事务最后回滚 → 不得回一个「账本里有」的归因。

        构造的是「环形裁剪的孤儿回收失败」：它在一次记录的最后一笔写，插入与段落表
        都已经成功——只看那两条的返回值会以为记上了。归因若照旧返回，直播字段就会
        展示一条库里根本不存在的记录（账本与直播同源）。
        """
        database = get_database()
        ledger = CacheLedger("rolled-back")
        for index in range(MAX_CALLS):
            assert (
                ledger.record(
                    parts("system:base", f"history:{index}:user"),
                    purpose="main",
                    model="m",
                    provider="p",
                    prompt=100 + index,
                    cached=0,
                )
                is not None
            )
        # 第 201 条触发环形裁剪：让孤儿段落的 DELETE 抛 IO 错（磁盘满的样子）
        conn = database._conn
        assert conn is not None
        database._conn = _FailingDelete(conn)  # ty: ignore[invalid-assignment]
        try:
            observation = ledger.record(
                parts("system:base", f"history:{MAX_CALLS}:user"),
                purpose="main",
                model="m",
                provider="p",
                prompt=100 + MAX_CALLS,
                cached=0,
            )
        finally:
            database._conn = conn
        assert observation is None, "回滚的那次记录不许返回归因"
        assert any("cache ledger record lost" in m for m in wing_warnings)
        # 事务整体回滚：既没有多出一行，也没有把本该被裁掉的调用删掉
        stored = recorded_calls("rolled-back")
        assert len(stored) == MAX_CALLS
        assert stored[0]["prompt"] == 100 + MAX_CALLS - 1

    @pytest.mark.asyncio
    async def test_llm_call_survives_ledger_failure(self, tmp_path: Path):
        """整条链：账本写库炸了，模型调用照常返回、metrics 照常发射。"""
        events: list[Any] = []
        event_bus.subscribe(events.append)
        try:
            cm = make_cm(tmp_path, session_id="boom-chain")
            cm.add_message(USER)
            loop = _loop_for(cm)
            probe = CacheProbe()
            probe.set_segments(cm.cache_system_parts())  # 直调 _call_llm 不走 provider
            options = RequestOptions(
                cache_parts=tuple(cm.cache_system_parts()), cache_probe=probe
            )
            provider = _FakeProvider(
                [
                    LLMResponse(
                        content="hi",
                        usage=LLMUsage(
                            prompt_tokens=100, completion_tokens=5, model="m"
                        ),
                    ),
                    LLMResponse(
                        content_blocks=[TextBlock(text="hi")], usage=LLMUsage(model="m")
                    ),
                ]
            )
            with patch(
                "wing.context.cache_ledger.CacheLedger.record",
                return_value=None,
            ):
                message = await loop._call_llm(
                    provider,  # ty: ignore[invalid-argument-type]
                    [SYSTEM, USER],
                    "m",
                    None,
                    options,
                )
            assert message.content == "hi"
            metrics = [
                event for event in events if isinstance(event, LLMCallMetricsEvent)
            ]
            assert len(metrics) == 1
            # 记账失败 → 归因不可得，但事件照常发射（字段回落缺省）
            assert metrics[0].cache_verdict == ""
            assert metrics[0].cache_expected_tokens is None
        finally:
            event_bus._subscribers.clear()
            event_bus._routing.clear()


class TestWeakTyping:
    """SQLite 是弱类型：INTEGER 列里可以是 TEXT（外部写入 / 手工 sqlite3）。

    旧实现有 `normalize_ledger` 逐字段归一；换成表之后这份防线要落在读行这一层，
    否则一条类型错位的行就能让端点 500，或者把这个会话的记账永久卡死。
    """

    def _insert_bad_row(
        self, session_id: str, column: str, value: Any, **overrides: Any
    ) -> None:
        row = {
            "session_id": session_id,
            "ts": "2026-10-18T15:41:02.123",
            "ts_ms": 1,
            "fp": "bad",
            "purpose": "main",
            "model": "m",
            "provider": "p",
            "prompt": 100,
            "cached": 0,
            "expected": None,
            "ref_fp": None,
            "verdict": "cold",
            "segment_count": 1,
        }
        row.update(overrides)
        row[column] = value  # 坏值最后写：overrides 不该盖掉它
        assert get_database().execute(
            f"INSERT INTO cache_call ({', '.join(row)}) "
            f"VALUES ({', '.join('?' * len(row))})",
            tuple(row.values()),
        )

    def test_text_in_an_integer_column_does_not_break_the_report(self):
        """`expected` 是 TEXT → 当「没有理论水位」收下，不参与汇总里的减法。"""
        self._insert_bad_row("weak-report", "expected", "abc")
        report = CacheLedger("weak-report").report()
        assert [call["expected"] for call in report["calls"]] == [None]
        assert report["summary"]["calls"] == 1
        assert report["summary"]["expected_tokens"] == 0
        assert report["summary"]["provider_debt_rate"] == 0.0

    def test_text_in_an_integer_column_does_not_wedge_recording(self):
        """坏行留在窗口里也不能让后续记账永久失败（参照比较会拿它的段数）。"""
        self._insert_bad_row("weak-record", "segment_count", "abc")
        ledger = CacheLedger("weak-record")
        for index in range(3):
            observation = ledger.record(
                parts("system:base", f"history:{index}:user"),
                purpose="main",
                model="m",
                provider="p",
                prompt=100 + index,
                cached=0,
            )
            assert observation is not None, f"第 {index} 轮被坏行卡住了"
        # 坏行仍是「一行调用」（它的段数按 0 处理、不参与前缀比较），三笔新记录照常落库
        assert len(ledger.report()["calls"]) == 4

    def test_a_non_integer_fork_index_is_dropped_not_fatal(self):
        """分叉位点不是整数 → 当成「没有分叉」，但这一行调用仍然算数。

        （TEXT 列有 TEXT 亲和性，写进去的整数会被转成字符串，所以这里能构造出
        类型错位的只有 INTEGER 亲和性的列。）
        """
        fork = {
            "fork_kind": "modify",
            "fork_index": 1,
            "fork_label": "rules:/r.md",
            "fork_segments_affected": 2,
        }
        self._insert_bad_row("weak-fork", "fork_index", "abc", **fork)
        report = CacheLedger("weak-fork").report()
        assert report["forks"] == []
        assert len(report["calls"]) == 1


# ─────────────────────────────────────────────────────────────────────────────
# 11. 会话后端不再影响账本：写入的是进程级库，与会话日志解耦
# ─────────────────────────────────────────────────────────────────────────────


class TestStorageIsDecoupledFromTheSessionLog:
    def test_memory_backend_lands_in_the_database(self, tmp_path: Path):
        """`backend: memory` 的会话：账本照样在库里（与审计行同口径），不产生会话文件。"""
        store = MemorySessionStore()
        assert store.open_log("mem") is not None
        ledger = CacheLedger("mem")
        base = parts("system:base", "tools:Bash")
        first = ledger.record(
            base, purpose="main", model="m", provider="p", prompt=900, cached=0
        )
        second = ledger.record(
            base + parts("history:u1:user"),
            purpose="main",
            model="m",
            provider="p",
            prompt=1100,
            cached=900,
        )
        assert first is not None and first.verdict == "cold"
        assert second is not None and second.verdict == "hit"
        assert ledger.report()["summary"]["calls"] == 2
        assert list(tmp_path.rglob("*")) == []  # 会话目录里零痕迹

    def test_ledger_does_not_need_a_message_log(self):
        """没有 MessageLog 的纯内存链：记账照旧（账本不再经 aux kv 通道）。"""
        ledger = CacheLedger("pure")
        observation = ledger.record(
            parts("system:base"),
            purpose="main",
            model="m",
            provider="p",
            prompt=10,
            cached=0,
        )
        assert observation is not None and observation.verdict == "cold"
        assert ledger.report()["calls"][0]["verdict"] == "cold"

    def test_a_ledger_handle_keeps_its_connection(self):
        """一条账本操作只认一条连接：单例被换掉（测试隔离 / 收尾）也不换连接。"""
        from wing.context.cache_store import CacheStore
        from wing.db import get_database, reset_database

        store = CacheStore("pinned")
        database = store.database()
        assert database is get_database()
        reset_database()
        try:
            assert store.database() is database, "中途换连接会把事务劈成两半"
        finally:
            reset_database()

    def test_concurrent_records_do_not_lose_rows(self):
        """多线程同刻记账（主调用 + 后台增量压缩在真并行）：一行都不丢。"""

        def record(index: int) -> None:
            CacheLedger("threads").record(
                parts("system:base", f"history:{index}:user"),
                purpose="main",
                model="m",
                provider="p",
                prompt=100 + index,
                cached=0,
            )

        threads = [
            threading.Thread(target=record, args=(index,)) for index in range(20)
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        assert len(recorded_calls("threads")) == 20


# ─────────────────────────────────────────────────────────────────────────────
# 12. 端点三种形状（有数据 / 空账本 200 / 会话不存在 200）
# ─────────────────────────────────────────────────────────────────────────────


class TestCacheEndpoint:
    @pytest.fixture
    def real_client(self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
        from wing.gateway.server import GatewayServer
        from wing.runtime import WingRuntime

        monkeypatch.setenv("WING_SESSIONS_PATH", str(tmp_path / "sessions"))
        config = MagicMock()
        with patch("wing.gateway.server.load_config") as mock_load_config:
            mock_load_config.return_value = config
            server = GatewayServer()
        server.runtime = WingRuntime()
        with TestClient(server._app) as tc:
            yield tc, tmp_path / "sessions"

    def _seed_ledger(self, session_id: str) -> None:
        """按生产写入路径造一份账本（两次调用：改 rules 文件 → 前缀破碎）。"""
        ledger = CacheLedger(session_id)
        rule = CacheSource(path="/rules/one.md", mtime="2026-10-18T15:00:00", size=12)
        first = [
            CachePart.text("system:base", "S"),
            CachePart.text("rules:/rules/one.md", "OLD RULE", rule),
            CachePart.text("system:wire", "wire"),
        ]
        ledger.record(
            first,
            purpose="main",
            model="deepseek-v3",
            provider="main",
            prompt=14000,
            cached=0,
        )
        second = [
            CachePart.text("system:base", "S"),
            CachePart.text("rules:/rules/one.md", "NEW RULE", rule),
            CachePart.text("system:wire", "wire"),
            CachePart.text("history:u1:user", "ask"),
        ]
        ledger.record(
            second,
            purpose="main",
            model="deepseek-v3",
            provider="main",
            prompt=15000,
            cached=12000,
        )

    def test_with_data(self, real_client):
        client, _ = real_client
        session_id = "cache-demo"
        assert (
            client.post(
                "/api/session/create", json={"session_id": session_id}
            ).status_code
            == 200
        )
        self._seed_ledger(session_id)

        response = client.get(f"/api/session/cache?session_id={session_id}")
        assert response.status_code == 200
        body = response.json()
        assert body["session_id"] == session_id
        assert body["summary"]["calls"] == 2
        assert body["summary"]["prompt_tokens"] == 29000
        assert body["summary"]["cached_tokens"] == 12000
        assert body["summary"]["expected_tokens"] == 0
        assert body["summary"]["theory_rate"] == 0.0
        assert body["summary"]["actual_rate"] == round(12000 / 29000, 6)
        assert [call["purpose"] for call in body["calls"]] == ["main", "main"]
        # 新→旧
        assert body["calls"][0]["prompt"] == 15000
        assert body["calls"][0]["verdict"] == "prefix_broken"
        assert len(body["forks"]) == 1
        fork = body["forks"][0]
        assert fork["label"] == "rules:/rules/one.md"
        assert fork["kind"] == "modify"
        assert fork["blame"] == "external"
        assert fork["avoidable"] is True
        assert fork["verdict"] == "prefix_broken"
        assert fork["detail"]["path"] == "/rules/one.md"
        assert fork["detail"]["mtime"] == "2026-10-18T15:00:00"
        assert fork["detail"]["sha_before"] is not None
        assert fork["detail"]["sha_before"] != fork["detail"]["sha_after"]
        assert fork["detail"]["ref_ts"] is not None

        limited = client.get(f"/api/session/cache?session_id={session_id}&limit=1")
        assert limited.status_code == 200
        assert len(limited.json()["calls"]) == 1
        assert len(limited.json()["forks"]) == 1

        # limit 只在 1..200 内受理
        assert (
            client.get(
                f"/api/session/cache?session_id={session_id}&limit=0"
            ).status_code
            == 422
        )
        assert (
            client.get(
                f"/api/session/cache?session_id={session_id}&limit=201"
            ).status_code
            == 422
        )

    def test_sessions_do_not_see_each_other(self, real_client):
        """同一个库里两个会话：各看各的（summary 覆盖的只是本会话的行）。"""
        client, _ = real_client
        for session_id in ("cache-a", "cache-b"):
            assert (
                client.post(
                    "/api/session/create", json={"session_id": session_id}
                ).status_code
                == 200
            )
        self._seed_ledger("cache-a")

        other = client.get("/api/session/cache?session_id=cache-b")
        assert other.status_code == 200
        assert other.json()["calls"] == []
        assert other.json()["summary"]["calls"] == 0
        assert (
            client.get("/api/session/cache?session_id=cache-a").json()["summary"][
                "calls"
            ]
            == 2
        )

    def test_odd_rows_do_not_break_the_endpoint(self, real_client):
        """对外口径不会遇到的怪行（未知 verdict / 缺时间戳）→ 仍是 200，坏行只影响自己。"""
        client, _ = real_client
        session_id = "cache-odd"
        assert (
            client.post(
                "/api/session/create", json={"session_id": session_id}
            ).status_code
            == 200
        )
        assert get_database().execute(
            "INSERT INTO cache_call (session_id, ts, ts_ms, fp, purpose, model, "
            "provider, prompt, cached, expected, ref_fp, verdict, segment_count) "
            "VALUES (?, ?, NULL, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?)",
            (
                session_id,
                "2026-10-18T15:41:02.123",
                "abc",
                "main",
                "m",
                "p",
                100,
                0,
                "future_verdict",
                0,
            ),
        )
        response = client.get(f"/api/session/cache?session_id={session_id}")
        assert response.status_code == 200, response.text
        body = response.json()
        assert [call["verdict"] for call in body["calls"]] == ["future_verdict"]
        assert body["summary"]["calls"] == 1
        assert body["summary"]["prompt_tokens"] == 100
        # 未知 verdict 不进 provider 欠账（合同只认那两档）
        assert body["summary"]["provider_debt_rate"] == 0.0

    def test_empty_ledger_is_200(self, real_client):
        client, _ = real_client
        session_id = "cache-empty"
        assert (
            client.post(
                "/api/session/create", json={"session_id": session_id}
            ).status_code
            == 200
        )
        response = client.get(f"/api/session/cache?session_id={session_id}")
        assert response.status_code == 200
        body = response.json()
        assert body["summary"] == {
            "calls": 0,
            "prompt_tokens": 0,
            "cached_tokens": 0,
            "expected_tokens": 0,
            "theory_rate": None,
            "actual_rate": None,
            "provider_debt_rate": None,
        }
        assert body["forks"] == []
        assert body["calls"] == []

    def test_missing_session_is_200_with_empty_payload(self, real_client):
        client, _ = real_client
        response = client.get("/api/session/cache?session_id=does-not-exist")
        assert response.status_code == 200
        body = response.json()
        assert body["session_id"] == "does-not-exist"
        assert body["calls"] == [] and body["forks"] == []
        assert body["summary"]["calls"] == 0
        assert body["summary"]["actual_rate"] is None
        # 非法 id 同样不炸（闸门 → 空报告，不 500）
        assert client.get("/api/session/cache?session_id=../etc").status_code == 200

    def test_archived_session_reports_empty(self, real_client):
        """归档会话按"不存在"处理（与 `read_session_aux` 同口径的冻结语义）。"""
        client, _ = real_client
        session_id = "cache-archived"
        assert (
            client.post(
                "/api/session/create",
                json={"session_id": session_id, "tags": ["probe"]},
            ).status_code
            == 200
        )
        self._seed_ledger(session_id)
        assert (
            client.get(f"/api/session/cache?session_id={session_id}").json()["summary"][
                "calls"
            ]
            == 2
        )
        archived = client.post(
            "/api/session/archive", json={"session_id": session_id}
        ).json()
        assert archived["archived"] is True, archived
        body = client.get(f"/api/session/cache?session_id={session_id}").json()
        assert body["calls"] == [] and body["summary"]["calls"] == 0

    def test_a_recreated_session_starts_from_an_empty_ledger(self, real_client):
        """同一个 id 被新会话接管（目录被手工删掉后重建）→ 账本从空开始。

        账本搬进进程级库之后，"会话目录没了账本就没了" 不再自动成立：不清一次的话，
        新会话的第一轮会拿旧化身的窗口当参照（判成 prefix_broken / hit）。
        """
        import shutil

        client, sessions_root = real_client
        session_id = "cache-reuse"
        assert (
            client.post(
                "/api/session/create", json={"session_id": session_id}
            ).status_code
            == 200
        )
        self._seed_ledger(session_id)
        assert (
            client.get(f"/api/session/cache?session_id={session_id}").json()["summary"][
                "calls"
            ]
            == 2
        )

        # 模拟"手工删掉会话目录"：目录没了、内存里也没了这个会话
        assert (
            client.post(
                "/api/session/release", json={"session_id": session_id}
            ).status_code
            == 200
        )
        shutil.rmtree(sessions_root / session_id)

        assert (
            client.post(
                "/api/session/create", json={"session_id": session_id}
            ).status_code
            == 200
        )
        body = client.get(f"/api/session/cache?session_id={session_id}").json()
        assert body["calls"] == [], f"新化身的账本必须从空开始: {body['calls']}"
        assert body["summary"]["calls"] == 0

    def test_archiving_drops_the_ledger_rows(self, real_client):
        """归档清掉账本行：归档会话没人读得到，留着只会让库随会话数线性长大。"""
        client, _ = real_client
        session_id = "cache-archive-clean"
        assert (
            client.post(
                "/api/session/create",
                json={"session_id": session_id, "tags": ["probe"]},
            ).status_code
            == 200
        )
        self._seed_ledger(session_id)
        assert len(recorded_calls(session_id)) == 2
        assert (
            client.post("/api/session/archive", json={"session_id": session_id}).json()[
                "archived"
            ]
            is True
        )
        assert recorded_calls(session_id) == []
        assert (
            db_rows("SELECT 1 FROM cache_segment WHERE session_id = ?", (session_id,))
            == []
        )

    def test_report_echoes_the_requested_id(self, real_client):
        """回显请求里的 id（别名归一不改对外读到的键）。"""
        client, _ = real_client
        session_id = "cache-echo"
        assert (
            client.post(
                "/api/session/create", json={"session_id": session_id}
            ).status_code
            == 200
        )
        self._seed_ledger(session_id)
        body = client.get(f"/api/session/cache?session_id={session_id}").json()
        assert body["session_id"] == session_id

    def test_read_path_does_not_run_on_the_event_loop(self, real_client):
        """读 + 投影不在循环线程上：否则一次查询会挡住所有会话的流式帧（S1）。"""
        client, _ = real_client
        session_id = "cache-off-loop"
        assert (
            client.post(
                "/api/session/create", json={"session_id": session_id}
            ).status_code
            == 200
        )

        server = client.app.state.server
        real = server.runtime.cache_report
        on_loop: list[bool] = []

        def spy(*args, **kwargs):
            try:
                asyncio.get_running_loop()
            except RuntimeError:
                on_loop.append(False)
            else:
                on_loop.append(True)
            return real(*args, **kwargs)

        with patch.object(server.runtime, "cache_report", new=spy):
            response = client.get(f"/api/session/cache?session_id={session_id}")
        assert response.status_code == 200
        assert on_loop == [False], "cache_report 在事件循环线程上跑了"


# ─────────────────────────────────────────────────────────────────────────────
# 实现级契约：直播字段同源 / compact 记账
# ─────────────────────────────────────────────────────────────────────────────


class _FakeProvider:
    """最小 provider 替身（只有 _call_llm 用到的那几个方法）。"""

    def __init__(self, chunks: list[LLMResponse]) -> None:
        self._chunks = chunks

    async def generate(self, **kwargs: Any) -> AsyncIterator[LLMResponse]:
        for chunk in self._chunks:
            yield chunk

    def create_accumulator(self) -> None:
        return None

    def snapshot_blocks(self, accumulator: Any) -> None:
        return None

    def unfinished_tool_calls(self, accumulator: Any) -> int:
        return 0


def _loop_for(cm: ContextManager) -> ReActLoop:
    return ReActLoop(
        tool_executor=None,  # ty: ignore[invalid-argument-type]
        sink=AgentEventSink(session_id=cm.id, append_event=cm.append_event),
        context_manager=cm,
        inbox=None,  # ty: ignore[invalid-argument-type]
        current_model=lambda: "m",
        current_provider=lambda: None,  # ty: ignore[invalid-argument-type]
        current_tools=lambda: [],
        current_options=lambda: RequestOptions(),
        stream=True,
    )


def _segments_of(session_id: str, fp: str) -> list[str]:
    return [
        row["label"]
        for row in db_rows(
            "SELECT label FROM cache_segment WHERE session_id = ? AND fp = ? "
            "ORDER BY idx",
            (session_id, fp),
        )
    ]


class TestLiveMetricsAndCompact:
    @pytest.mark.asyncio
    async def test_metrics_fields_come_from_the_same_ledger_record(
        self, tmp_path: Path
    ):
        events: list[Any] = []
        event_bus.subscribe(events.append)
        try:
            cm = make_cm(tmp_path, session_id="live")
            cm.add_message(USER)
            loop = _loop_for(cm)
            provider = _FakeProvider(
                [
                    LLMResponse(
                        content="hi",
                        usage=LLMUsage(
                            prompt_tokens=1000, completion_tokens=5, model="m"
                        ),
                    ),
                    LLMResponse(
                        content_blocks=[TextBlock(text="hi")], usage=LLMUsage(model="m")
                    ),
                ]
            )
            probe = CacheProbe()
            probe.set_segments(cm.cache_system_parts())  # 直调 _call_llm 不走 provider
            options = RequestOptions(
                cache_parts=tuple(cm.cache_system_parts()), cache_probe=probe
            )
            await loop._call_llm(
                provider,  # ty: ignore[invalid-argument-type]
                [SYSTEM, USER],
                "m",
                None,
                options,
            )

            metrics = [
                event for event in events if isinstance(event, LLMCallMetricsEvent)
            ]
            assert len(metrics) == 1
            assert metrics[0].cache_expected_tokens is None
            assert metrics[0].cache_verdict == "cold"
            assert metrics[0].cache_diverged == ""
            stored = recorded_calls("live")
            assert len(stored) == 1
            assert stored[0]["verdict"] == metrics[0].cache_verdict
            assert stored[0]["purpose"] == "main"
            # 分段确实含 system 侧与 wire 侧（探针开着）
            assert _segments_of("live", stored[0]["fp"]) == ["system:base"]
        finally:
            event_bus._subscribers.clear()
            event_bus._routing.clear()

    @pytest.mark.asyncio
    async def test_manual_compact_is_recorded_with_compact_purpose(
        self, tmp_path: Path
    ):
        class _CompactProvider:
            """模拟 provider 契约：构建请求时分段写进探针，然后回一个摘要。"""

            name = "main"

            @staticmethod
            async def generate(messages, model, tools=None, stream=False, options=None):
                probe = options.cache_probe if options is not None else None
                assert probe is not None
                probe.set_segments(
                    [
                        *(options.cache_parts if options is not None else ()),
                        CachePart.text("system:wire", "wire"),
                    ]
                )
                yield LLMResponse(
                    content="<summary>squashed</summary>",
                    usage=LLMUsage(prompt_tokens=4321, completion_tokens=123),
                )

        cm = make_cm(
            tmp_path,
            session_id="compact",
            compactor=Compactor(context_window_tokens=1000, keep_recent_tokens=100),
        )
        cm.add_messages(
            [
                Message(role="user", content="first question"),
                Message(role="assistant", content="first answer"),
            ]
        )
        await cm.do_manual_compact(
            model="m",
            model_provider=_CompactProvider(),  # ty: ignore[invalid-argument-type]
            current_tools=lambda: [],
            options=RequestOptions(cache_probe=CacheProbe()),
        )
        stored = recorded_calls("compact")
        assert [row["purpose"] for row in stored] == ["compact"]
        assert stored[0]["prompt"] == 4321
        assert stored[0]["verdict"] == "cold"


class TestProbeIsolation:
    @pytest.mark.asyncio
    async def test_background_compact_gets_its_own_probe(self, tmp_path: Path):
        """增量压缩是**另一次**调用：不得覆盖主调用的探针。

        生产常态路径：主调用的 `get_messages_for_llm` 触发后台压缩，压缩请求在
        同一个 event loop 上先构建 body 并写入探针（覆盖式）。共用探针会让
        `purpose="main"` 的账本行记成压缩请求的段列表，把后续所有归因污染。
        """

        class _SegmentingProvider:
            """模拟 provider 契约：构建请求时分段写进**自己的**探针。"""

            name = "main"

            @staticmethod
            async def generate(messages, model, tools=None, stream=False, options=None):
                assert options is not None and options.cache_probe is not None
                options.cache_probe.set_segments(
                    [
                        *options.cache_parts,
                        CachePart.text("history:COMPACT:user", "squash me"),
                    ]
                )
                yield LLMResponse(
                    content="<summary>squashed</summary>",
                    usage=LLMUsage(prompt_tokens=900, completion_tokens=10),
                )

        cm = make_cm(
            tmp_path,
            session_id="bg",
            compactor=Compactor(context_window_tokens=200, keep_recent_tokens=20),
        )
        cm.add_messages(
            [
                Message(role="user", content="x" * 400),
                Message(role="assistant", content="y" * 400),
                Message(role="user", content="z" * 400),
            ]
        )
        main_probe = CacheProbe()
        await cm.get_messages_for_llm(
            model="m",
            model_provider=_SegmentingProvider(),  # ty: ignore[invalid-argument-type]
            current_tools=lambda: [],
            options=RequestOptions(
                cache_parts=(CachePart.text("system:base", "S"),),
                cache_probe=main_probe,
            ),
        )
        assert cm._pending_compact_task is not None
        await cm._pending_compact_task

        # 主调用的探针没有被压缩请求覆盖（它此时还没把请求发出去）
        assert main_probe.segments == []
        stored = recorded_calls("bg")
        assert [row["purpose"] for row in stored] == ["compact"]
        assert _segments_of("bg", stored[0]["fp"]) == [
            "system:base",
            "history:COMPACT:user",
        ]

    @pytest.mark.asyncio
    async def test_validated_call_hands_parts_and_probe_to_the_provider(
        self, tmp_path: Path
    ):
        """接线：`_call_llm_validated` 把 CM 的分段 + 发送时刻的探针交给 provider。

        缺少这条接线时账本一行都不会记，而单元测试里直接调 `_call_llm` 看不出来。
        """
        seen: dict[str, Any] = {}

        class _CapturingProvider:
            name = "main"

            @staticmethod
            def create_accumulator():
                return None

            @staticmethod
            def snapshot_blocks(accumulator: Any) -> None:
                return None

            @staticmethod
            def unfinished_tool_calls(accumulator: Any) -> int:
                return 0

            @staticmethod
            async def generate(**kwargs: Any):
                options = kwargs["options"]
                seen["cache_parts"] = options.cache_parts
                seen["probe"] = options.cache_probe
                if options.cache_probe is not None:
                    options.cache_probe.set_segments(
                        [*options.cache_parts, CachePart.text("system:wire", "w")]
                    )
                yield LLMResponse(
                    content="ok",
                    usage=LLMUsage(prompt_tokens=100, completion_tokens=2, model="m"),
                )
                yield LLMResponse(
                    content_blocks=[TextBlock(text="ok")], usage=LLMUsage(model="m")
                )

        cm = make_cm(tmp_path, session_id="wired")
        cm.add_message(USER)
        probe = CacheProbe()
        loop = ReActLoop(
            tool_executor=None,  # ty: ignore[invalid-argument-type]
            sink=AgentEventSink(session_id=cm.id, append_event=cm.append_event),
            context_manager=cm,
            inbox=None,  # ty: ignore[invalid-argument-type]
            current_model=lambda: "m",
            current_provider=lambda: _CapturingProvider(),  # ty: ignore[invalid-argument-type]
            current_tools=lambda: [],
            current_options=lambda: RequestOptions(cache_probe=probe),
            stream=True,
        )
        await loop._call_llm_validated(_turn_ctx(), model="m")

        assert [part.label for part in seen["cache_parts"]] == ["system:base"]
        assert seen["probe"] is probe
        stored = recorded_calls("wired")
        assert [row["purpose"] for row in stored] == ["main"]
        assert stored[0]["prompt"] == 100


def _turn_ctx() -> Any:
    """`_call_llm_validated` 只用到 usage / text 累计。"""
    from wing.agent.react_loop import _TurnAccumulator

    return _TurnAccumulator()


# ─────────────────────────────────────────────────────────────────────────────
# 纯函数卫生：指纹 / 前缀指纹 / 空投影
# ─────────────────────────────────────────────────────────────────────────────


class TestFingerprintPrimitives:
    def test_fingerprint_is_order_sensitive_and_stable(self):
        forward = parts("a", "b")
        assert fingerprint(forward) == fingerprint(parts("a", "b"))
        assert fingerprint(forward) != fingerprint(parts("b", "a"))
        assert fingerprint([]) == fingerprint([])

    def test_prefix_fingerprint_is_the_lazy_shortcut_of_the_full_one(self):
        """「前 n 段的指纹」就是 `fingerprint(segments[:n])`，且全量那条与它同源。"""
        segments = parts("system:base", "tools:Bash", "history:u1:user")
        prefixes = _PrefixFingerprints(segments, fingerprint(segments))
        assert prefixes.size == 3
        for length in range(1, len(segments) + 1):
            assert prefixes.at(length) == fingerprint(segments[:length])
        # 全量那一条是构造时给进来的（不重复哈希）
        assert prefixes.at(3) is prefixes.at(3)

    def test_segment_hash_is_16_hex_and_protocol_order_independent(self):
        one = CachePart.obj("tools:X", {"a": 1, "b": 2})
        two = CachePart.obj("tools:X", {"b": 2, "a": 1})
        assert one.sha == two.sha
        assert len(one.sha) == 16
        int(one.sha, 16)

    def test_build_report_on_empty_ledger(self):
        report = build_report("s", [], limit=20, segment_at=lambda _fp, _index: None)
        assert report["summary"]["calls"] == 0
        assert report["summary"]["theory_rate"] is None
        assert report["forks"] == []
        assert report["calls"] == []
        assert empty_report("s")["session_id"] == "s"
        assert empty_report("s")["summary"]["actual_rate"] is None
