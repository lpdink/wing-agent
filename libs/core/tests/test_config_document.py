"""``config/document.py`` 单测 —— 稀疏读取 / 指纹 / 密文三态 / merge / diff / 问题定位。

覆盖两条设计红线：

- **密文三态**（总设计 §7.5）：`null` = 保留现值 / 字符串 = 设置 / 缺席 = 不覆盖；
  hint 只给末 4 位且长度 ≥ 8 才给。
- **P9**（协议增补）：裸字符串形态的调用名非法时，pydantic 的 loc 记成
  ``providers[i].name``——``locate_problems`` 必须归到 ``providers[i].models``（列表级，
  **不许猜具体下标**），且不能把「provider 名非法」误归到 models。
"""

from __future__ import annotations

import hashlib
import dataclasses
from pathlib import Path

import pytest

from wing.config import build_catalog
from wing.config.catalog import Element, Index, Key
from wing.config.document import (
    ABSENT_FINGERPRINT,
    ConfigDocumentError,
    SecretState,
    SparseDocument,
    changed_paths,
    locate_problems,
    mask_secrets,
    merge_with_defaults,
    node_at_path,
    read_document,
    resolve_secrets,
    restart_required_paths,
    secret_states,
)
from wing.config.problems import ProblemKind

VALID_YAML = """\
providers:
  - name: local
    base_url: https://api.example.com
    api_key: sk-super-secret-value-1234
    models:
      - id: ds-flash
        name: sonnet
agents:
  - name: default
    model: ds-flash
"""


@pytest.fixture
def config_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """把 ``document`` 的读取口钉到 tmp 文件（不碰真实 WING_HOME）。"""
    path = tmp_path / "core" / "config.yaml"
    path.parent.mkdir(parents=True)
    monkeypatch.setattr("wing.config.document.get_config_path", lambda: path)
    return path


def _catalog():
    return build_catalog()


def _doc(**kwargs) -> SparseDocument:
    return SparseDocument(**kwargs)


# ============================================================
# 读取与指纹
# ============================================================


class TestReadDocument:
    def test_missing_file_is_absent(self, config_path: Path):
        doc, fingerprint = read_document()
        assert doc.data == {}
        assert doc.extra == []
        assert fingerprint.value == ABSENT_FINGERPRINT

    def test_fingerprint_matches_file_bytes(self, config_path: Path):
        config_path.write_text(VALID_YAML, encoding="utf-8")
        _, fingerprint = read_document()
        expected = hashlib.sha256(config_path.read_bytes()).hexdigest()
        assert fingerprint.value == expected

    def test_fingerprint_changes_with_content(self, config_path: Path):
        config_path.write_text(VALID_YAML, encoding="utf-8")
        _, before = read_document()
        config_path.write_text(VALID_YAML + "\nimages:\n  max_bytes: 42\n", "utf-8")
        _, after = read_document()
        assert before.value != after.value

    def test_same_content_same_fingerprint(self, config_path: Path):
        config_path.write_text(VALID_YAML, encoding="utf-8")
        _, first = read_document()
        _, second = read_document()
        assert first.value == second.value

    def test_empty_file_is_empty_document(self, config_path: Path):
        config_path.write_text("", encoding="utf-8")
        doc, _ = read_document()
        assert doc.data == {}

    def test_sparse_document_keeps_only_written_keys(self, config_path: Path):
        config_path.write_text(VALID_YAML, encoding="utf-8")
        doc, _ = read_document()
        assert set(doc.data) == {"providers", "agents"}
        assert doc.data["providers"][0]["name"] == "local"

    def test_unknown_keys_go_to_extra(self, config_path: Path):
        config_path.write_text(
            "future_root: 1\n"
            "gateway:\n"
            "  port: 40000\n"
            "  future_key: hello\n"
            "providers:\n"
            "  - name: local\n"
            "    base_url: https://x\n"
            "    api_key: k\n"
            "    models: [m]\n"
            "    future_provider_key: [1, 2]\n"
            "agents: [{name: a, model: m}]\n",
            encoding="utf-8",
        )
        doc, _ = read_document()
        assert doc.data["gateway"] == {"port": 40000}
        assert sorted((path, repr(value)) for path, value in doc.extra) == sorted(
            [
                ("future_root", repr(1)),
                ("gateway.future_key", repr("hello")),
                ("providers[0].future_provider_key", repr([1, 2])),
            ]
        )

    def test_freeform_map_keys_are_not_unknown(self, config_path: Path):
        config_path.write_text(
            "providers:\n"
            "  - name: local\n"
            "    base_url: https://x\n"
            "    api_key: k\n"
            "    models: [m]\n"
            "    extra_body:\n"
            "      thinking: {type: enabled}\n"
            "agents: [{name: a, model: m}]\n",
            encoding="utf-8",
        )
        doc, _ = read_document()
        assert doc.extra == []
        assert doc.data["providers"][0]["extra_body"] == {
            "thinking": {"type": "enabled"}
        }

    def test_union_variants_are_walked(self, config_path: Path):
        """``providers[].models`` 的元素是 union：对象形态按 ModelSpec 走。"""
        config_path.write_text(
            "providers:\n"
            "  - name: local\n"
            "    base_url: https://x\n"
            "    api_key: k\n"
            "    models:\n"
            "      - name: m\n"
            "        future_spec_key: 7\n"
            "      - bare\n"
            "agents: [{name: a, model: bare}]\n",
            encoding="utf-8",
        )
        doc, _ = read_document()
        assert doc.extra == [("providers[0].models[0].future_spec_key", 7)]

    def test_yaml_syntax_error_carries_fingerprint(self, config_path: Path):
        config_path.write_text("providers: [unclosed\n", encoding="utf-8")
        with pytest.raises(ConfigDocumentError) as excinfo:
            read_document()
        expected = hashlib.sha256(config_path.read_bytes()).hexdigest()
        assert excinfo.value.fingerprint == expected
        problem = excinfo.value.as_problem()
        assert problem.path is None
        assert problem.kind is ProblemKind.INVALID_VALUE
        assert "YAML" in problem.message

    def test_non_mapping_toplevel_is_error(self, config_path: Path):
        config_path.write_text("- 1\n- 2\n", encoding="utf-8")
        with pytest.raises(ConfigDocumentError) as excinfo:
            read_document()
        assert "顶层必须是映射" in str(excinfo.value)


# ============================================================
# merge_with_defaults
# ============================================================


class TestMergeWithDefaults:
    def test_fills_object_and_scalar_defaults(self):
        merged = merge_with_defaults(_doc(data={}), _catalog())
        assert merged["gateway"]["host"] == "127.0.0.1"
        assert merged["gateway"]["port"] == 32523
        assert merged["gateway"]["auth"]["enabled"] is False
        assert merged["images"]["max_bytes"] == 4_718_592
        assert merged["log"]["level"] == "WARNING"
        assert merged["hooks"] == []

    def test_keeps_explicit_values(self):
        merged = merge_with_defaults(
            _doc(data={"gateway": {"port": 40000}, "images": {"max_bytes": 7}}),
            _catalog(),
        )
        assert merged["gateway"]["port"] == 40000
        assert merged["gateway"]["host"] == "127.0.0.1"  # 同对象的缺席键照补
        assert merged["images"]["max_bytes"] == 7

    def test_missing_required_stays_missing(self):
        """必填但缺席**不会**被补成有值——那正是要报的错（pydantic 的 Field required）。"""
        merged = merge_with_defaults(_doc(data={}), _catalog())
        assert "providers" not in merged
        assert "agents" not in merged

    def test_list_items_get_object_defaults(self):
        merged = merge_with_defaults(
            _doc(data={"providers": [{"name": "p", "models": ["m"]}]}), _catalog()
        )
        provider = merged["providers"][0]
        assert provider["protocol"] == "openai"
        assert provider["timeout_first_chunk"] == 300.0
        assert "api_key" not in provider  # 必填缺席照旧缺席
        assert provider["models"] == ["m"]  # 裸字符串形态原样保留


# ============================================================
# 密文三态
# ============================================================


class TestResolveSecrets:
    def _current(self, api_key: object = ...) -> SparseDocument:
        provider: dict = {"name": "p"}
        if api_key is not ...:  # 省略键 = 当前文档里根本没有这个 secret
            provider["api_key"] = api_key
        return _doc(
            data={
                "providers": [provider],
                "gateway": {
                    "auth": {"keys": [{"key": "auth-secret-123", "role": "admin"}]}
                },
            }
        )

    def test_null_keeps_current_value(self):
        incoming = _doc(data={"providers": [{"name": "p", "api_key": None}]})
        resolved = resolve_secrets(incoming, self._current("disk-secret"), _catalog())
        assert resolved.data["providers"][0]["api_key"] == "disk-secret"

    def test_null_with_no_current_value_removes_key(self):
        incoming = _doc(data={"providers": [{"name": "p", "api_key": None}]})
        resolved = resolve_secrets(incoming, self._current(), _catalog())
        assert "api_key" not in resolved.data["providers"][0]

    def test_string_is_set(self):
        incoming = _doc(data={"providers": [{"name": "p", "api_key": "new"}]})
        resolved = resolve_secrets(incoming, self._current("old"), _catalog())
        assert resolved.data["providers"][0]["api_key"] == "new"

    def test_empty_string_is_explicit_clear(self):
        incoming = _doc(data={"providers": [{"name": "p", "api_key": ""}]})
        resolved = resolve_secrets(incoming, self._current("old"), _catalog())
        assert resolved.data["providers"][0]["api_key"] == ""

    def test_absence_is_not_overridden(self):
        """键缺席 = 该项不被覆盖（从文件移除）——绝不偷偷从 current 补一个回来。"""
        incoming = _doc(data={"providers": [{"name": "p"}]})
        resolved = resolve_secrets(incoming, self._current("disk-secret"), _catalog())
        assert "api_key" not in resolved.data["providers"][0]

    def test_nested_auth_keys_null_keeps_current(self):
        incoming = _doc(
            data={"gateway": {"auth": {"keys": [{"key": None, "role": "admin"}]}}}
        )
        resolved = resolve_secrets(incoming, self._current("x"), _catalog())
        assert resolved.data["gateway"]["auth"]["keys"][0]["key"] == "auth-secret-123"

    def test_non_secret_null_is_untouched(self):
        """``null`` 只在 secret 叶子上是「保留」哨兵；别处的 null 是用户写下的值。"""
        incoming = _doc(
            data={
                "providers": [{"name": "p", "api_key": None, "reasoning_effort": None}]
            }
        )
        resolved = resolve_secrets(incoming, self._current("disk"), _catalog())
        assert resolved.data["providers"][0]["reasoning_effort"] is None

    def test_inputs_are_not_mutated(self):
        current = self._current("disk-secret")
        incoming = _doc(data={"providers": [{"name": "p", "api_key": None}]})
        resolve_secrets(incoming, current, _catalog())
        assert incoming.data["providers"][0]["api_key"] is None
        assert current.data["providers"][0]["api_key"] == "disk-secret"

    def test_index_mismatch_falls_back_to_absent(self):
        """incoming 的下标在 current 里不存在时按「当前也没有值」处理（宁可报错不猜值）。"""
        incoming = _doc(
            data={
                "providers": [
                    {"name": "a", "api_key": None},
                    {"name": "b", "api_key": None},
                ]
            }
        )
        resolved = resolve_secrets(incoming, self._current("disk"), _catalog())
        assert resolved.data["providers"][0]["api_key"] == "disk"
        assert "api_key" not in resolved.data["providers"][1]


class TestSecretStates:
    def test_set_with_hint(self):
        doc = _doc(data={"providers": [{"api_key": "sk-super-secret-value-1234"}]})
        states = secret_states(doc, _catalog())
        assert states["providers[0].api_key"].state == "set"
        assert states["providers[0].api_key"].hint == "1234"

    def test_short_value_has_no_hint(self):
        doc = _doc(data={"providers": [{"api_key": "short"}]})
        states = secret_states(doc, _catalog())
        assert states["providers[0].api_key"].state == "set"
        assert states["providers[0].api_key"].hint is None

    def test_seven_chars_has_no_hint_eight_has(self):
        seven = _doc(data={"providers": [{"api_key": "1234567"}]})
        eight = _doc(data={"providers": [{"api_key": "12345678"}]})
        assert secret_states(seven, _catalog())["providers[0].api_key"].hint is None
        assert secret_states(eight, _catalog())["providers[0].api_key"].hint == "5678"

    def test_empty_and_absent(self):
        doc = _doc(data={"providers": [{"api_key": ""}, {"name": "p2"}]})
        states = secret_states(doc, _catalog())
        assert states["providers[0].api_key"].state == "empty"
        assert states["providers[0].api_key"].hint is None
        assert states["providers[1].api_key"].state == "absent"

    def test_no_entries_without_list_items(self):
        assert secret_states(_doc(data={}), _catalog()) == {}
        assert secret_states(_doc(data={"providers": []}), _catalog()) == {}

    def test_nested_auth_keys(self):
        doc = _doc(data={"gateway": {"auth": {"keys": [{"key": "abcdefgh"}]}}})
        states = secret_states(doc, _catalog())
        assert states == {
            "gateway.auth.keys[0].key": states["gateway.auth.keys[0].key"]
        }
        assert states["gateway.auth.keys[0].key"].state == "set"
        assert states["gateway.auth.keys[0].key"].hint == "efgh"

    def test_null_state_is_empty(self):
        """手写 ``api_key: null``：键在文件里但没有可用值 → empty（不是 set）。"""
        doc = _doc(data={"providers": [{"api_key": None}]})
        assert secret_states(doc, _catalog())["providers[0].api_key"].state == "empty"


class TestMaskSecrets:
    def test_present_secrets_become_null(self):
        doc = _doc(
            data={
                "providers": [{"name": "p", "api_key": "sk-realsecret"}],
                "gateway": {"auth": {"keys": [{"key": "authsecret", "role": "admin"}]}},
            }
        )
        masked = mask_secrets(doc, _catalog())
        assert masked["providers"][0]["api_key"] is None
        assert masked["providers"][0]["name"] == "p"
        assert masked["gateway"]["auth"]["keys"][0]["key"] is None
        assert masked["gateway"]["auth"]["keys"][0]["role"] == "admin"
        # 原文档不动
        assert doc.data["providers"][0]["api_key"] == "sk-realsecret"

    def test_absent_secret_stays_absent(self):
        masked = mask_secrets(_doc(data={"providers": [{"name": "p"}]}), _catalog())
        assert "api_key" not in masked["providers"][0]

    def test_empty_string_becomes_null_too(self):
        masked = mask_secrets(_doc(data={"providers": [{"api_key": ""}]}), _catalog())
        assert masked["providers"][0]["api_key"] is None


# ============================================================
# changed_paths
# ============================================================


class TestChangedPaths:
    def _changed(self, old: dict, new: dict) -> list[str]:
        return changed_paths(_doc(data=old), _doc(data=new), _catalog())

    def test_scalar_modified(self):
        assert self._changed(
            {"images": {"max_bytes": 1}}, {"images": {"max_bytes": 2}}
        ) == ["images.max_bytes"]

    def test_added_and_removed(self):
        assert self._changed({}, {"images": {"max_bytes": 1}}) == ["images.max_bytes"]
        assert self._changed({"images": {"max_bytes": 1}}, {}) == ["images.max_bytes"]

    def test_identical_documents_have_no_changes(self):
        doc = {"providers": [{"name": "p"}], "images": {"max_bytes": 1}}
        assert self._changed(doc, doc) == []

    def test_absent_equals_empty_container(self):
        assert self._changed({}, {"images": {}}) == []
        assert self._changed({}, {"hooks": []}) == []
        assert self._changed({"hooks": ["a"]}, {"hooks": ["a"]}) == []

    def test_nested_list_index_change(self):
        old = {"providers": [{"models": [{"name": "a"}, {"name": "b"}]}]}
        new = {"providers": [{"models": [{"name": "a"}, {"name": "c"}]}]}
        assert self._changed(old, new) == ["providers[0].models[1].name"]

    def test_scalar_list_item_change(self):
        assert self._changed({"hooks": ["a", "b"]}, {"hooks": ["a", "c"]}) == [
            "hooks[1]"
        ]

    def test_list_delete_shifts_indices(self):
        """按下标 diff（设计决定）：删除第一项会让后续项逐个记成 modified。"""
        assert self._changed({"hooks": ["a", "b"]}, {"hooks": ["b"]}) == [
            "hooks[0]",
            "hooks[1]",
        ]

    def test_list_grow_reports_added_index(self):
        assert self._changed({"hooks": ["a"]}, {"hooks": ["a", "b"]}) == ["hooks[1]"]

    def test_union_variant_item_walks_object_fields(self):
        old = {"providers": [{"models": [{"name": "m", "display_name": "one"}]}]}
        new = {"providers": [{"models": [{"name": "m", "display_name": "two"}]}]}
        assert self._changed(old, new) == ["providers[0].models[0].display_name"]

    def test_type_sensitive(self):
        """``True != 1``、``300 != 300.0``——盘上的字节确实变了。"""
        assert self._changed({"yolo": True}, {"yolo": 1}) == ["yolo"]
        assert self._changed(
            {"gateway": {"remote_tool_timeout": 300}},
            {"gateway": {"remote_tool_timeout": 300.0}},
        ) == ["gateway.remote_tool_timeout"]

    def test_structural_mismatch_reports_node(self):
        assert self._changed({"gateway": 3}, {"gateway": {"port": 1}}) == ["gateway"]
        assert self._changed({"hooks": "x"}, {"hooks": ["a"]}) == ["hooks"]

    def test_bool_default_not_confused(self):
        assert self._changed({"yolo": False}, {"yolo": False}) == []


# ============================================================
# locate_problems
# ============================================================


def _provider(**kwargs) -> dict:
    fields: dict = {
        "name": "p",
        "base_url": "https://x",
        "api_key": "k",
        "models": ["m"],
    }
    fields.update(kwargs)
    return fields


def _raw(providers: list | None = None, agents: list | None = None, **extra) -> dict:
    raw: dict = {
        "providers": providers if providers is not None else [_provider()],
        "agents": agents if agents is not None else [{"name": "a", "model": "m"}],
    }
    raw.update(extra)
    return raw


class TestLocateProblems:
    def test_valid_document_has_no_problems(self):
        assert locate_problems(_raw()) == []

    def test_bare_empty_model_name_rehomed_to_list_level(self):
        """P9：``models: [""]`` 的 loc 是 (providers, 0, name) → 归到 models（列表级）。"""
        problems = locate_problems(_raw(providers=[_provider(models=[""])]))
        assert len(problems) == 1
        assert problems[0].path == "providers[0].models"
        assert problems[0].kind is ProblemKind.INVALID_VALUE
        assert problems[0].message == "model name must be non-empty"

    def test_bare_blank_model_name_rehomed_to_list_level(self):
        problems = locate_problems(_raw(providers=[_provider(models=[" m "])]))
        assert [p.path for p in problems] == ["providers[0].models"]
        assert "leading/trailing whitespace" in problems[0].message

    def test_invalid_provider_name_keeps_its_own_path(self):
        """provider 名非法与模型名非法共享同一个 loc——不许误归到 models。"""
        problems = locate_problems(_raw(providers=[_provider(name="bad name!")]))
        assert [p.path for p in problems] == ["providers[0].name"]
        assert "provider name must match" in problems[0].message

    def test_both_bad_still_points_at_provider_name(self):
        """两者同时非法：pydantic 只报 provider 名那条（loc 相同）——路径必须指向它。"""
        problems = locate_problems(
            _raw(providers=[_provider(name="bad name!", models=[""])])
        )
        assert [p.path for p in problems] == ["providers[0].name"]
        assert "provider name must match" in problems[0].message

    def test_union_member_tag_is_not_a_path_segment(self):
        problems = locate_problems(_raw(providers=[_provider(models=[123])]))
        assert [p.path for p in problems] == [
            "providers[0].models[0]",
            "providers[0].models[0]",
        ]
        assert all("valid" in p.message for p in problems)

    def test_nested_object_field_path(self):
        problems = locate_problems(
            _raw(
                providers=[
                    _provider(models=[{"name": "m", "capabilities": {"vision": "x"}}])
                ],
                agents=[{"name": "a", "model": "m"}],
            )
        )
        assert [p.path for p in problems] == [
            "providers[0].models[0].capabilities.vision"
        ]
        assert problems[0].kind is ProblemKind.INVALID_VALUE

    def test_object_model_field_path(self):
        problems = locate_problems(_raw(providers=[_provider(models=[{"name": "  "}])]))
        assert [p.path for p in problems] == ["providers[0].models[0].name"]

    def test_missing_required_field(self):
        problems = locate_problems({"providers": [_provider()]})
        assert [(p.path, p.kind) for p in problems] == [
            ("agents", ProblemKind.MISSING_REQUIRED)
        ]

    def test_document_level_model_errors_are_dropped(self):
        """模型级校验器（loc=()）不在这里报——cross_field_problems 给精确路径版本。"""
        assert locate_problems(_raw(providers=[])) == []
        assert locate_problems(_raw(agents=[])) == []
        assert locate_problems(_raw(agents=[{"name": "a", "model": "nope"}])) == []

    def test_wrong_type_paths(self):
        problems = locate_problems({"providers": "nope", "agents": []})
        assert [(p.path, p.kind) for p in problems] == [
            ("providers", ProblemKind.INVALID_VALUE)
        ]
        problems = locate_problems(
            _raw(providers=[_provider()], agents=[{"name": "a", "model": "m"}], log=3)
        )
        assert [p.path for p in problems] == ["log"]

    def test_literal_and_range_errors_keep_path(self):
        problems = locate_problems(_raw(providers=[_provider(protocol="grpc")]))
        assert [p.path for p in problems] == ["providers[0].protocol"]
        problems = locate_problems(
            _raw(providers=[_provider(image_max_bytes=0)], images={"max_bytes": 0})
        )
        assert [p.path for p in problems] == [
            "providers[0].image_max_bytes",
            "images.max_bytes",
        ]


# ============================================================
# node_at_path / restart_required_paths
# ============================================================


class TestNodeAtPath:
    def test_root_itself(self):
        catalog = _catalog()
        assert node_at_path(catalog, "config") is catalog

    def test_root_prefix_is_optional(self):
        catalog = _catalog()
        prefixed = node_at_path(catalog, "config.gateway.port")
        plain = node_at_path(catalog, "gateway.port")
        assert prefixed is not None and prefixed.path == "gateway.port"
        assert plain is prefixed

    def test_element_and_index_address_the_same_template(self):
        catalog = _catalog()
        template = node_at_path(catalog, "providers[].api_key")
        assert template is not None and template.path == "providers[].api_key"
        assert node_at_path(catalog, "providers[7].api_key") is template

    def test_unknown_paths_return_none(self):
        catalog = _catalog()
        assert node_at_path(catalog, "nope") is None
        assert node_at_path(catalog, "gateway.nope") is None
        assert node_at_path(catalog, "") is None
        assert node_at_path(catalog, "gateway..port") is None

    def test_variants_index_is_not_addressable(self):
        """``providers[].models`` 的元素是 union：没有文档值就选不出形态（Rust 同口径）。"""
        catalog = _catalog()
        assert node_at_path(catalog, "providers[0].models[0]") is None


class TestRestartRequiredPaths:
    def test_restart_leaf_is_reported(self):
        assert restart_required_paths(["gateway.port"], _catalog()) == ["gateway.port"]

    def test_hot_leaf_is_not_reported(self):
        assert restart_required_paths(["images.max_bytes"], _catalog()) == []

    def test_next_session_leaf_is_not_reported(self):
        assert restart_required_paths(["yolo"], _catalog()) == []

    def test_container_path_is_never_reported(self):
        """P13：容器节点的 apply 是子树最粗一档（sessions=restart），但判定只看叶子。"""
        catalog = _catalog()
        sessions = node_at_path(catalog, "sessions")
        assert sessions is not None and sessions.apply.value == "restart"
        assert restart_required_paths(["sessions"], catalog) == []
        assert restart_required_paths(["gateway", "providers[0]"], catalog) == []

    def test_leaf_under_list_index(self):
        assert restart_required_paths(["providers[0].models[0].id"], _catalog()) == []
        assert restart_required_paths(["providers[0].name"], _catalog()) == []


# ============================================================
# 路径文法（三变体，与 02 的 parse_path 同一口径）
# ============================================================


def test_parse_path_has_three_variants():
    from wing.config import parse_path

    assert parse_path("a.b[0].c") == [Key("a"), Key("b"), Index(0), Key("c")]
    assert parse_path("providers[].api_key") == [
        Key("providers"),
        Element(),
        Key("api_key"),
    ]
    assert parse_path("nope[") is None


def test_secret_state_is_frozen_dataclass():
    """密文状态是不可变值对象（防「顺手改一下」把状态表变成可变共享态）。"""
    state = SecretState(state="set", hint="1234")
    with pytest.raises(dataclasses.FrozenInstanceError):
        setattr(state, "hint", "0000")
