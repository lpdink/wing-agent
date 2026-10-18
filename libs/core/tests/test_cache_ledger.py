"""缓存账本（探针 + 账本 + 归因 + 端点）测试。

覆盖步骤任务书 `01_backend` 的 12 条必须测试，外加三条实现级契约：直播字段与
账本记录同源（contract §5）、压缩调用同样进账本（§8.8）、`src` 元信息在加载期
采集（热路径不扫目录）。口径唯一来源是 `../contract.md`——任何断言都不该比它更宽。
"""

from __future__ import annotations

import asyncio
import json
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
    CACHE_LEDGER_KEY,
    MAX_CALLS,
    MAX_LEDGER_BYTES,
    CacheLedger,
    build_report,
    empty_ledger,
    normalize_ledger,
    record_call,
)
from wing.event import LLMCallMetricsEvent
from wing.event_bus import event_bus
from wing.provider.anthropic.provider import AnthropicProvider
from wing.provider.base import ModelProvider, RequestOptions
from wing.provider.openai.provider import OpenAICompatProvider
from wing.schema import (
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
from wing.store.file import FileMessageLog, FileSessionStore
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


def ledger_path(tmp_path: Path, session_id: str) -> Path:
    return tmp_path / session_id / "cache_ledger.json"


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


def parts(*labels: str) -> list[CachePart]:
    """一串固定文本段（label → 内容一一对应，内容与 label 绑定）。"""
    return [CachePart.text(label, f"content of {label}") for label in labels]


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
    def test_first_call_is_cold_without_fork(self, tmp_path: Path):
        ledger = CacheLedger(FileMessageLog(tmp_path / "s1"), "s1")
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

    def test_second_call_with_same_prefix_and_full_cache_is_hit(self, tmp_path: Path):
        ledger = CacheLedger(FileMessageLog(tmp_path / "s2"), "s2")
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
        ledger = CacheLedger(FileMessageLog(tmp_path / "s3"), "s3")

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
        # 前缀彻底断裂 ⇒ 没有参照 ⇒ tokens_lost 为 null（contract §4，展示为 `—`）
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
        ledger = CacheLedger(FileMessageLog(tmp_path / "s4"), "s4")

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

    def test_tool_declaration_change_blames_tools(self, tmp_path: Path):
        ledger = CacheLedger(FileMessageLog(tmp_path / "s5"), "s5")
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

    def test_zero_cached_with_identical_prefix_is_provider_underdelivered(
        self, tmp_path: Path
    ):
        ledger = CacheLedger(FileMessageLog(tmp_path / "s6"), "s6")
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
        # provider 欠账进汇总（forks 只收有分叉的调用——contract §6）
        assert report["summary"]["provider_debt_rate"] == round(10000 / 22000, 6)
        assert report["forks"] == []

    @pytest.mark.parametrize(
        ("cached", "expected_verdict"),
        [
            (10000 - 256, "block_granularity"),  # diff == tol
            (10000 - 257, "provider_underdelivered"),  # diff == tol + 1
        ],
    )
    def test_tolerance_boundary(
        self, tmp_path: Path, cached: int, expected_verdict: str
    ):
        """tol = max(256, ceil(0.02 * ref.prompt))；边界两侧各一条。"""
        ledger = CacheLedger(FileMessageLog(tmp_path / f"s7-{cached}"), "s7")
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

    def test_twin_call_uses_identical_segments_as_reference(self, tmp_path: Path):
        """段列表完全相同（重试 / 重放）也有参照——契约的严格前缀格子之外的补齐。"""
        ledger = CacheLedger(FileMessageLog(tmp_path / "s8"), "s8")
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

    def test_expired_reference_is_cold_expired(self, tmp_path: Path):
        """参照调用超过 TTL（72h）→ cold_expired，expected 归 null。"""
        ledger = CacheLedger(FileMessageLog(tmp_path / "s9"), "s9")
        base = parts("system:base", "tools:Bash")
        old = datetime.now() - timedelta(hours=100)
        data, _ = record_call(
            empty_ledger("s9"),
            base,
            purpose="main",
            model="m",
            provider="p",
            prompt=1000,
            cached=0,
            now=old,
        )
        (tmp_path / "s9").mkdir(parents=True, exist_ok=True)
        (tmp_path / "s9" / f"{CACHE_LEDGER_KEY}.json").write_text(
            json.dumps(data), encoding="utf-8"
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

    def test_compact_attribution_for_history_forks(self, tmp_path: Path):
        """history 分叉 + 参照与本次之间存在 compact 调用 → blame=compact（不可避免）。

        构造：c1 是本次的严格前缀（参照），c2 是夹在中间的压缩调用（最近一条，
        因此分叉点按 c2 算），c3 回落到 c1 分支继续跑 —— 这正是"压缩后换分支"的
        形状：参照存在、但上一次调用的前缀没能延续。
        """
        ledger = CacheLedger(FileMessageLog(tmp_path / "s10"), "s10")
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
        log = FileMessageLog(tmp_path / "s-revert")

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

        data = empty_ledger("s-revert")
        for index, (parts_, prompt, cached) in enumerate(
            [
                (first, 1000, 0),
                (second, 1500, 1000),
                (changed, 2000, 1500),
                (reverted, 2500, 2000),
            ]
        ):
            data, _ = record_call(
                data,
                parts_,
                purpose="main",
                model="m",
                provider="p",
                prompt=prompt,
                cached=cached,
                now=datetime(2026, 10, 18, 10, index),
            )
        log.write_aux(CACHE_LEDGER_KEY, data)
        fork = CacheLedger(log, "s-revert").report()["forks"][0]

        assert fork["label"] == rule_label
        assert fork["kind"] == "modify"
        calls = data["calls"]
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
        assert calls[-1]["ref_fp"] == calls[1]["fp"]
        assert fork["detail"]["ref_ts"] == calls[2]["ts"]
        assert calls[2]["ts"] != calls[1]["ts"]

    def test_reference_left_the_ledger_but_the_compared_call_is_still_named(self):
        """参照已滚出账本、对照侧（邻座）还在 → 时刻跟着对照侧，不指向无关的调用。"""
        base = {"label": "system:base", "sha": "aaaa0000aaaa0000", "chars": 10}
        old_rule = {"label": "rules:/r.md", "sha": "cc", "chars": 2}
        new_rule = {"label": "rules:/r.md", "sha": "bb", "chars": 2}
        fork = build_report(
            normalize_ledger(
                {
                    "version": 1,
                    "session_id": "gone",
                    "fingerprints": {
                        "new": {"segments": [base, new_rule]},
                        "old": {"segments": [base, old_rule]},
                    },
                    "calls": [
                        {
                            "ts": "2026-10-18T10:00:00.000",
                            "fp": "old",
                            "purpose": "main",
                            "model": "m",
                            "provider": "p",
                            "prompt": 900,
                            "cached": 0,
                            "expected": None,
                            "ref_fp": None,
                            "verdict": "cold",
                            "fork": None,
                        },
                        {
                            "ts": "2026-10-18T10:05:00.000",
                            "fp": "new",
                            "purpose": "main",
                            "model": "m",
                            "provider": "p",
                            "prompt": 1000,
                            "cached": 0,
                            "expected": None,
                            "ref_fp": "gone",
                            "verdict": "prefix_broken",
                            "fork": {
                                "kind": "modify",
                                "index": 1,
                                "label": "rules:/r.md",
                                "segments_affected": 1,
                                "tokens_lost": None,
                            },
                        },
                    ],
                },
                "gone",
            ),
            session_id="gone",
            limit=20,
        )["forks"][0]
        assert fork["detail"]["sha_before"] == "cc"
        assert fork["detail"]["sha_before"] != fork["detail"]["sha_after"]
        # 对照侧是邻座那条（sha 与 ts 同源），而不是记在案却已不可定位的参照
        assert fork["detail"]["ref_ts"] == "2026-10-18T10:00:00.000"

    def test_comparison_side_gone_leaves_both_the_sha_and_the_ts_empty(self):
        """对照侧整个滚出账本（分叉行成了最旧一条）→ `sha_before` 与 `ref_ts` 一起缺省。

        `--why` 因此整行不显示 `ref`：不拿「紧邻的后来者」的 ts 冒充那个已经不可见的时刻。
        """
        base = {"label": "system:base", "sha": "aaaa0000aaaa0000", "chars": 10}
        new_rule = {"label": "rules:/r.md", "sha": "bb", "chars": 2}
        fork = build_report(
            normalize_ledger(
                {
                    "version": 1,
                    "session_id": "gone",
                    "fingerprints": {"new": {"segments": [base, new_rule]}},
                    "calls": [
                        {
                            "ts": "2026-10-18T10:05:00.000",
                            "fp": "new",
                            "purpose": "main",
                            "model": "m",
                            "provider": "p",
                            "prompt": 1000,
                            "cached": 0,
                            "expected": None,
                            "ref_fp": "gone",
                            "verdict": "prefix_broken",
                            "fork": {
                                "kind": "modify",
                                "index": 1,
                                "label": "rules:/r.md",
                                "segments_affected": 1,
                                "tokens_lost": None,
                            },
                        }
                    ],
                },
                "gone",
            ),
            session_id="gone",
            limit=20,
        )["forks"][0]
        assert fork["detail"]["sha_before"] is None
        assert fork["detail"]["ref_ts"] is None
        assert fork["detail"]["sha_after"] == "bb"


# ─────────────────────────────────────────────────────────────────────────────
# 9. 环形上界 + fingerprint GC
# ─────────────────────────────────────────────────────────────────────────────


class TestRingBound:
    def test_calls_are_capped_and_fingerprints_are_garbage_collected(
        self, tmp_path: Path
    ):
        store = FileSessionStore(tmp_path)
        ledger = CacheLedger(store.open_log("ring"), "ring")
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
        data = json.loads(ledger_path(tmp_path, "ring").read_text(encoding="utf-8"))
        assert data["version"] == 1
        assert len(data["calls"]) == MAX_CALLS == 200
        alive = {call["fp"] for call in data["calls"]}
        assert set(data["fingerprints"]) == alive  # 无孤儿指纹
        # 保留的是最近 200 条（最旧的 50 条被挤出）
        assert data["calls"][0]["prompt"] == 100 + 50
        assert data["calls"][-1]["prompt"] == 100 + 249

    def test_over_budget_ledger_is_trimmed_and_keeps_the_newest_call(
        self, tmp_path: Path
    ):
        """字节预算（cloud S3）：超了按「最旧调用先淘汰」裁到预算内，最近一条必留。"""
        ledger = CacheLedger(FileSessionStore(tmp_path).open_log("budget"), "budget")
        path = ledger_path(tmp_path, "budget")
        path.parent.mkdir(parents=True, exist_ok=True)
        fat_segments = [
            {
                "label": f"history:seg-{index:03d}:user",
                "sha": f"{index:016x}",
                "chars": 900,
            }
            for index in range(200)
        ]
        seeded = empty_ledger("budget")
        for index in range(MAX_CALLS):
            fp = f"fp{index:04d}"
            seeded["fingerprints"][fp] = {"segments": fat_segments}
            seeded["calls"].append(
                {
                    "ts": f"2026-10-18T10:{index // 60:02d}:{index % 60:02d}.000",
                    "fp": fp,
                    "purpose": "main",
                    "model": "m",
                    "provider": "p",
                    "prompt": 1000 + index,
                    "cached": 0,
                    "expected": None,
                    "ref_fp": None,
                    "verdict": "cold",
                    "fork": None,
                }
            )
        path.write_text(json.dumps(seeded, ensure_ascii=False), encoding="utf-8")
        assert path.stat().st_size > MAX_LEDGER_BYTES  # 前置：确实超预算

        newest = parts("system:base")
        observation = ledger.record(
            newest, purpose="main", model="m", provider="p", prompt=42, cached=0
        )
        assert observation is not None

        raw = path.read_text(encoding="utf-8")
        assert len(raw.encode("utf-8")) <= MAX_LEDGER_BYTES
        stored = json.loads(raw)
        # 最近一条（刚记的这条）仍在
        assert stored["calls"][-1]["prompt"] == 42
        assert stored["calls"][-1]["fp"] == fingerprint(newest)
        # 淘汰的是**最旧**的：存活的种子调用是原序列的后缀（prompt 连续递增且首条已晚于第 0 条）
        prompts = [call["prompt"] for call in stored["calls"]][:-1]
        assert prompts, "预算只该淘汰旧的，不该一次砍光"
        assert prompts == list(range(prompts[0], prompts[0] + len(prompts)))
        assert prompts[0] > 1000
        # 旧的被淘汰，且没有留下孤儿段落表
        assert len(stored["calls"]) < MAX_CALLS
        assert set(stored["fingerprints"]) == {call["fp"] for call in stored["calls"]}

    def test_bool_is_not_accepted_as_a_count(self):
        """`bool` 是 `int` 的子类：手改账本写成 `true` 不能当 1 收下（cloud N2）。"""
        row = {
            "ts": "2026-10-18T15:41:02.123",
            "fp": "f",
            "purpose": "main",
            "model": "m",
            "provider": "p",
            "prompt": 100,
            "cached": 0,
            "expected": None,
            "ref_fp": None,
            "verdict": "cold",
            "fork": None,
        }
        calls = normalize_ledger(
            {
                "version": 1,
                "session_id": "b",
                "fingerprints": {},
                "calls": [
                    {**row, "prompt": True},  # prompt 是 bool → 整行丢弃
                    {**row, "cached": False},  # cached 同理
                    {
                        **row,
                        "expected": True,
                        "fork": {
                            "kind": "modify",
                            "label": "rules:/r.md",
                            "index": True,
                            "segments_affected": True,
                            "tokens_lost": True,
                        },
                    },
                ],
            },
            "b",
        )["calls"]
        assert len(calls) == 1
        assert calls[0]["expected"] is None
        assert calls[0]["fork"] == {
            "kind": "modify",
            "index": 0,
            "label": "rules:/r.md",
            "segments_affected": 0,
            "tokens_lost": None,
        }
        # 合法值照旧
        assert (
            normalize_ledger(
                {"version": 1, "session_id": "b", "fingerprints": {}, "calls": [row]},
                "b",
            )["calls"][0]["prompt"]
            == 100
        )

    def test_corrupted_ledger_is_treated_as_empty(self, tmp_path: Path):
        store = FileSessionStore(tmp_path)
        log_ = store.open_log("broken")
        (tmp_path / "broken").mkdir(parents=True, exist_ok=True)
        (tmp_path / "broken" / f"{CACHE_LEDGER_KEY}.json").write_text(
            '{"version": 99, "calls": "nonsense"}', encoding="utf-8"
        )
        ledger = CacheLedger(log_, "broken")
        assert ledger.load()["calls"] == []
        assert normalize_ledger(None, "broken")["session_id"] == "broken"
        observation = ledger.record(
            parts("system:base"),
            purpose="main",
            model="m",
            provider="p",
            prompt=10,
            cached=0,
        )
        assert observation is not None and observation.verdict == "cold"


# ─────────────────────────────────────────────────────────────────────────────
# 10. best-effort：写盘失败只 WARN，绝不影响 LLM 调用
# ─────────────────────────────────────────────────────────────────────────────


class _ExplodingLog(FileMessageLog):
    """写盘永远失败的 MessageLog（模拟 ENOSPC / 只读挂载）。"""

    def write_aux(self, key: str, data: dict[str, Any]) -> None:
        raise OSError("disk full")


class TestBestEffort:
    def test_write_failure_returns_none_and_warns(
        self, tmp_path: Path, wing_warnings: list[str]
    ):
        log_ = _ExplodingLog(tmp_path / "x")
        ledger = CacheLedger(log_, "x")
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
        assert any("cache ledger record failed" in message for message in wing_warnings)

    def test_read_failure_is_treated_as_empty(
        self, tmp_path: Path, wing_warnings: list[str]
    ):
        class _UnreadableLog(FileMessageLog):
            def read_aux(self, key: str) -> dict[str, Any] | None:
                raise OSError("io error")

        ledger = CacheLedger(_UnreadableLog(tmp_path / "y"), "y")
        assert ledger.load()["calls"] == []
        assert ledger.report()["calls"] == []
        assert any("cache ledger unreadable" in message for message in wing_warnings)

    @pytest.mark.asyncio
    async def test_llm_call_survives_ledger_failure(
        self, tmp_path: Path, wing_warnings: list[str]
    ):
        """整条链：账本写盘炸了，模型调用照常返回、metrics 照常发射。"""
        events: list[Any] = []
        event_bus.subscribe(events.append)
        try:
            cm = make_cm(tmp_path, session_id="boom")
            cm._messages = TrackedList(_ExplodingLog(tmp_path / "boom"))
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
            assert any(
                "cache ledger record failed" in message for message in wing_warnings
            )
        finally:
            event_bus._subscribers.clear()
            event_bus._routing.clear()


# ─────────────────────────────────────────────────────────────────────────────
# 11. memory 后端：内存态可用、不抛、不产生文件
# ─────────────────────────────────────────────────────────────────────────────


class TestMemoryBackend:
    def test_memory_store_keeps_state_in_process(self, tmp_path: Path):
        store = MemorySessionStore()
        log_ = store.open_log("mem")
        ledger = CacheLedger(log_, "mem")
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
        assert log_.read_aux(CACHE_LEDGER_KEY) is not None
        assert list(tmp_path.rglob("*")) == []  # 零存储痕迹

    def test_logless_chain_works_without_persistence(self):
        """`log=None` 的纯内存链：记账不抛、状态不落盘。"""
        ledger = CacheLedger(TrackedList(), "pure")
        observation = ledger.record(
            parts("system:base"),
            purpose="main",
            model="m",
            provider="p",
            prompt=10,
            cached=0,
        )
        assert observation is not None and observation.verdict == "cold"
        assert ledger.report()["calls"] == []


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

    def _seed_ledger(self, sessions_root: Path, session_id: str) -> None:
        """按生产写入路径造一份账本（`record_call` + 落盘格式）。"""
        directory = sessions_root / session_id
        directory.mkdir(parents=True, exist_ok=True)
        data = empty_ledger(session_id)
        rule = CacheSource(path="/rules/one.md", mtime="2026-10-18T15:00:00", size=12)
        first = [
            CachePart.text("system:base", "S"),
            CachePart.text("rules:/rules/one.md", "OLD RULE", rule),
            CachePart.text("system:wire", "wire"),
        ]
        data, _ = record_call(
            data,
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
        data, _ = record_call(
            data,
            second,
            purpose="main",
            model="deepseek-v3",
            provider="main",
            prompt=15000,
            cached=12000,
        )
        (directory / f"{CACHE_LEDGER_KEY}.json").write_text(
            json.dumps(data), encoding="utf-8"
        )

    def test_with_data(self, real_client):
        client, sessions_root = real_client
        session_id = "cache-demo"
        assert (
            client.post(
                "/api/session/create", json={"session_id": session_id}
            ).status_code
            == 200
        )
        self._seed_ledger(sessions_root, session_id)

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

    def test_corrupted_ledger_rows_do_not_break_the_endpoint(self, real_client):
        """合法 JSON 但字段类型错位的账本 → 仍是 200（坏行丢弃，不 500）。"""
        client, sessions_root = real_client
        session_id = "cache-bad"
        assert (
            client.post(
                "/api/session/create", json={"session_id": session_id}
            ).status_code
            == 200
        )
        directory = sessions_root / session_id
        (directory / f"{CACHE_LEDGER_KEY}.json").write_text(
            json.dumps(
                {
                    "version": 1,
                    "session_id": session_id,
                    "fingerprints": {"abc": {"segments": "nonsense"}},
                    "calls": [
                        {"ts": 123, "fp": "abc"},  # 类型错位的行 → 丢弃
                        "not-a-dict",
                        {
                            "ts": "2026-10-18T15:41:02.123",
                            "fp": "abc",
                            "purpose": "main",
                            "model": "m",
                            "provider": "p",
                            "prompt": 100,
                            "cached": 0,
                            "expected": None,
                            "ref_fp": None,
                            "verdict": "cold",
                            "fork": None,
                        },
                    ],
                }
            ),
            encoding="utf-8",
        )
        response = client.get(f"/api/session/cache?session_id={session_id}")
        assert response.status_code == 200, response.text
        body = response.json()
        assert [call["verdict"] for call in body["calls"]] == ["cold"]
        assert body["summary"]["calls"] == 1
        assert body["summary"]["prompt_tokens"] == 100

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

    def test_read_path_does_not_run_on_the_event_loop(self, real_client):
        """读 + 规整 + 投影不在循环线程上：否则一次查询会挡住所有会话的流式帧（S1）。"""
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
            data = json.loads(
                (tmp_path / "live" / f"{CACHE_LEDGER_KEY}.json").read_text(
                    encoding="utf-8"
                )
            )
            assert len(data["calls"]) == 1
            assert data["calls"][0]["verdict"] == metrics[0].cache_verdict
            assert data["calls"][0]["purpose"] == "main"
            # 分段确实含 system 侧与 wire 侧（探针开着）
            fingerprint_of_call = data["calls"][0]["fp"]
            labels = [
                segment["label"]
                for segment in data["fingerprints"][fingerprint_of_call]["segments"]
            ]
            assert labels[0] == "system:base"
            assert labels == ["system:base"]  # 直调 _call_llm 不经 provider 序列化
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
        data = json.loads(
            (tmp_path / "compact" / f"{CACHE_LEDGER_KEY}.json").read_text(
                encoding="utf-8"
            )
        )
        assert [call["purpose"] for call in data["calls"]] == ["compact"]
        assert data["calls"][0]["prompt"] == 4321
        assert data["calls"][0]["verdict"] == "cold"


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
        data = json.loads(
            (tmp_path / "bg" / f"{CACHE_LEDGER_KEY}.json").read_text(encoding="utf-8")
        )
        assert [call["purpose"] for call in data["calls"]] == ["compact"]
        labels = [
            segment["label"]
            for segment in data["fingerprints"][data["calls"][0]["fp"]]["segments"]
        ]
        assert labels == ["system:base", "history:COMPACT:user"]

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
        data = json.loads(
            (tmp_path / "wired" / f"{CACHE_LEDGER_KEY}.json").read_text(
                encoding="utf-8"
            )
        )
        assert [call["purpose"] for call in data["calls"]] == ["main"]
        assert data["calls"][0]["prompt"] == 100


def _turn_ctx() -> Any:
    """`_call_llm_validated` 只用到 usage / text 累计。"""
    from wing.agent.react_loop import _TurnAccumulator

    return _TurnAccumulator()


# ─────────────────────────────────────────────────────────────────────────────
# 纯函数卫生：指纹与规范形
# ─────────────────────────────────────────────────────────────────────────────


class TestFingerprintPrimitives:
    def test_fingerprint_is_order_sensitive_and_stable(self):
        forward = parts("a", "b")
        assert fingerprint(forward) == fingerprint(parts("a", "b"))
        assert fingerprint(forward) != fingerprint(parts("b", "a"))

    def test_segment_hash_is_16_hex_and_protocol_order_independent(self):
        one = CachePart.obj("tools:X", {"a": 1, "b": 2})
        two = CachePart.obj("tools:X", {"b": 2, "a": 1})
        assert one.sha == two.sha
        assert len(one.sha) == 16
        int(one.sha, 16)

    def test_build_report_on_empty_ledger(self):
        report = build_report(empty_ledger("s"), session_id="s", limit=20)
        assert report["summary"]["calls"] == 0
        assert report["summary"]["theory_rate"] is None
        assert report["forks"] == []
        assert report["calls"] == []
