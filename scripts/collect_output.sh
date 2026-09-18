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

TMP_BASE="${TMPDIR:-/tmp}"
LOG_DIR="${TMP_BASE%/}/wing-${MODE}-logs"
mkdir -p "$LOG_DIR"

# ── 并行执行（组间互不依赖；各自输出入独立日志，避免互相刷屏）──────
# 串行执行时总耗时 = 各组之和；三组并行后 ≈ 最慢的一组。
# 需要排查并发干扰时：WING_TEST_SERIAL=1 make test
SERIAL="${WING_TEST_SERIAL:-0}"
rm -f "$LOG_DIR"/*.code "$LOG_DIR"/*.dur

START=$SECONDS
PIDS=()
for i in "${!LABELS[@]}"; do
  label="${LABELS[$i]}"
  cmd="${CMDS[$i]}"
  log="$LOG_DIR/${label}.log"
  printf '▶ %-6s %s（日志：%s）\n' "$label" "$cmd" "$log"
  if [ "$SERIAL" = "1" ]; then
    t0=$SECONDS
    eval "$cmd" >"$log" 2>&1
    echo "$?" >"$LOG_DIR/${label}.code"
    echo "$((SECONDS - t0))" >"$LOG_DIR/${label}.dur"
  else
    (
      t0=$SECONDS
      eval "$cmd" >"$log" 2>&1
      echo "$?" >"$LOG_DIR/${label}.code"
      echo "$((SECONDS - t0))" >"$LOG_DIR/${label}.dur"
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
if [ "${#FAILED[@]}" -eq 0 ]; then
  echo "✅ 结论：make $MODE 全部通过，无需重跑。"
  echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  exit 0
fi
echo "❌ 结论：make $MODE 失败于：${FAILED_STR}（详情见上方）——修复后重跑，已通过的组无需单独重跑。"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
exit 1
