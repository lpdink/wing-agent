"""缓存账本（KV 前缀缓存）端到端红线：理论命中率 + 分叉归因的可观测口径。

账本是「这次请求的前缀指纹」与「provider 报的实际命中」同放一张表：`GET
/api/session/cache` 给出 `summary`（三档命中率）、`calls`（每次调用一行）与 `forks`
（每次前缀破碎一行）。它只有观察面，没有护栏——所以口径一旦漂了，用户会看到一堆
似是而非的数字而无法行动。本文件把六条性质钉死（A–F，括号里是失败时被破坏的性质）：

- **A｜首调用 `cold`**：没有参照 → `expected` 为 null、`forks` 为空；`theory_rate`
  是 `0.0` 而不是 null（分母 Σprompt 非 0，null 只属于空账本，见 F）。
- **B｜前缀延续即命中**：provider 兑现整段参照前缀 → `expected` == 参照调用的
  `prompt`、`verdict == hit`、`forks` 仍空；直播字段（`llm_call_metrics`）与账本同源。
- **C｜块粒度**：只差一个缓存块（≤ `tol = max(256, 2% · 参照 prompt)`）→
  `block_granularity`，不是 `provider_underdelivered`。
- **D｜provider 欠兑现**：前缀逐段延续而 `cached == 0` → `provider_underdelivered`，
  归因是 provider 欠账（进 `summary.provider_debt_rate`），**不是**我们的分叉。
- **E｜外部分叉可点名**：改一个 rules 文件 → reload → 下一轮 `forks` 点名
  `rules:<abs-path>`、`blame == external`、`avoidable is True`，`verdict` 因优先级
  落 `prefix_broken`（`expected` 为 null）。
- **F｜无数据不报错**：新建未跑轮的会话（以及不存在的会话）→ 200 + 空 `forks` /
  `calls` + 全 null summary，**不是 404**。

数值纪律：usage 由剧本给定（假 Provider 是唯一数值来源），断言只写**相对关系**
（`expected == 参照调用的 prompt`、`theory_rate == Σexpected / Σprompt` …），不硬编码
token 数；`limit=200` 取全量 `calls`，以便与覆盖整本账本的 `summary` 对账。
"""

from __future__ import annotations

from typing import Any

import pytest
import yaml

from wing_probe import DriverHttpError, Probe, Session, Turn, Usage

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
MODEL = "probe/cache-ledger"

#: 每轮剧本给定的 usage（`prompt_tokens` 递增模拟真实对话的上下文增长）。
TURN_PROMPTS = (1000, 1200, 1400, 1600)

#: 块粒度缺口：落在 `tol = max(256, ceil(0.02 · 参照 prompt))` 之内。
BLOCK_SLACK = 128

#: rules 注入用（相对会话 workspace 的 glob，`load_rules` 的相对路径基准）。
RULES_PATTERN = "rules/*.md"
RULE_FILE = "team.md"

#: 端点 limit 取上界：`summary` 覆盖整本账本，`calls` 也要全量才对得上。
FULL_LIMIT = 200

#: 空账本 summary 的精确形状（contract §6：无数据 = 计数 0 + 三档 rate 全 null）。
EMPTY_SUMMARY = {
    "calls": 0,
    "prompt_tokens": 0,
    "cached_tokens": 0,
    "expected_tokens": 0,
    "theory_rate": None,
    "actual_rate": None,
    "provider_debt_rate": None,
}


# ── 驱动与对账 ────────────────────────────────────────────────────────────


def _turn(text: str, prompt: int, *, cached: int = 0) -> Turn:
    """一轮剧本：正文 + 给定的 usage（`cached` 就是 provider 报的实际命中）。"""
    return Turn.of(
        text=text,
        usage=Usage(prompt_tokens=prompt, completion_tokens=5, cached_tokens=cached),
    )


async def _cache_report(probe: Probe, session_id: str) -> dict[str, Any]:
    """`GET /api/session/cache`（全量 calls/forks）——本文件唯一的账本读取口。"""
    payload = await probe.driver_required.http.request(
        "GET",
        "/api/session/cache",
        params={"session_id": session_id, "limit": FULL_LIMIT},
    )
    assert isinstance(payload, dict), payload
    assert payload.get("session_id") == session_id, payload
    return payload


def _assert_rates_are_consistent(report: dict[str, Any], *, context: str) -> None:
    """三档 rate 与 `calls` 逐项对账（contract §6 的公式）。

    `theory_rate = Σexpected（仅非 null）/ Σprompt`、`actual_rate = Σcached /
    Σprompt`、`provider_debt_rate = Σmax(0, expected - cached)（仅 verdict ∈
    {provider_underdelivered, block_granularity}）/ Σprompt`；分母为 0 时三者全 null。

    这是「相对关系」断言：不关心具体 token 数，只要求 summary 与它汇总的那些行自洽
    ——公式抄错、把 null 的 expected 当成 0 计入、欠账口径漏了某个 verdict，都会红。
    """
    calls = report["calls"]
    summary = report["summary"]
    prompt = sum(call["prompt"] for call in calls)
    cached = sum(call["cached"] for call in calls)
    expected = sum(call["expected"] for call in calls if call["expected"] is not None)
    debt = sum(
        max(0, call["expected"] - call["cached"])
        for call in calls
        if call["expected"] is not None
        and call["verdict"] in ("provider_underdelivered", "block_granularity")
    )

    assert summary["calls"] == len(calls), (
        f"{context}：summary.calls 必须等于账本里的调用条数（账本丢了记录？）:"
        f" {summary['calls']} vs {len(calls)}"
    )
    assert summary["prompt_tokens"] == prompt, f"{context}: Σprompt 对不上"
    assert summary["cached_tokens"] == cached, f"{context}: Σcached 对不上"
    assert summary["expected_tokens"] == expected, (
        f"{context}: Σexpected 只累计非 null 的 expected:"
        f" {summary['expected_tokens']} vs {expected}"
    )
    if prompt == 0:
        for key in ("theory_rate", "actual_rate", "provider_debt_rate"):
            assert summary[key] is None, (
                f"{context}: 分母为 0 时 {key} 必须是 null（不是 0）: {summary[key]!r}"
            )
        return
    assert summary["theory_rate"] == pytest.approx(expected / prompt, abs=1e-6), (
        f"{context}: theory_rate 必须是 Σexpected / Σprompt:"
        f" {summary['theory_rate']!r} vs {expected / prompt!r}"
    )
    assert summary["actual_rate"] == pytest.approx(cached / prompt, abs=1e-6), (
        f"{context}: actual_rate 必须是 Σcached / Σprompt:"
        f" {summary['actual_rate']!r} vs {cached / prompt!r}"
    )
    assert summary["provider_debt_rate"] == pytest.approx(debt / prompt, abs=1e-6), (
        f"{context}: provider_debt_rate 只累计欠兑现与块粒度两档的缺口:"
        f" {summary['provider_debt_rate']!r} vs {debt / prompt!r}"
    )


def _last_metrics(session: Session) -> dict[str, Any]:
    """最近一条 `llm_call_metrics` 的载荷（直播字段的观测面，contract §5）。"""
    events = session.watch.events("llm_call_metrics")
    assert events, "本轮必须发出 llm_call_metrics 事件（用量与缓存归因的直播面）"
    return events[-1].data


# ── A｜首调用 cold ────────────────────────────────────────────────────────


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_first_call_is_cold_without_theoretical_watermark(probe: Probe) -> None:
    """A｜首调用 `cold`：无参照、无分叉、无理论水位。

    WHY：账本的第一行没有可比较的前缀——把它判成「命中」或「破碎」都会让首屏显示
    一个不存在的结论。`expected` 必须 null（没有参照就没有理论可命中量）。
    """
    probe.register(MODEL, _turn("reply one", TURN_PROMPTS[0]))
    session = await probe.session(model=MODEL)
    await session.chat("alpha")

    report = await _cache_report(probe, session.session_id)
    calls = report["calls"]
    assert len(calls) == 1, f"一轮对话恰好记一次调用: {calls}"

    first = calls[0]
    assert first["verdict"] == "cold", (
        "首调用没有参照，verdict 必须是 cold（不是 hit / prefix_broken）"
    )
    assert first["purpose"] == "main", f"主调用必须记成 purpose=main: {first}"
    assert first["expected"] is None, (
        "cold 没有理论水位：expected 必须是 null（不是 0）"
    )
    assert first["cached"] == 0, f"剧本报的 cached 是 0: {first}"
    assert report["forks"] == [], "首调用不产生分叉行"

    # 分母是 Σprompt（非 0）→ 理论命中率是 0.0，**不是 null**：null 只属于空账本
    # （contract §6「分母为 0 时三个 rate 均为 null」，空账本由 F 覆盖）。
    assert report["summary"]["expected_tokens"] == 0, report["summary"]
    assert report["summary"]["theory_rate"] == 0.0, (
        "首调用有 prompt 却无 expected → theory_rate 必须是 0.0（null 只属于空账本）"
    )
    _assert_rates_are_consistent(report, context="A 首调用 cold")


# ── B｜前缀延续即命中 ──────────────────────────────────────────────────────


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_continued_prefix_reports_hit_and_matches_live_metrics(
    probe: Probe,
) -> None:
    """B｜第二轮沿用第一轮的前缀 → `hit`，理论水位 == 参照调用的 prompt。

    WHY：这是账本存在的意义——「provider 兑现了整段前缀」必须显示成 100% 命中，
    而不是一个含糊的小数。直播字段与账本同源同刻（contract §5），所以两边必须
    给出同一个 verdict。
    """
    probe.register(
        MODEL,
        _turn("reply one", TURN_PROMPTS[0]),
        # 第二轮：provider 报「上一轮的 prompt 全部命中」。
        _turn("reply two", TURN_PROMPTS[1], cached=TURN_PROMPTS[0]),
    )
    session = await probe.session(model=MODEL)
    await session.chat("alpha")
    await session.chat("beta")

    report = await _cache_report(probe, session.session_id)
    calls = report["calls"]  # 新→旧
    assert len(calls) == 2, f"两轮对话两次调用: {calls}"

    latest, reference = calls[0], calls[1]
    assert reference["verdict"] == "cold", reference
    assert latest["verdict"] == "hit", (
        "provider 兑现了整段参照前缀（cached >= 参照 prompt）→ verdict 必须是 hit"
    )
    assert latest["expected"] == reference["prompt"], (
        "理论水位就是参照调用的 prompt（参照 = 上一次调用）:"
        f" {latest['expected']!r} vs {reference['prompt']!r}"
    )
    assert latest["cached"] == reference["prompt"], (
        f"剧本报的 cached 必须原样进账本: {latest['cached']!r}"
    )
    assert report["forks"] == [], "前缀延续就不是分叉"

    live = _last_metrics(session)
    assert live["cache_verdict"] == "hit", (
        f"直播字段必须与账本同源（contract §5）: {live}"
    )
    assert live["cache_expected_tokens"] == reference["prompt"], live
    assert live["cache_diverged"] == "", f"没有分叉 → cache_diverged 为空串: {live}"
    _assert_rates_are_consistent(report, context="B 前缀延续")


# ── C｜块粒度 ─────────────────────────────────────────────────────────────


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_block_granularity_is_not_provider_underdelivery(probe: Probe) -> None:
    """C｜缺口落在 `tol` 之内 → `block_granularity`（provider 只能按块兑现）。

    WHY：缓存按块兑现，差一个块不是 provider 违约。把这种缺口判成欠账会让人去报
    一个不存在的 bug；判成 hit 又会掩盖真实损耗。它只进 `provider_debt_rate`。
    """
    probe.register(
        MODEL,
        _turn("reply one", TURN_PROMPTS[0]),
        _turn("reply two", TURN_PROMPTS[1], cached=TURN_PROMPTS[0]),
        _turn(
            "reply three",
            TURN_PROMPTS[2],
            cached=TURN_PROMPTS[1] - BLOCK_SLACK,
        ),
    )
    session = await probe.session(model=MODEL)
    await session.chat("alpha")
    await session.chat("beta")
    await session.chat("gamma")

    report = await _cache_report(probe, session.session_id)
    calls = report["calls"]  # 新→旧
    assert len(calls) == 3, f"三轮对话三次调用: {[call['verdict'] for call in calls]}"

    latest, reference = calls[0], calls[1]
    assert latest["expected"] == reference["prompt"], latest
    gap = reference["prompt"] - latest["cached"]
    assert 0 < gap <= max(256, reference["prompt"] * 0.02), (
        f"剧本构造的缺口必须真的落在 tol 之内: gap={gap}, ref={reference['prompt']}"
    )
    assert latest["verdict"] == "block_granularity", (
        f"缺口 {gap} token 在块粒度容差内 → verdict 必须是 block_granularity"
        f"（不是 provider_underdelivered）: {latest}"
    )
    assert report["forks"] == [], "块粒度缺口不是分叉"

    assert report["summary"]["provider_debt_rate"] > 0, (
        "块粒度缺口进 provider 欠账汇总（provider_debt_rate > 0）"
    )
    _assert_rates_are_consistent(report, context="C 块粒度")


# ── D｜provider 欠兑现 ────────────────────────────────────────────────────


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_underdelivery_is_provider_debt_and_not_a_fork(probe: Probe) -> None:
    """D｜前缀逐段相同而 `cached == 0` → provider 欠账，不是我们的分叉。

    WHY：这条是「我该报 bug」与「我什么都做不了」的分界：前缀没变（我们的请求
    逐段与上一轮一致）而 provider 一个 token 都没兑现——责任在 provider 侧，
    它只进 `summary.provider_debt_rate`，`forks`（前缀破碎）必须干净。

    `forks` 只收 `fork != null` 的调用（contract §6），而 `fork` 只在「前缀没能延续」
    时产生、`prefix_broken` 又优先于其余 verdict（contract §4）——所以欠兑现永远
    进不了 `forks`。同一性质在 `calls`（账本）与 `llm_call_metrics`（直播）两侧钉住。
    """
    probe.register(
        MODEL,
        _turn("reply one", TURN_PROMPTS[0]),
        _turn("reply two", TURN_PROMPTS[1], cached=TURN_PROMPTS[0]),
        _turn("reply three", TURN_PROMPTS[2], cached=TURN_PROMPTS[1]),
        # 第四轮：前缀没变，provider 却报 cached=0。
        _turn("reply four", TURN_PROMPTS[3], cached=0),
    )
    session = await probe.session(model=MODEL)
    for message in ("alpha", "beta", "gamma", "delta"):
        await session.chat(message)

    report = await _cache_report(probe, session.session_id)
    calls = report["calls"]  # 新→旧
    assert len(calls) == 4, f"四轮对话四次调用: {[c['verdict'] for c in calls]}"

    latest, reference = calls[0], calls[1]
    assert latest["expected"] == reference["prompt"], (
        "前缀逐段延续时理论水位仍是参照调用的 prompt:"
        f" {latest['expected']!r} vs {reference['prompt']!r}"
    )
    assert latest["cached"] == 0, f"剧本报的 cached 是 0: {latest}"
    assert latest["verdict"] == "provider_underdelivered", (
        "前缀没碎而 cached 远低于理论水位 → verdict 必须是 provider_underdelivered"
        f"（tol = max(256, 2%·ref.prompt)）: {latest}"
    )
    assert report["forks"] == [], (
        "provider 欠兑现不是分叉：forks 只收前缀破碎（fork != null）的调用，"
        f"欠账只进 summary.provider_debt_rate: {report['forks']}"
    )
    assert report["summary"]["provider_debt_rate"] > 0, (
        f"欠兑现必须进 provider 欠账汇总: {report['summary']}"
    )

    live = _last_metrics(session)
    assert live["cache_verdict"] == "provider_underdelivered", (
        f"直播字段必须与账本同源（contract §5）: {live}"
    )
    assert live["cache_diverged"] == "", (
        f"没有分叉 → cache_diverged 必须是空串（不是 provider 之类）: {live}"
    )
    _assert_rates_are_consistent(report, context="D provider 欠兑现")


# ── E｜外部分叉可点名 ────────────────────────────────────────────────────


def _write_rule(probe: Probe, content: str) -> str:
    """写会话 workspace 里的 rules 文件，返回其绝对路径（label 里就是它）。"""
    path = probe.workspace / "rules" / RULE_FILE
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    return str(path)


def _enable_rules(probe: Probe) -> None:
    """把 rules glob 挂到盘上 config.yaml 的 default agent（reload 才生效）。"""
    config: dict[str, Any] = yaml.safe_load(
        probe.env.config_path.read_text(encoding="utf-8")
    )
    config["agents"][0]["rules"] = [RULES_PATTERN]
    probe.env.config_path.write_text(
        yaml.safe_dump(config, sort_keys=False, allow_unicode=True), encoding="utf-8"
    )


async def _reload(probe: Probe, *, context: str) -> None:
    """`POST /api/system/reload`，并断言 skills & rules 那一项真的重载了。"""
    result = await probe.driver_required.http.reload()
    assert result.get("ok") is True, f"{context}: reload 必须整体 ok: {result}"
    items = {item["name"]: item for item in result.get("results", [])}
    assert items.get("skills & rules", {}).get("ok") is True, (
        f"{context}: reload 的「skills & rules」项必须成功（否则 rules 不会重读）:"
        f" {result}"
    )


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_rules_change_is_an_external_avoidable_fork(probe: Probe) -> None:
    """E｜改 rules 文件 → reload → 下一轮点名 `rules:<path>`，external 且可避免。

    WHY：这是唯一「我能避免」的归因路径——用户改一个 rules 文件就让整段前缀失效，
    账本必须点名到文件（label + `detail.path` + `sha_before/sha_after`），并且
    `avoidable=true`。判成 `composition` 或只说「前缀碎了」都不可行动。

    rules 段在 `system:base` 之后（本场景无 `append_system_prompt`），故 fork 点
    `index == 1`；label 在两侧都存在而 sha 变了，依 contract §4 的 kind 判定是
    `modify`。有 fork ⇒ `prefix_broken` 且 `expected` 为 null（优先级）。
    """
    rule_path = _write_rule(probe, "RULE: answer in English.")
    _enable_rules(probe)
    # reload 先于建会话：模板里的 rules glob 只在会话创建时进入 CM。
    await _reload(probe, context="E 注入 rules")

    probe.register(
        MODEL,
        _turn("reply one", TURN_PROMPTS[0]),
        _turn("reply two", TURN_PROMPTS[1], cached=0),
    )
    session = await probe.session(model=MODEL)
    await session.chat("alpha")

    before = await _cache_report(probe, session.session_id)
    assert [call["verdict"] for call in before["calls"]] == ["cold"], before["calls"]
    assert before["forks"] == [], "首调用没有参照 → 没有分叉"

    _write_rule(probe, "RULE: answer in English.\nRULE: cite the file you changed.")
    await _reload(probe, context="E 改写 rules 后")
    await session.chat("beta")

    report = await _cache_report(probe, session.session_id)
    calls = report["calls"]
    assert len(calls) == 2, f"两轮对话两次调用: {[c['verdict'] for c in calls]}"
    latest = calls[0]
    assert latest["verdict"] == "prefix_broken", (
        "改 rules 文件让前缀没能延续 → prefix_broken（优先级高于 hit / cold）:"
        f" {latest}"
    )
    assert latest["expected"] is None, (
        f"前缀破碎时没有理论水位（expected 为 null）: {latest}"
    )

    forks = report["forks"]
    assert len(forks) == 1, f"改 rules 恰好产生一个分叉行: {forks}"
    fork = forks[0]
    assert fork["label"] == f"rules:{rule_path}", (
        f"分叉必须点名到 rules 文件（label = rules:<abs-path>）: {fork}"
    )
    assert fork["blame"] == "external", (
        f"rules / skills 的变更归因是 external（我能避免）: {fork}"
    )
    assert fork["avoidable"] is True, f"external 分叉必须标成可避免: {fork}"
    assert fork["kind"] == "modify", (
        f"同一 label 的 sha 变了 → kind 必须是 modify: {fork}"
    )
    assert fork["index"] == 1, (
        f"rules 段紧跟 system:base（本场景无 system:append）→ index == 1: {fork}"
    )
    assert fork["detail"]["path"] == rule_path, (
        f"detail.path 必须指向被改的那个文件: {fork['detail']}"
    )
    sha_before = fork["detail"]["sha_before"]
    sha_after = fork["detail"]["sha_after"]
    assert sha_before and sha_after and sha_before != sha_after, (
        f"detail 必须给出改前 / 改后的段 sha（内容真的变了）: {fork['detail']}"
    )
    assert fork["tokens_lost"] is None, (
        f"没有参照调用时 tokens_lost 必须是 null（估算没有基线）: {fork}"
    )

    live = _last_metrics(session)
    assert live["cache_verdict"] == "prefix_broken", (
        f"直播字段必须与账本同源（contract §5）: {live}"
    )
    assert live["cache_diverged"] == fork["label"], (
        "cache_diverged 就是分叉处的段 label（contract §5）:"
        f" {live['cache_diverged']!r} vs {fork['label']!r}"
    )
    # wire 出口剥除值为 null 的字段（`event.wire_dump`）：没有理论水位时该键**缺席**，
    # 与「旧网关没有这个字段」是同一形态（contract §5「缺省/旧网关 = 无」）。
    assert live.get("cache_expected_tokens") is None, (
        f"前缀破碎没有理论水位 → 该字段在帧里缺席（null 不下发）: {live}"
    )
    _assert_rates_are_consistent(report, context="E 外部分叉")


# ── F｜无数据不报错 ──────────────────────────────────────────────────────


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_session_without_calls_reports_empty_ledger(probe: Probe) -> None:
    """F｜没有调用记录的会话（含不存在的会话）→ 200 + 空账本，不是 404。

    WHY：前端与 CLI 都按同一条路径渲染「还没数据」。回 404 会让它们把「新会话」
    当成错误，或者在错误处理里再分一遍支。
    """
    session = await probe.session(model=MODEL)  # 建会话，但一轮都不跑
    try:
        report = await _cache_report(probe, session.session_id)
    except DriverHttpError as exc:
        pytest.fail(
            "无账本的会话必须回 200 + 空账本（contract §6：不要 404），"
            f"实际 HTTP {exc.status}"
        )

    recorded = probe.last_http_call(path="/api/session/cache")
    assert recorded is not None and recorded.status == 200, (
        f"GET /api/session/cache 必须回 200: {recorded!r}"
    )
    assert report["forks"] == [], f"没有调用就没有分叉: {report['forks']}"
    assert report["calls"] == [], f"没有调用就没有调用行: {report['calls']}"
    assert report["summary"] == EMPTY_SUMMARY, (
        f"空账本的 summary 必须全 0 / 全 null: {report['summary']}"
    )

    # 会话不存在同样 200（前端不该为「新会话」与「没这个会话」分两条路径）。
    ghost = "19990101-000000-ghost001"
    ghost_report = await _cache_report(probe, ghost)
    assert ghost_report["calls"] == [], ghost_report
    assert ghost_report["forks"] == [], ghost_report
    assert ghost_report["summary"] == EMPTY_SUMMARY, ghost_report["summary"]
    _assert_rates_are_consistent(ghost_report, context="F 不存在的会话")
