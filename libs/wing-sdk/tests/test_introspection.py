"""签名推断与 docstring 解析测试（host.py 内部 helper）。"""

from wing_sdk.host import (
    _extract_description,
    _hint_to_type,
    _infer_params,
    _parse_args_section,
)


def test_hint_basic_types():
    assert _hint_to_type(str) == ("string", None)
    assert _hint_to_type(int) == ("integer", None)
    assert _hint_to_type(float) == ("number", None)
    assert _hint_to_type(bool) == ("boolean", None)
    assert _hint_to_type(dict) == ("object", None)
    assert _hint_to_type(list) == ("array", None)


def test_hint_generic_list():
    assert _hint_to_type(list[str]) == ("array", "string")
    assert _hint_to_type(list[int]) == ("array", "integer")
    # 未知元素类型退化为 string
    assert _hint_to_type(list[object]) == ("array", "string")


def test_hint_generic_dict():
    assert _hint_to_type(dict[str, int]) == ("object", None)


def test_hint_optional_unwrap():
    import typing

    assert _hint_to_type(typing.Optional[str]) == ("string", None)
    assert _hint_to_type(typing.Optional[list[int]]) == ("array", "integer")
    # PEP 604
    assert _hint_to_type(str | None) == ("string", None)
    assert _hint_to_type(int | None) == ("integer", None)


def test_hint_none_and_unknown_fallback():
    assert _hint_to_type(None) == ("string", None)
    assert _hint_to_type(object) == ("string", None)


def test_infer_params_names_types_defaults():
    async def f(command: str, timeout: int = 30, verbose: bool = False):
        """Run it.

        Args:
            command: The command to execute.
            timeout: Max seconds.
        """
        return command, timeout, verbose

    params = _infer_params(f)
    assert [(p.name, p.type) for p in params] == [
        ("command", "string"),
        ("timeout", "integer"),
        ("verbose", "boolean"),
    ]
    assert params[0].default is None
    assert params[1].default == 30
    assert params[0].description == "The command to execute."
    assert params[1].description == "Max seconds."
    assert params[2].description == ""


def test_infer_params_array_items():
    async def f(paths: list[str]):
        """Do."""

    params = _infer_params(f)
    assert params[0].type == "array"
    assert params[0].items == "string"


def test_infer_params_skips_self():
    class C:
        async def method(self, x: int):
            """Do."""

    params = _infer_params(C.method)
    assert [p.name for p in params] == ["x"]


def test_extract_description_first_para():
    async def f():
        """First line.

        Second para ignored.

        Args:
            x: y
        """

    assert _extract_description(f) == "First line."


def test_extract_description_stops_at_args():
    async def f():
        """Summary here.
        Args:
            x: y
        """

    assert _extract_description(f) == "Summary here."


def test_parse_args_section_continuation():
    doc = """Summary.

    Args:
        name: description one
            continuation of one
        other: description two

    Returns:
        something
    """
    result = _parse_args_section(doc)
    assert result["name"] == "description one continuation of one"
    assert result["other"] == "description two"
    assert "something" not in result.values()
