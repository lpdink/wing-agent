# wing/common/partial_json.py — 容错 JSON 解析（流式工具参数）

"""
Best-effort parsing of potentially incomplete JSON during LLM streaming.

Strategy (mirrors PI json-parse.ts):
  1. Try json.loads directly.
  2. Repair malformed escapes / control chars, then close unterminated
     strings and open brackets, then json.loads.
  3. Return {} as last resort.
"""

from __future__ import annotations

import json

_VALID_ESCAPES = frozenset('"\\/bfnrtu')


def _repair_json(raw: str) -> str:
    """Escape raw control characters and invalid backslash sequences inside strings."""
    out: list[str] = []
    in_string = False
    i = 0
    n = len(raw)

    while i < n:
        ch = raw[i]

        if not in_string:
            out.append(ch)
            if ch == '"':
                in_string = True
            i += 1
            continue

        # Inside a string literal
        if ch == '"':
            out.append(ch)
            in_string = False
            i += 1
            continue

        if ch == "\\":
            next_ch = raw[i + 1] if i + 1 < n else None
            if next_ch is None:
                # Trailing backslash — double it so JSON parser sees literal \
                out.append("\\\\")
                i += 1
                continue
            if next_ch == "u":
                # Check for valid \uXXXX
                hex_digits = raw[i + 2 : i + 6]
                if len(hex_digits) == 4 and all(
                    c in "0123456789abcdefABCDEF" for c in hex_digits
                ):
                    out.append(raw[i : i + 6])
                    i += 6
                    continue
                # Incomplete unicode escape at end of stream — drop it
                out.append("\\\\")
                i += 1
                continue
            if next_ch in _VALID_ESCAPES:
                out.append(f"\\{next_ch}")
                i += 2
                continue
            # Invalid escape — double the backslash
            out.append("\\\\")
            i += 1
            continue

        # Control character inside string
        cp = ord(ch)
        if cp <= 0x1F:
            escape_map = {
                0x08: "\\b",
                0x0C: "\\f",
                0x0A: "\\n",
                0x0D: "\\r",
                0x09: "\\t",
            }
            out.append(escape_map.get(cp, f"\\u{cp:04x}"))
        else:
            out.append(ch)
        i += 1

    return "".join(out)


def _close_json(raw: str) -> str:
    """Close unterminated strings and open brackets/braces."""
    out: list[str] = []
    in_string = False
    escaped = False
    stack: list[str] = []  # tracks open { and [
    # Track last structural char outside strings to determine context
    # when an unterminated string is encountered.
    last_structural = ""  # one of: { [ , : or ""

    for ch in raw:
        out.append(ch)

        if escaped:
            escaped = False
            continue

        if in_string:
            if ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
            continue

        # Outside string
        if ch == '"':
            in_string = True
        elif ch in "{[":
            stack.append(ch)
            last_structural = ch
        elif ch in "}]":
            if stack:
                stack.pop()
            last_structural = ch
        elif ch in ",:":
            last_structural = ch

    # Close unterminated string
    if in_string:
        if escaped:
            # Trailing backslash inside string — remove it then close
            out.pop()
        out.append('"')
        # If the unterminated string is a key (after { or , in object context),
        # add empty value. In array context (top of stack is [), no value needed.
        if last_structural in "{," and (not stack or stack[-1] == "{"):
            out.append(': ""')

    # Remove trailing comma before closing brackets (invalid JSON)
    result = "".join(out).rstrip()
    if result.endswith(","):
        result = result[:-1]

    # Close open brackets in reverse order
    for opener in reversed(stack):
        result += "}" if opener == "{" else "]"

    return result


def parse_streaming_json(raw: str) -> dict:
    """Parse potentially incomplete JSON, returning best-effort dict.

    Never raises. Returns {} on total failure.
    """
    if not raw or not raw.strip():
        return {}

    # Level 1: direct parse
    try:
        result = json.loads(raw)
        return result if isinstance(result, dict) else {}
    except (json.JSONDecodeError, ValueError):
        pass

    # Level 2: repair + close + parse
    try:
        repaired = _repair_json(raw)
        closed = _close_json(repaired)
        result = json.loads(closed)
        return result if isinstance(result, dict) else {}
    except (json.JSONDecodeError, ValueError):
        pass

    # Level 3: give up gracefully
    return {}
