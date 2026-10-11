"""Hashline 补丁核心（``wing/tools/experimental/hashline.py``）——实验性行锚编辑。

被守的语义（借鉴 oh-my-pi 的 hashline 格式）：

- ``content_tag``：LF 规范化 + sha256 前 4 位十六进制（大写）——行尾风格不改变
  内容同一性；
- 解析：裸段 / ``*** Begin Patch`` 包装都收；块定位（``N*``）显式拒绝；
- 应用：所有定位子相对**原文快照**行号（与同载荷中先前操作无关）；
  替换区间不相交、插入点不落进替换区间；命名寄存器载荷内先捕获后使用；
- 窗口：变更区间 ± 3 行，两侧各自独立裁剪——插入 / 纯删除的另一侧按"间隙"表达。

纯逻辑测试：不碰工具注册、不碰 IO。
"""

from __future__ import annotations

import hashlib

import pytest

from wing.tools.experimental.hashline import (
    DIFF_CONTEXT_LINES,
    CutOp,
    HashlineError,
    PutOp,
    apply_section,
    content_tag,
    decode_utf8,
    normalize_newlines,
    parse_payload,
    split_lines,
)


def put_view(section):
    """(loc, body, register) 视图——排除 line 字段（两种载荷的绝对行号不同）。"""
    return [
        (op.loc, op.body, op.register) for op in section.ops if isinstance(op, PutOp)
    ]


# ── content_tag ───────────────────────────────────────────────────────


def test_content_tag_is_sha256_prefix_uppercase():
    expected = hashlib.sha256("alpha\nbeta\n".encode("utf-8")).hexdigest()[:4].upper()
    assert content_tag("alpha\nbeta\n") == expected
    assert len(expected) == 4


def test_content_tag_normalizes_newlines_only():
    assert content_tag("a\r\nb") == content_tag("a\nb")
    assert content_tag("a\rb") == content_tag("a\nb")
    # 空白与大小写属于内容，不参与规范化。
    assert content_tag("a b") != content_tag("a  b")
    assert content_tag("A") != content_tag("a")


def test_normalize_newlines_folds_crlf_and_cr():
    assert normalize_newlines("a\r\nb\rc\nd") == "a\nb\nc\nd"
    # 幂等：已经是 LF 的文本原样返回。
    assert normalize_newlines("a\nb\n") == "a\nb\n"


def test_decode_utf8_reports_bom():
    assert decode_utf8("hi".encode("utf-8")) == ("hi", False)
    assert decode_utf8(b"\xef\xbb\xbfhi") == ("hi", True)
    with pytest.raises(UnicodeDecodeError):
        decode_utf8(b"\xff\xfe")


def test_split_lines_matches_upstream_semantics():
    assert split_lines("a\nb\n") == (["a", "b"], True)
    assert split_lines("a\nb") == (["a", "b"], False)
    assert split_lines("") == ([], False)
    assert split_lines("a\n\n") == (["a", ""], True)
    assert split_lines("\n") == ([""], True)


# ── 解析 ──────────────────────────────────────────────────────────────


def test_parse_bare_and_wrapped_payloads():
    bare = parse_payload("[a.py#A1B2]\nPUT 1.=1:\n+x\n")
    wrapped = parse_payload(
        "*** Begin Patch\n[a.py#A1B2]\nPUT 1.=1:\n+x\n*** End Patch\n"
    )
    assert len(bare) == 1 and len(wrapped) == 1
    assert bare[0].path == "a.py"
    assert bare[0].tag == "A1B2"
    assert put_view(wrapped[0]) == put_view(bare[0])


def test_parse_lowercase_tag_normalized_uppercase():
    (section,) = parse_payload("[a.py#a1b2]\nREM\n")
    assert section.tag == "A1B2"


def test_parse_multiple_sections_and_blank_lines_between_ops():
    sections = parse_payload(
        "*** Begin Patch\n"
        "[a.py#1111]\n"
        "PUT 1.=1:\n+A\n"
        "\n"
        "PUT >$:\n+tail\n"
        "[b.py#2222]\n"
        "CUT 2.=3 @keep\n"
        "*** End Patch\n"
    )
    assert [s.path for s in sections] == ["a.py", "b.py"]
    assert len(sections[0].ops) == 2
    (cut,) = [op for op in sections[1].ops if isinstance(op, CutOp)]
    assert cut.register == "keep"


@pytest.mark.parametrize(
    ("payload", "message"),
    [
        ("", "empty patch"),
        ("*** Begin Patch\n*** End Patch\n", "empty patch"),
        ("not a header\n", "section header"),
        ("[a.py#123]\nREM\n", "section header"),
        ("[a.py#A1B2]\nPUT 1.=1:\n", "needs at least one"),
        ("[a.py#A1B2]\n+x\n", "body line without a preceding"),
        ("[a.py#A1B2]\nPUT 1*:\n+A\n", "block locators"),
        ("[a.py#A1B2]\nPUT >1*:\n+A\n", "block locators"),
        ("[a.py#A1B2]\nCUT 1* @x\n", "block locators"),
        ("[a.py#A1B2]\nFROB 1.=1\n", "expected an op"),
        ("[a.py#A1B2]\nCUT 1\n", "CUT needs an explicit range"),
        ("[a.py#A1B2]\nCUT 2.=1 @x\n", "end < start"),
        ("[a.py#A1B2]\nPUT ?:\n+A\n", "invalid locator"),
        ("[a.py#A1B2]\nPUT 2.=1:\n+A\n", "end < start"),
        ("[a.py#A1B2]\nMV\n", "MV needs a destination path"),
        (
            "*** Begin Patch\n[a.py#A1B2]\nREM\n*** End Patch\njunk\n",
            "unexpected content after the patch",
        ),
    ],
)
def test_parse_errors(payload, message):
    with pytest.raises(HashlineError, match=message):
        parse_payload(payload)


def test_parse_paste_forms_split_register():
    (section,) = parse_payload(
        "[a.py#A1B2]\nPUT <1 @head\nPUT >2 @mid\nPUT >$ @tail\nPUT 1.=2 @span\nPUT <1\n"
    )
    assert [(op.register, op.body) for op in section.ops if isinstance(op, PutOp)] == [
        ("head", None),
        ("mid", None),
        ("tail", None),
        ("span", None),
        (None, None),
    ]


# ── 应用 ──────────────────────────────────────────────────────────────


def apply_one(payload: str, content: str, registers=None):
    (section,) = parse_payload(payload)
    return apply_section(section, content, registers or {})


def test_put_range_replace_and_gap_inserts():
    content = "1\n2\n3\n"
    outcome = apply_one(
        "[a.txt#AAAA]\nPUT 2.=2:\n+two\nPUT <1:\n+head\nPUT >$:\n+tail\n", content
    )
    assert outcome.new_content == "head\n1\ntwo\n3\ntail\n"
    assert outcome.ops_applied == 3
    assert (outcome.old_line_count, outcome.new_line_count) == (3, 5)


def test_line_numbers_refer_to_original_snapshot_regardless_of_order():
    content = "1\n2\n3\n4\n"
    # 载荷顺序与位置顺序相反：两个定位子都相对原文。
    outcome = apply_one("[a.txt#AAAA]\nPUT 4.=4:\n+D\nPUT 2.=2:\n+B\n", content)
    assert outcome.new_content == "1\nB\n3\nD\n"


def test_trailing_newline_style_is_preserved():
    result = apply_one("[a#AAAA]\nPUT 1.=1:\n+x\n", "a\n")
    assert result.new_content == "x\n"
    result = apply_one("[a#AAAA]\nPUT 1.=1:\n+x\n", "a")
    assert result.new_content == "x"


def test_body_verbatim_and_escape():
    outcome = apply_one("[a#AAAA]\nPUT 1.=1:\n+\n+- item\n++not a body\n", "gone\n")
    assert outcome.new_content == "\n- item\n+not a body\n"


def test_cut_captures_and_deletes_anonymous_register_pastes_same_payload():
    content = "line1\nline2\nline3\n"
    outcome = apply_one("[a#AAAA]\nCUT 2.=3\nPUT >$:\n+end\nPUT <1\n", content)
    # CUT 捕获 line2/line3；尾部插入 end 后再在头部粘贴匿名寄存器。
    assert outcome.new_content == "line2\nline3\nline1\nend\n"


def test_named_register_capture_and_paste_from_session():
    content = "a\nb\nc\n"
    outcome = apply_one("[x#AAAA]\nCUT 1.=1 @first\n", content)
    assert outcome.named_registers == {"first": ["a"]}
    assert outcome.new_content == "b\nc\n"
    # 命名寄存器来自会话（上一次调用提交的）。
    outcome2 = apply_one("[x#BBBB]\nPUT >$ @first\n", "b\nc\n", {"first": ["a"]})
    assert outcome2.new_content == "b\nc\na\n"


def test_named_register_visible_within_same_section():
    outcome = apply_one("[x#AAAA]\nCUT 1.=1 @x\nPUT >$ @x\n", "a\nb\n")
    assert outcome.new_content == "b\na\n"
    assert outcome.named_registers == {"x": ["a"]}


def test_apply_section_reports_and_accepts_anonymous_register():
    outcome = apply_one("[x#AAAA]\nCUT 1.=1\n", "a\nb\n")
    assert outcome.anonymous_register == ["a"]
    # 调用方可以把匿名寄存器传给下一段（载荷内跨段传递）。
    (section,) = parse_payload("[y#BBBB]\nPUT >$\n")
    outcome2 = apply_section(section, "b\n", {}, anonymous=outcome.anonymous_register)
    assert outcome2.new_content == "b\na\n"


def test_range_paste_requires_named_register():
    # 范围粘贴必须有名称——裸 `PUT N.=M`（少写 ':'）不是覆盖而是报错，避免静默。
    with pytest.raises(HashlineError, match="range paste, which requires @name"):
        apply_one("[x#AAAA]\nPUT 1.=1\n", "a\n")
    with pytest.raises(HashlineError, match="unknown register @nope"):
        apply_one("[x#AAAA]\nPUT 1.=1 @nope\n", "a\n")
    # 间隙粘贴可以是匿名。
    with pytest.raises(HashlineError, match="no anonymous register"):
        apply_one("[x#AAAA]\nPUT <1\n", "a\n")


def test_range_paste_over_lines_with_named_register():
    outcome = apply_one("[x#AAAA]\nPUT 1.=2 @span\n", "a\nb\nc\n", {"span": ["X"]})
    assert outcome.new_content == "X\nc\n"
    assert outcome.ops_applied == 1


def test_rem_must_be_alone_and_marks_removed():
    outcome = apply_one("[x#AAAA]\nREM\n", "a\n")
    assert outcome.removed is True
    with pytest.raises(HashlineError, match="REM must be the only op"):
        apply_one("[x#AAAA]\nREM\nPUT 1.=1:\n+a\n", "a\n")


def test_mv_records_destination_and_quoted_path():
    outcome = apply_one('[x#AAAA]\nMV "sub dir/x y.txt"\n', "a\n")
    assert outcome.moved_to == "sub dir/x y.txt"
    assert outcome.new_content == "a\n"


def test_overlap_and_bounds_rejected():
    content = "1\n2\n3\n"
    with pytest.raises(HashlineError, match="overlapping edits"):
        apply_one("[a#AAAA]\nPUT 1.=2:\n+x\nPUT 2.=3:\n+y\n", content)
    with pytest.raises(HashlineError, match="falls inside the replaced range"):
        apply_one("[a#AAAA]\nPUT 1.=3:\n+x\nPUT >2:\n+y\n", content)
    with pytest.raises(HashlineError, match="out of range"):
        apply_one("[a#AAAA]\nPUT 1.=9:\n+x\n", content)
    with pytest.raises(HashlineError, match="insert position out of range"):
        apply_one("[a#AAAA]\nPUT >9:\n+x\n", content)


def test_delete_everything_yields_empty_file():
    outcome = apply_one("[a#AAAA]\nCUT 1.=2\n", "only\ntwo\n")
    assert outcome.new_content == ""


def test_insert_into_empty_file():
    outcome = apply_one("[a#AAAA]\nPUT <1:\n+first\n", "")
    assert outcome.new_content == "first"


def test_new_tag_reflects_result():
    outcome = apply_one("[a#AAAA]\nPUT 1.=1:\n+new\n", "old\n")
    assert outcome.new_tag == content_tag("new\n")


# ── 窗口 ──────────────────────────────────────────────────────────────


def test_window_replace_span_is_change_plus_context():
    content = "\n".join(f"line {i}" for i in range(1, 11)) + "\n"
    outcome = apply_one("[a#AAAA]\nPUT 6.=6:\n+LINE 6\n", content)
    (window,) = outcome.windows
    assert window.old_text == "\n".join(f"line {i}" for i in range(3, 10))
    expected_new = (
        [f"line {i}" for i in range(3, 6)]
        + ["LINE 6"]
        + [f"line {i}" for i in range(7, 10)]
    )
    assert window.new_text == "\n".join(expected_new)
    assert (window.old_start_line, window.new_start_line) == (3, 3)


def test_window_gap_insert_uses_gap_context():
    content = "\n".join(f"line {i}" for i in range(1, 11)) + "\n"
    outcome = apply_one("[a#AAAA]\nPUT >5:\n+inserted\n", content)
    (window,) = outcome.windows
    # 间隙在第 5 行之后（= 第 6 行之前）：旧侧窗口 = 第 3–8 行。
    assert window.old_text.splitlines() == [f"line {i}" for i in range(3, 9)]
    assert window.new_start_line == 3
    assert "inserted" in window.new_text


def test_window_clips_at_file_edges_and_handles_empty_side():
    outcome = apply_one("[a#AAAA]\nPUT 1.=1:\n+x\n", "only\n")
    (window,) = outcome.windows
    assert window.old_text == "only"
    assert window.new_text == "x"
    assert (window.old_start_line, window.new_start_line) == (1, 1)

    outcome = apply_one("[a#AAAA]\nCUT 1.=1\n", "only\n")
    (window,) = outcome.windows
    assert window.old_text == "only"
    assert window.new_text == ""
    assert (window.old_start_line, window.new_start_line) == (1, 1)


def test_window_context_constant_is_three():
    assert DIFF_CONTEXT_LINES == 3
