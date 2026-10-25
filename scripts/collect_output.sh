#!/bin/bash
# 分组执行 + 结果汇总的执行器（make test / make check 共用）。
#
# 动机：逐组跑并直接把输出刷到 stdout 时，读的人（尤其是只看尾部输出的 agent）
# 只能看到最后一组的日志——前面的组过了没有、跑没跑，无从判断，于是常常整轮重跑。
#
# 行为：
#   1. 各组并行执行（组间互不依赖），输出各自收集到独立日志，不刷屏；
#   2. 失败组的详情贴在末尾**之前**；
#   3. 最后一段永远是结论块：逐组状态 + 统计 + 总用时 + 一句话结论。
# 这样即使 tail 给得很短，也能直接看到「过没过」，不需要重跑。
# 排查并发干扰时用 WING_TEST_SERIAL=1 退回串行。
#
# 用法：scripts/collect_output.sh [test|check]
set -uo pipefail

MODE="${1:-test}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 2

case "$MODE" in
  test)
    LABELS=("python" "probe" "rust" "ts")
    CMDS=("make test-python" "make test-probe" "make test-rust" "make test-ts")
    ;;
  check)
    LABELS=("python" "rust" "ts")
    CMDS=("make check-python" "make check-rust" "make check-ts")
    ;;
  *)
    echo "usage: $0 [test|check]" >&2
    exit 2
    ;;
esac

# SKIP_TS=1：本地逃生舱，ts 组不跑。这里把它从组列表里摘掉，
# 并在结论块里显式标注「已跳过」——否则「没跑」会被读成「跑了且过了」。
# CI 不设这个变量，门禁照旧强制。
SKIP_TS="${SKIP_TS:-}"
TS_SKIPPED=0
if [ -n "$SKIP_TS" ] && [ "$SKIP_TS" != "0" ]; then
  TS_SKIPPED=1
  KEPT_LABELS=()
  KEPT_CMDS=()
  for i in "${!LABELS[@]}"; do
    [ "${LABELS[$i]}" = "ts" ] && continue
    KEPT_LABELS+=("${LABELS[$i]}")
    KEPT_CMDS+=("${CMDS[$i]}")
  done
  LABELS=(${KEPT_LABELS[@]+"${KEPT_LABELS[@]}"})
  CMDS=(${KEPT_CMDS[@]+"${KEPT_CMDS[@]}"})
fi

TMP_BASE="${TMPDIR:-/tmp}"
# 日志目录按 checkout 分开：一台机器上并行开几个 worktree 是常态（见 AGENTS.md
# 「Worktree 纪律」），而每个 worktree 里都有 agent 在跑 `make test`。共用一个目录
# 时并发两轮会互相覆盖 .code / .dur / *.log——实测出现过「失败的组被判成通过」
# （结论块读到了另一轮刚写下的 exit=0）。cksum 是 POSIX，macOS / Linux 都能用。
ROOT_TAG="$(printf '%s' "$ROOT" | cksum | cut -d' ' -f1)"
LOG_DIR="${TMP_BASE%/}/wing-${MODE}-logs-${ROOT_TAG}"
# 目录带 tag 之后每个曾经存在过的 checkout 都会留一份：worktree 按纪律收走后
# 没人回来删它。按目录里记下的 ROOT 判活，已消失的整份清掉。旧的无 tag 目录
# （`wing-<mode>-logs`）没有 root 记录，不碰。
for stale in "$TMP_BASE"/wing-*-logs-*; do
  [ -d "$stale" ] && [ "$stale" != "$LOG_DIR" ] || continue
  recorded="$(cat "$stale/root" 2>/dev/null || true)"
  if [ -n "$recorded" ] && [ ! -d "$recorded" ]; then
    rm -rf "$stale"
  fi
done
mkdir -p "$LOG_DIR"
printf '%s\n' "$ROOT" >"$LOG_DIR/root"

# ── 并行执行（组间互不依赖；各自输出入独立日志，避免互相刷屏）──────
# 串行执行时总耗时 = 各组之和；三组并行后 ≈ 最慢的一组。
# 需要排查并发干扰时：WING_TEST_SERIAL=1 make test
SERIAL="${WING_TEST_SERIAL:-0}"
rm -f "$LOG_DIR"/*.code "$LOG_DIR"/*.dur

# 四组并行 ⇒ 每组都不独占整机：导出这个标记，各组的默认并行度收到"共享"档
# （见 Makefile 的 PYTEST_WORKERS / TS_WORKERS 与 scripts/test_rust_parallel.py 的
# `_default_cores`）。不导出时它们各自按独占配——单独跑一组的人应该拿满机器。
# 串行模式（WING_TEST_SERIAL=1）不导出：那时每组本来就独占。
SHARED=0
if [ "$SERIAL" != "1" ]; then
  export WING_TEST_SHARED=1
  SHARED=1
fi

# 亚秒级计时只给机器读的报告用（`WING_TEST_REPORT=<path>`，见 scripts/bench_tests.py）：
# bash 3.2 没有 EPOCHREALTIME，一次取时要 fork 一个小工具，日常跑没必要为此付费。
# 人看的 `用时 Ns` 永远是整秒（`$SECONDS`），与今天的输出逐字一致。
REPORT="${WING_TEST_REPORT:-}"
now_ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }
if [ -n "$REPORT" ]; then
  START_MS=$(now_ms)
fi

START=$SECONDS
PIDS=()
for i in "${!LABELS[@]}"; do
  label="${LABELS[$i]}"
  cmd="${CMDS[$i]}"
  log="$LOG_DIR/${label}.log"
  printf '▶ %-6s %s（日志：%s）\n' "$label" "$cmd" "$log"
  if [ "$SERIAL" = "1" ]; then
    t0=$SECONDS
    [ -n "$REPORT" ] && t0_ms=$(now_ms)
    eval "$cmd" >"$log" 2>&1
    echo "$?" >"$LOG_DIR/${label}.code"
    echo "$((SECONDS - t0))" >"$LOG_DIR/${label}.dur"
    [ -n "$REPORT" ] && echo "$(( $(now_ms) - t0_ms ))" >"$LOG_DIR/${label}.ms"
  else
    (
      t0=$SECONDS
      [ -n "$REPORT" ] && t0_ms=$(now_ms)
      eval "$cmd" >"$log" 2>&1
      echo "$?" >"$LOG_DIR/${label}.code"
      echo "$((SECONDS - t0))" >"$LOG_DIR/${label}.dur"
      [ -n "$REPORT" ] && echo "$(( $(now_ms) - t0_ms ))" >"$LOG_DIR/${label}.ms"
    ) &
    PIDS+=("$!")
  fi
done
if [ "$SERIAL" != "1" ]; then
  for pid in "${PIDS[@]}"; do
    wait "$pid"
  done
fi
WALL=$((SECONDS - START))

CODES=()
DURS=()
for i in "${!LABELS[@]}"; do
  label="${LABELS[$i]}"
  code=$(cat "$LOG_DIR/${label}.code" 2>/dev/null || echo 1)
  dur=$(cat "$LOG_DIR/${label}.dur" 2>/dev/null || echo 0)
  CODES+=("$code")
  DURS+=("$dur")
  if [ "$code" -eq 0 ]; then
    printf '  └ %-6s 完成，用时 %ss\n' "$label" "$dur"
  else
    printf '  └ %-6s 失败（exit=%s），用时 %ss\n' "$label" "$code" "$dur"
  fi
done

# ── 从日志里提取一句统计 ─────────────────────────────────────
# 优先级：pytest 汇总行 → cargo test result 求和 → 检查类工具的 ✅/❌ 清单
stats_of() {
  local log="$1" line
  line=$(grep -E '^=+ .*(passed|failed|error|no tests ran)' "$log" 2>/dev/null | tail -1)
  if [ -n "$line" ]; then
    echo "$line" | sed -e 's/^=*//' -e 's/=*$//' -e 's/^ *//' -e 's/ *$//'
    return
  fi
  if grep -q '^test result:' "$log" 2>/dev/null; then
    awk '
      /^test result:/ {
        n++
        for (i = 1; i <= NF; i++) {
          if ($i == "passed;") p += $(i - 1)
          if ($i == "failed;") f += $(i - 1)
          if ($i == "ignored;") g += $(i - 1)
        }
      }
      END { printf "%d passed, %d failed, %d ignored (%d suites)", p, f, g, n }
    ' "$log"
    return
  fi
  grep -E '^(✅|❌) ' "$log" 2>/dev/null |
    sed -e 's/^✅ *//' -e 's/^❌ *//' -e 's/ *passed$//' -e 's/ *failed$//' |
    paste -sd '、' -
}

FAILED=()
for i in "${!LABELS[@]}"; do
  if [ "${CODES[$i]}" -ne 0 ]; then
    FAILED+=("${LABELS[$i]}")
  fi
done
FAILED_STR=""
if [ "${#FAILED[@]}" -gt 0 ]; then
  FAILED_STR=$(printf '%s、' "${FAILED[@]}")
  FAILED_STR="${FAILED_STR%、}"
fi

# ── 失败详情（排在结论之前：结论必须永远在最后）──────────────────
for i in "${!LABELS[@]}"; do
  label="${LABELS[$i]}"
  [ "${CODES[$i]}" -eq 0 ] && continue
  log="$LOG_DIR/${label}.log"
  echo ""
  echo "───── ${label} 失败详情（exit=${CODES[$i]}）完整日志：$log ─────"
  summary_lines=$(grep -E '^(FAILED|ERROR) ' "$log" 2>/dev/null | head -20)
  if [ -n "$summary_lines" ]; then
    echo "失败用例："
    echo "$summary_lines"
    echo ""
  fi
  echo "最后 100 行输出："
  tail -100 "$log"
  echo "───── ${label} 详情结束 ─────"
done

# ── 结论块（最后一段，永远包含逐组状态与一句话结论）──────────────
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
if [ "${#FAILED[@]}" -eq 0 ]; then
  printf '✅ make %s 全部通过（%d 组，总用时 %ss）\n' "$MODE" "${#LABELS[@]}" "$WALL"
else
  printf '❌ make %s 失败：%d/%d 组未通过（%s，总用时 %ss）\n' \
    "$MODE" "${#FAILED[@]}" "${#LABELS[@]}" "$FAILED_STR" "$WALL"
fi
for i in "${!LABELS[@]}"; do
  label="${LABELS[$i]}"
  stat=$(stats_of "$LOG_DIR/${label}.log")
  if [ "${CODES[$i]}" -eq 0 ]; then
    icon="✅"
    note=""
  else
    icon="❌"
    note="  ← 详情见上方"
  fi
  printf '  %s %-6s %4ss  %s%s\n' "$icon" "$label" "${DURS[$i]}" "$stat" "$note"
done
if [ "$TS_SKIPPED" -eq 1 ]; then
  printf '  ⏭️  %-6s  —     TypeScript 组已按 SKIP_TS=%s 跳过（CI 不设该变量：那边照旧强制）\n' "ts" "$SKIP_TS"
fi

# 机器读的报告（可选）：一次运行的墙钟 + 逐组毫秒耗时 + 统计行。
# 消费者只有一个——scripts/bench_tests.py（提速的前后对比）；它要的是可解析的
# 数字，而终端里那几行是给人读的、精度只有整秒。
write_report() {
  [ -n "$REPORT" ] || return 0
  {
    printf '{\n  "mode": "%s", "wall_ms": %s, "serial": %s, "shared": %s, "skip_ts": %s,\n' \
      "$MODE" "$(( $(now_ms) - START_MS ))" \
      "$([ "$SERIAL" = "1" ] && echo true || echo false)" \
      "$([ "$SHARED" = "1" ] && echo true || echo false)" \
      "$([ "$TS_SKIPPED" -eq 1 ] && echo true || echo false)"
    printf '  "groups": [\n'
    for i in "${!LABELS[@]}"; do
      label="${LABELS[$i]}"
      ms=$(cat "$LOG_DIR/${label}.ms" 2>/dev/null || echo 0)
      stat=$(stats_of "$LOG_DIR/${label}.log" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')
      printf '    {"label": "%s", "code": %s, "ms": %s, "stat": "%s"}%s\n' \
        "$label" "${CODES[$i]}" "$ms" "$stat" \
        "$([ "$i" -lt "$(( ${#LABELS[@]} - 1 ))" ] && echo ,)"
    done
    printf '  ]\n}\n'
  } >"$REPORT"
}

if [ "${#FAILED[@]}" -eq 0 ]; then
  echo "✅ 结论：make $MODE 全部通过，无需重跑。"
  echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  write_report
  exit 0
fi
echo "❌ 结论：make $MODE 失败于：${FAILED_STR}（详情见上方）——修复后重跑，已通过的组无需单独重跑。"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
write_report
exit 1
