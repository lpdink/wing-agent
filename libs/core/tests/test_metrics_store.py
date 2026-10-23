"""tests/test_metrics_store.py — 用量聚合：度量口径、各维度、树递归、目录回填。

播种一律走**生产写入路径**：审计行经 ``_support.seed_*``（内部走
``AuditRecorder.on_event``，事件带显式 ``created_at``——时间维度是这些用例的主角），
目录行经 ``metrics.catalog.record_session_created``。测试不手写 INSERT 造数：那样
测的是 SQLite，不是采集链路。

窗口一律相对「今天」取（目录行的创建时刻是 ``datetime.now()``，只有落在窗口里
才谈得上 ``sessions_created``）。
"""

from __future__ import annotations

from dataclasses import fields
from datetime import date, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest

from wing.config import AgentConfig, Config, ProviderConfig
from wing.config.models import ModelSpec
from wing.db import Database, get_database
from wing.metrics import (
    MEASURE_FIELDS,
    Measures,
    MetricsStore,
    Row,
    Window,
    backfill_catalog,
    record_session_created,
    sum_measures,
    usage_report,
)
from wing.store import SessionMetadata, SessionStore
from wing.store.file import FileSessionStore

from _support import local_time, seed_llm_call, seed_tool_call, seed_user_message

#: 相对今天的三个日子（目录行创建于「现在」，窗口必须覆盖今天）。
TODAY = date.today()
YESTERDAY = TODAY - timedelta(days=1)
TWO_DAYS_AGO = TODAY - timedelta(days=2)

#: 覆盖 [TODAY-6, TODAY] 的缺省用窗（7 个稠密 day 行）。
SINCE = (TODAY - timedelta(days=6)).isoformat()
UNTIL = TODAY.isoformat()

#: 只写 metadata 的测试会话靠这个标签进 `list_summaries`（无 history 也无标签的目录不算会话）。
LISTED = ["probe"]


def _window(**kwargs: Any) -> Window:
    """缺省窗口 = 最近 7 天（相对今天）；关键字可覆盖。"""
    params: dict[str, Any] = {"since": date.fromisoformat(SINCE), "until": TODAY}
    params.update(kwargs)
    return Window(**params)


def _report(**kwargs: Any) -> dict:
    params: dict[str, Any] = {"since": SINCE, "until": UNTIL}
    params.update(kwargs)
    return usage_report(**params)


def _row_by_key(report: dict, key: str) -> dict:
    for row in report["rows"]:
        if row["key"] == key:
            return row
    raise AssertionError(
        f"row {key!r} missing from {[r['key'] for r in report['rows']]}"
    )


# ============================================================
# 值类型
# ============================================================


class TestMeasures:
    def test_wire_carries_all_measure_fields(self) -> None:
        assert tuple(Measures().wire()) == MEASURE_FIELDS
        assert len(MEASURE_FIELDS) == 11
        # 全零填充：默认值一个都不省略
        assert set(Measures().wire().values()) == {0}

    def test_sum_is_measure_wise(self) -> None:
        left = Measures(llm_calls=2, prompt_tokens=100)
        right = Measures(llm_calls=3, tool_calls=1, prompt_tokens=50)
        assert left + right == Measures(llm_calls=5, tool_calls=1, prompt_tokens=150)
        assert sum_measures([]) == Measures()
        assert sum_measures([left, right]) == left + right

    def test_field_order_matches_the_vocabulary(self) -> None:
        """``astuple()`` 按词表取值、``Measures(*…)`` 按字段声明序构造——两者必须同序。"""
        assert tuple(f.name for f in fields(Measures)) == MEASURE_FIELDS

    def test_row_wire_is_flat(self) -> None:
        row = Row(
            key="k",
            label="L",
            parent_session_id="p",
            measures=Measures(user_messages=1),
        )
        wire = row.wire()
        assert wire["key"] == "k"
        assert wire["label"] == "L"
        assert wire["parent_session_id"] == "p"
        assert wire["user_messages"] == 1
        assert set(wire) == set(MEASURE_FIELDS) | {"key", "label", "parent_session_id"}

    def test_window_days_are_dense_and_closed(self) -> None:
        window = Window(since=date(2026, 3, 1), until=date(2026, 3, 3))
        assert window.days() == ["2026-03-01", "2026-03-02", "2026-03-03"]
        assert window.wire() == {"since": "2026-03-01", "until": "2026-03-03"}
        # 单日窗口仍是一行
        assert Window(since=date(2026, 3, 1), until=date(2026, 3, 1)).days() == [
            "2026-03-01"
        ]


# ============================================================
# 按天
# ============================================================


class TestDayDimension:
    def test_dense_rows_and_bucket_totals(self) -> None:
        """稠密：窗口内每天都有一行；同一个桶里的三张表分别入账，sessions 跨表去重。"""
        seed_llm_call("s1", local_time(YESTERDAY, 9))
        seed_llm_call(
            "s1", local_time(YESTERDAY, 10), prompt=50, completion=5, cached=10
        )
        seed_tool_call("s1", local_time(YESTERDAY, 11))
        seed_user_message("s1", local_time(YESTERDAY, 11))

        report = _report()

        assert [row["key"] for row in report["rows"]] == _window().days()
        yesterday = _row_by_key(report, YESTERDAY.isoformat())
        assert yesterday["llm_calls"] == 2
        assert yesterday["tool_calls"] == 1
        assert yesterday["user_messages"] == 1
        assert yesterday["prompt_tokens"] == 150
        assert yesterday["completion_tokens"] == 15
        assert yesterday["cached_tokens"] == 50
        assert yesterday["llm_time_ms"] == 1000
        assert yesterday["tool_time_ms"] == 30
        # 同一个会话既调模型又用工具：sessions 只算一次（跨表去重）
        assert yesterday["sessions"] == 1
        assert yesterday["tool_errors"] == 0

        # 空天全 0（不是缺行）
        empty = _row_by_key(report, TODAY.isoformat())
        assert all(empty[name] == 0 for name in MEASURE_FIELDS)

    def test_rows_sum_to_totals_except_sessions(self) -> None:
        """可加度量行和 == totals（TOTAL 行可对账的前提）。

        ``sessions`` 是去重计数：同一个会话在两天都有活动时，两天各算 1、合计仍算 1
        ——它天生不可加，是 day 维度唯一的例外。
        """
        seed_llm_call("s1", local_time(YESTERDAY), prompt=10)
        seed_llm_call("s2", local_time(TWO_DAYS_AGO), prompt=20)
        seed_tool_call("s2", local_time(TWO_DAYS_AGO))
        seed_user_message("s1", local_time(TODAY))

        report = _report()

        for name in MEASURE_FIELDS:
            if name == "sessions":
                continue
            assert sum(row[name] for row in report["rows"]) == report["totals"][name], (
                name
            )
        # 去重计数：两天各 1，合计仍是 2 个不同会话
        assert sum(row["sessions"] for row in report["rows"]) == 3
        assert report["totals"]["sessions"] == 2

    def test_out_of_window_rows_are_excluded(self) -> None:
        seed_llm_call("s1", local_time(TODAY - timedelta(days=10)))
        seed_llm_call("s1", local_time(TODAY))

        report = _report()

        assert report["totals"]["llm_calls"] == 1
        assert len(report["rows"]) == 7

    def test_sessions_created_lands_on_creation_day(self) -> None:
        record_session_created("cat-s1", workspace="/w")

        report = _report()

        assert _row_by_key(report, TODAY.isoformat())["sessions_created"] == 1
        assert report["totals"]["sessions_created"] == 1
        # 没有审计事实的会话不算「活跃会话」
        assert report["totals"]["sessions"] == 0


# ============================================================
# 归属（我 / 我的 AI）
# ============================================================


class TestOriginDimension:
    def test_two_rows_fixed_order_and_attribution(self) -> None:
        """根会话算 me、有父的算 agents；顺序固定（me 在前）。"""
        record_session_created("root-1", workspace="/w")
        record_session_created("child-1", parent_session_id="root-1", workspace="/w")
        seed_llm_call("root-1", local_time(YESTERDAY), prompt=10)
        seed_tool_call("root-1", local_time(YESTERDAY))
        seed_llm_call("child-1", local_time(YESTERDAY), prompt=90)
        seed_user_message("child-1", local_time(YESTERDAY))

        report = _report(group_by="origin")

        assert [row["key"] for row in report["rows"]] == ["me", "agents"]
        me, agents = report["rows"]
        assert me["llm_calls"] == 1
        assert me["tool_calls"] == 1
        assert me["prompt_tokens"] == 10
        assert me["sessions"] == 1
        assert me["sessions_created"] == 1
        assert agents["llm_calls"] == 1
        assert agents["user_messages"] == 1
        assert agents["prompt_tokens"] == 90
        assert agents["sessions_created"] == 1

    def test_unknown_session_counts_as_me(self) -> None:
        """目录里没有的会话（旧数据 / 手工清理）按「我」计——不知道就当是自己开的。"""
        seed_llm_call("ghost", local_time(YESTERDAY), prompt=7)

        report = _report(group_by="origin")

        me, agents = report["rows"]
        assert me["llm_calls"] == 1 and me["sessions"] == 1
        assert agents["llm_calls"] == 0 and agents["sessions"] == 0

    def test_rows_sum_to_totals(self) -> None:
        record_session_created("root-1")
        record_session_created("child-1", parent_session_id="root-1")
        seed_llm_call("root-1", local_time(YESTERDAY), prompt=10)
        seed_llm_call("child-1", local_time(YESTERDAY), prompt=20)

        report = _report(group_by="origin")

        for name in MEASURE_FIELDS:
            assert sum(row[name] for row in report["rows"]) == report["totals"][name], (
                name
            )


# ============================================================
# 模型 / 工具
# ============================================================


class TestModelDimension:
    def test_same_name_across_providers_stays_separate(self) -> None:
        """key 是 ``provider/model`` 复合键：同名跨 provider 不合并。"""
        seed_llm_call(
            "s1", local_time(YESTERDAY), model="flash", provider="alpha", prompt=10
        )
        seed_llm_call(
            "s1", local_time(YESTERDAY), model="flash", provider="beta", prompt=20
        )

        report = _report(group_by="model")

        assert [row["key"] for row in report["rows"]] == [
            "beta/flash",
            "alpha/flash",
        ]
        assert [row["prompt_tokens"] for row in report["rows"]] == [20, 10]
        # 两个 provider 各自一个会话桶
        assert all(row["sessions"] == 1 for row in report["rows"])

    def test_label_is_display_name_from_config(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """``label`` = 配置里的展示名；未声明 / 无展示名 → None（回落调用名）。"""
        config = Config(
            providers=[
                ProviderConfig(
                    name="alpha",
                    base_url="https://example.invalid",
                    api_key="k",
                    models=[ModelSpec(name="flash", display_name="Alpha Flash")],
                )
            ],
            agents=[AgentConfig(name="default", model="flash")],
        )
        monkeypatch.setattr("wing.metrics.report.get_config", lambda: config)
        seed_llm_call("s1", local_time(YESTERDAY), model="flash", provider="alpha")
        seed_llm_call("s1", local_time(YESTERDAY), model="flash", provider="undeclared")
        seed_llm_call("s1", local_time(YESTERDAY), model="other", provider="alpha")

        report = _report(group_by="model")

        assert _row_by_key(report, "alpha/flash")["label"] == "Alpha Flash"
        # provider 未声明 / 调用名未在本 provider 下声明 → 无展示名
        assert _row_by_key(report, "undeclared/flash")["label"] is None
        assert _row_by_key(report, "alpha/other")["label"] is None

    def test_tool_and_user_measures_are_not_attributable(self) -> None:
        """工具行没有 model 列、用户消息行也没有——这两侧的度量恒为 0。"""
        seed_llm_call("s1", local_time(YESTERDAY), model="m", provider="p")
        seed_tool_call("s1", local_time(YESTERDAY))
        seed_user_message("s1", local_time(YESTERDAY))

        row = _report(group_by="model")["rows"][0]

        assert row["llm_calls"] == 1
        assert row["tool_calls"] == 0
        assert row["tool_errors"] == 0
        assert row["tool_time_ms"] == 0
        assert row["user_messages"] == 0
        assert row["sessions_created"] == 0

    def test_token_ranking_and_key_tiebreak(self) -> None:
        seed_llm_call(
            "s1",
            local_time(YESTERDAY),
            model="b",
            provider="p",
            prompt=10,
            completion=0,
        )
        seed_llm_call(
            "s1",
            local_time(YESTERDAY),
            model="a",
            provider="p",
            prompt=10,
            completion=0,
        )
        seed_llm_call(
            "s1",
            local_time(YESTERDAY),
            model="c",
            provider="p",
            prompt=50,
            completion=0,
        )

        rows = _report(group_by="model")["rows"]

        # 主键降序；并列按 key 升序（全序，不随库内顺序漂移）
        assert [row["key"] for row in rows] == ["p/c", "p/a", "p/b"]


class TestToolDimension:
    def test_ranking_errors_and_key_tiebreak(self) -> None:
        seed_tool_call("s1", local_time(YESTERDAY), tool_name="Bash")
        seed_tool_call("s1", local_time(YESTERDAY), tool_name="Bash", success=False)
        seed_tool_call("s2", local_time(YESTERDAY), tool_name="Edit")
        seed_tool_call("s2", local_time(YESTERDAY), tool_name="Grep")
        seed_llm_call("s1", local_time(YESTERDAY))

        report = _report(group_by="tool")

        assert [row["key"] for row in report["rows"]] == ["Bash", "Edit", "Grep"]
        bash = _row_by_key(report, "Bash")
        assert bash["tool_calls"] == 2
        assert bash["tool_errors"] == 1
        assert bash["sessions"] == 1
        assert bash["tool_time_ms"] == 60
        # 模型侧与用户消息侧都不可归因（工具行没有 model 列、也没有用户消息列）
        assert bash["llm_calls"] == 0
        assert bash["prompt_tokens"] == 0
        assert bash["user_messages"] == 0
        # 目录事实不在 `_measure_columns` 里：只有 5 个维度由 report 注入，tool 维恒 0
        assert bash["sessions_created"] == 0

    def test_totals_stay_whole_event_when_rows_are_limited(self) -> None:
        for name in ("Bash", "Edit", "Grep", "Read"):
            seed_tool_call("s1", local_time(YESTERDAY), tool_name=name)

        report = _report(group_by="tool", limit=2)

        assert len(report["rows"]) == 2
        assert report["truncated"] is True
        assert report["totals"]["tool_calls"] == 4


# ============================================================
# 会话（含树）
# ============================================================


class TestSessionDimension:
    def test_rows_carry_parent_and_rank_by_tokens(self) -> None:
        record_session_created("root-1")
        record_session_created("child-1", parent_session_id="root-1")
        seed_llm_call("root-1", local_time(YESTERDAY), prompt=10, completion=0)
        seed_llm_call("child-1", local_time(YESTERDAY), prompt=90, completion=0)

        report = _report(group_by="session")

        assert [row["key"] for row in report["rows"]] == ["child-1", "root-1"]
        assert _row_by_key(report, "child-1")["parent_session_id"] == "root-1"
        assert _row_by_key(report, "root-1")["parent_session_id"] is None
        assert report["truncated"] is False

    def test_scope_without_tree_is_the_session_alone(self) -> None:
        record_session_created("root-1")
        record_session_created("child-1", parent_session_id="root-1")
        seed_llm_call("root-1", local_time(YESTERDAY), prompt=10)
        seed_llm_call("child-1", local_time(YESTERDAY), prompt=90)

        report = _report(group_by="session", session_id="root-1")

        assert [row["key"] for row in report["rows"]] == ["root-1"]
        assert report["totals"]["prompt_tokens"] == 10

    def test_tree_includes_descendants(self) -> None:
        """三层树：祖先的 tree 范围含全部子孙；兄弟与更深一层都在。"""
        record_session_created("root-1")
        record_session_created("child-1", parent_session_id="root-1")
        record_session_created("grand-1", parent_session_id="child-1")
        record_session_created("other-1")
        seed_llm_call("root-1", local_time(YESTERDAY), prompt=1)
        seed_llm_call("child-1", local_time(YESTERDAY), prompt=2)
        seed_llm_call("grand-1", local_time(YESTERDAY), prompt=4)
        seed_llm_call("other-1", local_time(YESTERDAY), prompt=8)

        report = _report(group_by="session", session_id="root-1", tree=True)

        assert {row["key"] for row in report["rows"]} == {
            "root-1",
            "child-1",
            "grand-1",
        }
        assert report["totals"]["prompt_tokens"] == 7

    def test_tree_terminates_on_a_cycle(self) -> None:
        """血缘里出现环（手改 metadata）时递归必须终止（``UNION`` 而非 ``UNION ALL``）。"""
        record_session_created("a", parent_session_id="b")
        record_session_created("b", parent_session_id="a")
        seed_llm_call("a", local_time(YESTERDAY), prompt=1)
        seed_llm_call("b", local_time(YESTERDAY), prompt=2)

        report = _report(group_by="session", session_id="a", tree=True)

        assert {row["key"] for row in report["rows"]} == {"a", "b"}
        assert report["totals"]["prompt_tokens"] == 3

    def test_self_row_exists_without_activity(self) -> None:
        """自身行恒存在（没活动就全 0）：`info` 因此能区分「存在」与「id 打错了」。"""
        record_session_created("quiet-1", workspace="/w")

        report = _report(group_by="session", session_id="quiet-1", tree=True)

        assert [row["key"] for row in report["rows"]] == ["quiet-1"]
        assert all(
            report["rows"][0][name] == 0
            for name in MEASURE_FIELDS
            if name != "sessions_created"
        )
        assert report["rows"][0]["sessions_created"] == 1

    def test_self_row_survives_limit(self) -> None:
        """自身行是 scope 的锚点：被排行挤出 `limit` 后仍出现在结果里。"""
        record_session_created("root-1")
        seed_llm_call("root-1", local_time(YESTERDAY), prompt=1)
        for index in range(3):
            child = f"child-{index}"
            record_session_created(child, parent_session_id="root-1")
            seed_llm_call(child, local_time(YESTERDAY), prompt=100 + index)

        report = _report(group_by="session", session_id="root-1", tree=True, limit=2)

        keys = [row["key"] for row in report["rows"]]
        assert len(keys) == 3
        assert keys[-1] == "root-1"
        assert report["truncated"] is True

    def test_active_session_without_catalog_row_appears(self) -> None:
        """目录里没有但有审计行（旧数据）：行照样出，parent 为 null。"""
        seed_llm_call("legacy-1", local_time(YESTERDAY))

        report = _report(group_by="session")

        assert [row["key"] for row in report["rows"]] == ["legacy-1"]
        assert report["rows"][0]["parent_session_id"] is None


# ============================================================
# 工作区 / 模板
# ============================================================


class TestWorkspaceAndAgent:
    def test_workspace_rows_group_activity_and_creation(self) -> None:
        record_session_created("w1", workspace="/proj/a")
        record_session_created("w2", workspace="/proj/a")
        record_session_created("w3", workspace="/proj/b")
        seed_llm_call("w1", local_time(YESTERDAY), prompt=10)
        seed_llm_call("w3", local_time(YESTERDAY), prompt=90)

        report = _report(group_by="workspace")

        assert [row["key"] for row in report["rows"]] == ["/proj/b", "/proj/a"]
        assert _row_by_key(report, "/proj/a")["sessions_created"] == 2
        assert _row_by_key(report, "/proj/a")["prompt_tokens"] == 10
        assert report["totals"]["sessions_created"] == 3

    def test_created_only_bucket_still_gets_a_row(self) -> None:
        """只在窗口内建过、还没有任何活动的桶也占一行（否则刚建的会话会静默消失）。"""
        record_session_created("w1", workspace="/proj/idle")

        report = _report(group_by="workspace")

        row = _row_by_key(report, "/proj/idle")
        assert row["sessions"] == 0
        assert row["sessions_created"] == 1

    def test_agent_rows_use_template_name_and_empty_key(self) -> None:
        record_session_created("t1", template_name="coder")
        record_session_created("t2")
        seed_llm_call("t1", local_time(YESTERDAY), prompt=10)
        seed_llm_call("t2", local_time(YESTERDAY), prompt=90)

        report = _report(group_by="agent")

        assert [row["key"] for row in report["rows"]] == ["", "coder"]
        assert _row_by_key(report, "")["sessions"] == 1
        assert _row_by_key(report, "coder")["sessions_created"] == 1


# ============================================================
# 参数校验 / 404 / 降级
# ============================================================


class TestValidation:
    def test_unknown_group_by(self) -> None:
        with pytest.raises(ValueError, match="unknown group_by"):
            usage_report(group_by="week")

    def test_bad_dates(self) -> None:
        with pytest.raises(ValueError, match="invalid since"):
            usage_report(since="2026-13-01")
        with pytest.raises(ValueError, match="invalid until"):
            usage_report(until="yesterday")

    def test_since_after_until(self) -> None:
        with pytest.raises(ValueError, match="is after until"):
            usage_report(since="2026-03-05", until="2026-03-01")

    def test_tree_requires_session(self) -> None:
        with pytest.raises(ValueError, match="tree requires session_id"):
            usage_report(tree=True)

    def test_limit_bounds(self) -> None:
        with pytest.raises(ValueError, match="limit must be between 1 and 200"):
            usage_report(limit=0)
        with pytest.raises(ValueError, match="limit must be between 1 and 200"):
            usage_report(limit=201)

    def test_invalid_session_id_shape(self) -> None:
        with pytest.raises(ValueError, match="invalid session id"):
            usage_report(session_id="../etc/passwd")

    def test_unknown_session_raises_lookup_error(self) -> None:
        record_session_created("known-1")
        with pytest.raises(LookupError, match="session not found"):
            usage_report(group_by="session", session_id="nope-1")

    def test_session_with_audit_rows_only_is_not_unknown(self) -> None:
        """404 判据 = 目录里没有**且**审计表也没有——有行就算存在。"""
        seed_llm_call("legacy-1", local_time(YESTERDAY))

        report = usage_report(group_by="session", session_id="legacy-1")

        assert report["totals"]["llm_calls"] == 1


class TestDefaultsAndAll:
    def test_default_window_is_the_last_84_days(self) -> None:
        report = usage_report()

        assert report["range"]["until"] == TODAY.isoformat()
        assert report["range"]["since"] == (TODAY - timedelta(days=84)).isoformat()
        assert len(report["rows"]) == 85

    def test_since_all_starts_at_the_earliest_record(self) -> None:
        early = date(2026, 1, 5)
        seed_llm_call("s1", local_time(early))
        seed_llm_call("s1", local_time(YESTERDAY))

        report = usage_report(since="all", until=UNTIL)

        assert report["range"]["since"] == early.isoformat()
        assert report["rows"][0]["key"] == early.isoformat()
        assert report["totals"]["llm_calls"] == 2

    def test_since_all_survives_records_after_until(self) -> None:
        """时钟回拨 / 向西跨时区会留下「晚于今天」的行：逃生路径不能因此变 400。"""
        future = date.today() + timedelta(days=10)
        seed_llm_call("s1", local_time(future))

        report = usage_report(since="all")

        assert report["range"]["since"] == TODAY.isoformat()
        assert report["range"]["until"] == TODAY.isoformat()
        assert len(report["rows"]) == 1

    def test_data_since_is_the_audit_start_not_the_catalog_start(
        self, tmp_path: Path
    ) -> None:
        """``data_since`` 只看三张 ``audit_*``：目录回填能把会话追溯得更早，读数却不行。"""
        # 回填把 2025-01-02 的存量会话写进目录，而审计流水只有今天的一条。
        store = _store(tmp_path)
        store.save_metadata("20250102-030405-abcdef", SessionMetadata(tags=LISTED))
        assert backfill_catalog([store], database=get_database()) == 1
        seed_llm_call("audit-1", local_time(TODAY, hour=9), prompt=10)

        metrics = MetricsStore()
        assert metrics.earliest_day() == "2025-01-02"
        assert metrics.earliest_audit_day() == TODAY.isoformat()

        # 端点：`data_since` 说流水从今天起；`since=all` 仍从目录的最早一天起
        # （计数类指标是完整的，token 类不是）。
        report = _report(since="all", until=UNTIL)
        assert report["range"]["data_since"] == TODAY.isoformat()
        assert report["range"]["since"] == "2025-01-02"

    def test_the_earliest_day_agrees_with_the_text_minimum(self) -> None:
        """`since=all` 的起点与 `MIN(substr(ts,1,10))` 同值（索引友好写法不改口径）。

        反向也钉住：`session_catalog` 的回填可以比流水早得多，`earliest_day` 要把它算进来。
        """
        seed_llm_call("a", local_time(TWO_DAYS_AGO, hour=23), prompt=1)
        seed_user_message("b", local_time(YESTERDAY, hour=1))
        record_session_created("catalog-only")

        database = get_database()
        expected = database.query(
            "SELECT MIN(d) AS day FROM ("
            "SELECT MIN(substr(ts, 1, 10)) AS d FROM audit_llm_call"
            " UNION ALL SELECT MIN(substr(ts, 1, 10)) FROM audit_tool_call"
            " UNION ALL SELECT MIN(substr(ts, 1, 10)) FROM audit_user_message"
            " UNION ALL SELECT MIN(substr(created_ts, 1, 10)) FROM session_catalog)"
        )[0]["day"]

        assert MetricsStore().earliest_day() == expected
        assert expected == TWO_DAYS_AGO.isoformat()

    def test_data_since_agrees_with_the_text_minimum(self) -> None:
        """`ORDER BY ts_ms LIMIT 1` 与 `MIN(substr(ts,1,10))` 必须给出同一天。

        这条钉的是两种写法**语义等价**：前者走 `ts_ms` 索引（每个请求都要走），
        后者是每张表的全扫描。
        """
        seed_llm_call("a", local_time(TWO_DAYS_AGO, hour=23), prompt=1)
        seed_llm_call("b", local_time(YESTERDAY, hour=1), prompt=1)
        seed_tool_call("a", local_time(TODAY, hour=8))
        seed_user_message("b", local_time(YESTERDAY, hour=2))

        database = get_database()
        expected = database.query(
            "SELECT MIN(d) AS day FROM ("
            "SELECT MIN(substr(ts, 1, 10)) AS d FROM audit_llm_call"
            " UNION ALL SELECT MIN(substr(ts, 1, 10)) FROM audit_tool_call"
            " UNION ALL SELECT MIN(substr(ts, 1, 10)) FROM audit_user_message)"
        )[0]["day"]

        assert MetricsStore().earliest_audit_day() == expected
        assert expected == TWO_DAYS_AGO.isoformat()

    def test_since_all_on_an_empty_database_is_one_day(self) -> None:
        report = usage_report(since="all", until=UNTIL)

        assert report["range"] == {
            "since": UNTIL,
            "until": UNTIL,
            "data_since": None,
        }
        assert len(report["rows"]) == 1
        assert all(report["rows"][0][name] == 0 for name in MEASURE_FIELDS)


class TestDegradedDatabase:
    def test_closed_database_returns_zeros(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """库打不开（降级）：聚合全零、行仍稠密、不抛——诊断数据缺失不是 500。"""
        blocked = tmp_path / "wing.db"
        blocked.mkdir()
        degraded = Database(blocked)
        assert not degraded.ready
        monkeypatch.setattr("wing.metrics.store.get_database", lambda: degraded)

        report = _report(group_by="origin")

        assert [row["key"] for row in report["rows"]] == ["me", "agents"]
        assert all(report["totals"][name] == 0 for name in MEASURE_FIELDS)

    def test_scoped_request_stays_200_when_degraded(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """库降级时不把「读不到」翻译成「不存在」：带 session_id 仍是 200 + 全零。"""
        blocked = tmp_path / "wing.db"
        blocked.mkdir()
        degraded = Database(blocked)
        assert not degraded.ready
        monkeypatch.setattr("wing.metrics.store.get_database", lambda: degraded)

        report = usage_report(
            since=SINCE, until=UNTIL, group_by="session", session_id="real-session"
        )

        assert [row["key"] for row in report["rows"]] == ["real-session"]
        assert all(report["totals"][name] == 0 for name in MEASURE_FIELDS)

    def test_store_reads_are_empty_when_degraded(self, tmp_path: Path) -> None:
        blocked = tmp_path / "wing.db"
        blocked.mkdir()
        store = MetricsStore(Database(blocked))

        assert store.earliest_day() is None
        assert store.has_usage_record("anything") is False
        assert store.by_day(_window()) == {}
        assert store.by_session(_window()) == []
        assert store.created_total(_window()) == 0


# ============================================================
# 会话目录的写入
# ============================================================


class TestCatalogWrites:
    def test_record_session_created_is_insert_once(self, tmp_path: Path) -> None:
        db = Database(tmp_path / "wing.db")
        assert db.ready

        record_session_created(
            "s1",
            parent_session_id="p1",
            workspace="/w",
            template_name="coder",
            model_id="mid",
            database=db,
        )
        record_session_created("s1", workspace="/other", database=db)

        rows = db.query("SELECT * FROM session_catalog")
        assert len(rows) == 1
        row = rows[0]
        assert row["session_id"] == "s1"
        assert row["created_source"] == "create"
        assert row["parent_session_id"] == "p1"
        assert row["workspace"] == "/w"  # 既有行不被覆盖
        assert row["template_name"] == "coder"
        assert row["model_id"] == "mid"
        assert row["created_ts"].startswith(f"{TODAY.isoformat()}T")
        assert row["created_at_ms"] == int(
            datetime.fromisoformat(row["created_ts"]).timestamp() * 1000
        )
        db.close()

    def test_record_failure_is_swallowed(self, tmp_path: Path) -> None:
        blocked = tmp_path / "wing.db"
        blocked.mkdir()
        degraded = Database(blocked)
        assert not degraded.ready

        record_session_created("s1", database=degraded)  # 不抛


# ============================================================
# 回填
# ============================================================


def _store(tmp_path: Path) -> FileSessionStore:
    return FileSessionStore(tmp_path / "sessions")


def _write_history(store: FileSessionStore, session_id: str, timestamp: str) -> None:
    store.open_log(session_id).append(
        [{"role": "user", "content": "hi", "ts": timestamp}]
    )


class TestBackfill:
    @pytest.fixture
    def db(self, tmp_path: Path):
        database = Database(tmp_path / "wing.db")
        assert database.ready
        yield database
        database.close()

    def test_three_precision_tiers(self, tmp_path: Path, db: Database) -> None:
        """创建时刻的三个来源各命中一次，``created_source`` 如实标注。"""
        store = _store(tmp_path)
        # ① id 前缀（秒级）
        store.save_metadata(
            "20260102-030405-abcdef",
            SessionMetadata(
                workspace="/w1", template_name="coder", model_id="mid", tags=LISTED
            ),
        )
        # ② history 首条记录（毫秒）
        store.save_metadata("custom-log", SessionMetadata(workspace="/w2", tags=LISTED))
        _write_history(store, "custom-log", "2026-02-03T04:05:06.789")
        # ③ 只剩 last_interaction（兜底）
        store.save_metadata(
            "custom-meta",
            SessionMetadata(last_interaction="2026-04-05T06:07:08", tags=LISTED),
        )

        added = backfill_catalog([store], database=db)

        assert added == 3
        rows = {
            row["session_id"]: row for row in db.query("SELECT * FROM session_catalog")
        }
        assert rows["20260102-030405-abcdef"]["created_source"] == "id"
        assert rows["20260102-030405-abcdef"]["created_ts"] == "2026-01-02T03:04:05"
        assert rows["20260102-030405-abcdef"]["workspace"] == "/w1"
        assert rows["custom-log"]["created_source"] == "log"
        assert rows["custom-log"]["created_ts"] == "2026-02-03T04:05:06.789000"
        assert rows["custom-meta"]["created_source"] == "meta"
        assert rows["custom-meta"]["created_ts"] == "2026-04-05T06:07:08"

    def test_no_timestamp_means_no_row(self, tmp_path: Path, db: Database) -> None:
        """三档都拿不到创建时刻 → 跳过（不发明时间）。"""
        store = _store(tmp_path)
        store.save_metadata(
            "no-time-anywhere", SessionMetadata(workspace="/w", tags=LISTED)
        )

        assert backfill_catalog([store], database=db) == 0
        assert db.query("SELECT * FROM session_catalog") == []

    def test_rerun_is_idempotent(self, tmp_path: Path, db: Database) -> None:
        store = _store(tmp_path)
        store.save_metadata("20260102-030405-abcdef", SessionMetadata(tags=LISTED))

        assert backfill_catalog([store], database=db) == 1
        assert backfill_catalog([store], database=db) == 0
        assert len(db.query("SELECT * FROM session_catalog")) == 1

    def test_archived_sessions_keep_lineage(self, tmp_path: Path, db: Database) -> None:
        """归档会话的父 / fork 源只有它自己的 metadata 里有（归档索引不存这两列）。"""
        store = _store(tmp_path)
        store.save_metadata(
            "20260102-030405-abcdef",
            SessionMetadata(parent_session_id="root-1", workspace="/w"),
        )
        store.save_metadata(
            "20260103-030405-abcdef", SessionMetadata(forked_from="src-1")
        )
        store.archive("20260102-030405-abcdef")
        store.archive("20260103-030405-abcdef")
        assert store.load_archived_metadata("20260102-030405-abcdef") is not None

        assert backfill_catalog([store], database=db) == 2
        rows = {
            row["session_id"]: row for row in db.query("SELECT * FROM session_catalog")
        }
        assert rows["20260102-030405-abcdef"]["parent_session_id"] == "root-1"
        assert rows["20260103-030405-abcdef"]["forked_from"] == "src-1"

    def test_archived_metadata_api(self, tmp_path: Path) -> None:
        """归档 metadata 只读口：live 区 / 归档区互不可见，不存在返回 None。"""
        store = _store(tmp_path)
        store.save_metadata(
            "20260102-030405-abcdef", SessionMetadata(workspace="/live", tags=LISTED)
        )
        assert store.load_archived_metadata("20260102-030405-abcdef") is None
        store.archive("20260102-030405-abcdef")
        archived = store.load_archived_metadata("20260102-030405-abcdef")
        assert archived is not None
        assert archived.workspace == "/live"
        assert store.load_archived_metadata("nope") is None
        with pytest.raises(ValueError):
            store.load_archived_metadata("../escape")

    def test_one_bad_session_does_not_stop_the_rest(
        self, tmp_path: Path, db: Database, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """单个会话的解析失败只留 WARNING，其它会话照补。"""
        store = _store(tmp_path)
        store.save_metadata("20260102-030405-abcdef", SessionMetadata(tags=LISTED))
        store.save_metadata("custom-broken", SessionMetadata(tags=LISTED))

        from wing.metrics import catalog as catalog_module

        real = catalog_module._created_at

        def _explode(store_: SessionStore, candidate: Any) -> Any:
            if candidate.session_id == "custom-broken":
                raise RuntimeError("boom")
            return real(store_, candidate)

        monkeypatch.setattr(catalog_module, "_created_at", _explode)

        assert backfill_catalog([store], database=db) == 1
