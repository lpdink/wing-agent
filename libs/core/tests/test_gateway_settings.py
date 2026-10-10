"""Setting API 端到端测试 —— 4 端点 + 保存事务 + 事件 + 鉴权。

用**真实** ``WingRuntime``（不是 mock）：本文件要证明的是「保存事务真的改盘、真的回滚、
真的重建 provider / 重挂日志 handler」，mock 掉 runtime 就只剩 route 的胶水。
WING_HOME 指到 tmp（config.yaml 写在那里），sessions 走 conftest 的临时目录。

覆盖任务书点名的三条硬验证（另在最终消息里给了真机 curl 输出）：
密文不回显 / 409 指纹 / 全有或全无。
"""

from __future__ import annotations

import contextlib
import hashlib
from collections.abc import Iterator
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
import yaml
from fastapi.testclient import TestClient

from wing.config import ApiKeyEntry, AuthConfig, Config, reset_config
from wing.event import EVENT_TYPES, FACT_EVENTS, SettingsChangedEvent, wire_dump
from wing.event_bus import event_bus

#: 含一个密文（真实值）与一个可改的数值字段的**合法**配置。
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
images:
  max_bytes: 1000000
"""

ADMIN_KEY = "admin-secret-key-123456"
TOOL_KEY = "tool-runtime-key-123456"

#: reload_system 的逐项名字序（对外契约；第六项是本步骤追加的，R3：只允许追加在末尾）。
RELOAD_ITEMS = [
    "config.yaml",
    "hooks",
    "prompt commands",
    "provider",
    "skills & rules",
    "log level",
]


def _mock_config(
    auth_enabled: bool = False, keys: list[ApiKeyEntry] | None = None
) -> MagicMock:
    """只填 ``gateway.auth`` 的 mock Config（``server.auth_config`` 读它）。"""
    config = MagicMock()
    config.gateway.auth = AuthConfig(enabled=auth_enabled, keys=keys or [])
    return config


@contextlib.contextmanager
def _gateway(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    auth_enabled: bool = False,
    keys: list[ApiKeyEntry] | None = None,
    text: str = VALID_YAML,
) -> Iterator[tuple[TestClient, Path, object]]:
    """真实 runtime + tmp WING_HOME + TestClient（照 test_gateway_http 的既有模式）。"""
    monkeypatch.setenv("WING_HOME", str(tmp_path))
    monkeypatch.setenv("WING_SESSIONS_PATH", str(tmp_path / "sessions"))
    config_path = tmp_path / "core" / "config.yaml"
    config_path.parent.mkdir(parents=True)
    config_path.write_text(text, encoding="utf-8")
    reset_config()

    from wing.gateway.server import GatewayServer

    with patch("wing.gateway.server.load_config") as mock_load_config:
        mock_load_config.return_value = _mock_config(auth_enabled, keys)
        server = GatewayServer()
        with TestClient(server._app) as tc:
            yield tc, config_path, server
    reset_config()


@pytest.fixture
def gateway(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Iterator[tuple[TestClient, Path, object]]:
    with _gateway(tmp_path, monkeypatch) as context:
        yield context


@pytest.fixture
def auth_gateway(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Iterator[tuple[TestClient, Path, object]]:
    with _gateway(
        tmp_path,
        monkeypatch,
        auth_enabled=True,
        keys=[
            ApiKeyEntry(key=ADMIN_KEY, role="admin"),
            ApiKeyEntry(key=TOOL_KEY, role="tool_runtime"),
        ],
    ) as context:
        yield context


#: 两个 provider、**两把不同的 key**（A1 回归的靶子：单 provider / 单 key 结构性看不见错配）。
TWO_PROVIDER_YAML = """\
providers:
  - name: p1
    protocol: openai
    base_url: https://p1.example.com
    api_key: KEY-P1-AAAA
    models: [m1]
  - name: p2
    protocol: openai
    base_url: https://p2.example.com
    api_key: KEY-P2-BBBBBBBB
    models: [m2]
agents:
  - name: default
    model: m2
"""


@pytest.fixture
def two_provider_gateway(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Iterator[tuple[TestClient, Path, object]]:
    with _gateway(tmp_path, monkeypatch, text=TWO_PROVIDER_YAML) as context:
        yield context


def _get_doc(client: TestClient) -> tuple[dict, str]:
    """GET /api/settings/get → (稀疏文档, 指纹)。"""
    resp = client.get("/api/settings/get")
    assert resp.status_code == 200, resp.text
    body = resp.json()
    return body["values"], body["fingerprint"]


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _child(node: dict, key: str) -> dict:
    return next(child for child in node["children"] if child["key"] == key)


def _node(root: dict, *keys: str) -> dict:
    node = root
    for key in keys:
        node = _child(node, key)
    return node


# ============================================================
# GET /api/settings/schema
# ============================================================


class TestSettingsSchema:
    def test_schema_shape_and_root_spelling(self, gateway):
        client, config_path, _ = gateway
        resp = client.get("/api/settings/schema")
        assert resp.status_code == 200, resp.text
        body = resp.json()
        assert body["version"]
        assert body["config_path"] == str(config_path)
        root = body["root"]
        assert root["key"] == "config" and root["path"] == "config"
        assert root["kind"] == "object"

    def test_groups_are_the_navigation_anchors(self, gateway):
        """``groups`` 是前端左列锚点的唯一来源：顺序 / 名字 / 成员都在 wire 上。"""
        client, _, _ = gateway
        body = client.get("/api/settings/schema").json()
        groups = body["groups"]
        assert [g["id"] for g in groups] == [
            "providers",
            "agents",
            "behavior",
            "images",
            "sessions",
            "gateway",
            "advanced",
        ]
        assert [g["title"] for g in groups] == [
            "Providers",
            "Agents",
            "Behavior",
            "Images",
            "Sessions",
            "Gateway",
            "Advanced",
        ]
        by_id = {g["id"]: g for g in groups}
        assert by_id["behavior"]["members"] == [
            "safe_command_patterns",
            "yolo",
            "steer",
            "tool_result_truncate",
        ]
        assert by_id["advanced"]["members"] == [
            "hooks",
            "commands",
            "log",
            "user_agent",
        ]
        assert all(g["doc"] for g in groups)
        # 成员与目录的 section 同源（同一个组名，两个消费口径）。
        root = body["root"]
        for child in root["children"]:
            group = next(g for g in groups if child["key"] in g["members"])
            assert child["section"] == group["title"], child["key"]

    def test_providers_node_carries_declarations(self, gateway):
        client, _, _ = gateway
        root = client.get("/api/settings/schema").json()["root"]
        providers = _node(root, "providers")
        assert providers["kind"] == "list"
        assert providers["required"] is True
        assert providers["min_items"] == 1
        assert providers["summary_fields"] == ["name", "protocol", "base_url"]
        assert providers["section"] == "Providers"

        element = providers["element"]
        assert element["kind"] == "object"
        api_key = _child(element, "api_key")
        assert api_key["kind"] == "secret"
        assert api_key["secret"] is True
        # 密文以外：协议枚举带 choices 含义
        protocol = _child(element, "protocol")
        assert protocol["kind"] == "enum"
        assert [c["value"] for c in protocol["choices"]] == ["openai", "anthropic"]

    def test_models_node_has_variants_not_element(self, gateway):
        client, _, _ = gateway
        root = client.get("/api/settings/schema").json()["root"]
        provider_template = _node(root, "providers")["element"]
        models_node = _child(provider_template, "models")
        assert models_node["kind"] == "list"
        assert models_node["element"] is None
        assert [v["kind"] for v in models_node["variants"]] == ["str", "object"]

    def test_apply_scope_and_array_fields_are_lists(self, gateway):
        client, _, _ = gateway
        root = client.get("/api/settings/schema").json()["root"]
        assert _node(root, "gateway", "port")["apply"] == "restart"
        assert _node(root, "images", "max_bytes")["apply"] == "hot"
        assert _node(root, "log", "level")["apply"] == "hot"
        assert _node(root, "yolo")["apply"] == "next_session"
        # P1 修正：summary_fields 是数组，绝不 null（Rust 是 Vec<String>）
        assert _node(root, "gateway", "port")["summary_fields"] == []
        assert _node(root, "gateway", "port")["value_hint"] is None


# ============================================================
# GET /api/settings/get
# ============================================================


class TestSettingsGet:
    def test_masks_secret_value_and_reports_hint(self, gateway):
        client, config_path, _ = gateway
        resp = client.get("/api/settings/get")
        assert resp.status_code == 200, resp.text
        # 硬验证一：真实密钥绝不出现在响应体里
        assert "sk-super-secret-value-1234" not in resp.text
        assert "super-secret" not in resp.text

        body = resp.json()
        assert body["values"]["providers"][0]["api_key"] is None
        assert body["secrets"] == {
            "providers[0].api_key": {"state": "set", "hint": "1234"}
        }
        assert body["setup_mode"] is False
        assert body["config_path"] == str(config_path)
        assert body["fingerprint"] == _sha256(config_path)
        assert body["problems"] == []

    def test_values_are_sparse(self, gateway):
        client, _, _ = gateway
        values, _ = _get_doc(client)
        # 只有用户显式写下的键（+ 掩码后的 secret 占位），其余跟随默认
        assert set(values) == {"providers", "agents", "images"}
        assert "gateway" not in values

    def test_problems_report_precise_paths(self, gateway):
        client, config_path, _ = gateway
        config_path.write_text(
            VALID_YAML.replace("model: ds-flash", "model: nope"), encoding="utf-8"
        )
        body = client.get("/api/settings/get").json()
        paths = [p["path"] for p in body["problems"]]
        assert paths == ["agents[0].model"]
        assert body["problems"][0]["kind"] == "unknown_reference"
        assert "unknown model id 'nope'" in body["problems"][0]["message"]
        assert body["problems"][0]["hint"]

    def test_broken_file_is_reported_not_500(self, gateway):
        client, config_path, _ = gateway
        config_path.write_text("providers: [unclosed\n", encoding="utf-8")
        resp = client.get("/api/settings/get")
        assert resp.status_code == 200, resp.text
        body = resp.json()
        assert body["values"] == {}
        assert body["fingerprint"] == _sha256(config_path)
        assert body["problems"][0]["path"] is None
        assert body["problems"][0]["kind"] == "invalid_value"


# ============================================================
# GET /api/settings/status
# ============================================================


class TestSettingsStatus:
    def test_valid_config(self, gateway):
        client, config_path, _ = gateway
        body = client.get("/api/settings/status").json()
        assert body == {
            "valid": True,
            "setup_mode": False,
            "problems": [],
            "fingerprint": _sha256(config_path),
        }

    def test_invalid_after_external_edit(self, gateway):
        client, config_path, _ = gateway
        config_path.write_text(
            VALID_YAML.replace(
                "    models:\n      - id: ds-flash\n        name: sonnet",
                "    models: []",
            ),
            encoding="utf-8",
        )
        body = client.get("/api/settings/status").json()
        assert body["valid"] is False
        assert [p["path"] for p in body["problems"]] == ["providers[0].models"]
        assert body["problems"][0]["kind"] == "missing_required"

    def test_broken_file_is_invalid(self, gateway):
        client, config_path, _ = gateway
        config_path.write_text("- not a mapping\n", encoding="utf-8")
        body = client.get("/api/settings/status").json()
        assert body["valid"] is False
        assert body["problems"][0]["path"] is None


# ============================================================
# POST /api/settings/set
# ============================================================


class TestSettingsSet:
    def test_set_writes_file_and_reports(self, gateway):
        client, config_path, _ = gateway
        before = config_path.read_bytes()
        values, fingerprint = _get_doc(client)
        values["images"]["max_bytes"] = 2_000_000

        events: list = []
        event_bus.subscribe(events.append)
        try:
            resp = client.post(
                "/api/settings/set",
                json={"base": fingerprint, "document": values},
            )
        finally:
            event_bus.unsubscribe(events.append)

        assert resp.status_code == 200, resp.text
        body = resp.json()
        assert body["ok"] is True
        assert body["problems"] == []
        assert body["changed"] == ["images.max_bytes"]
        assert body["restart_required"] == []
        assert body["setup_mode_exited"] is False
        assert body["fingerprint"] == _sha256(config_path)
        assert body["fingerprint"] != fingerprint

        # 盘上真的变了 + 密文原值被保留（null 回填）
        text = config_path.read_text(encoding="utf-8")
        assert "max_bytes: 2000000" in text
        assert "sk-super-secret-value-1234" in text

        # ⑤ 备份 = 改前的逐字副本
        backup = config_path.with_name("config.yaml.bak")
        assert body["backup_path"] == str(backup)
        assert backup.read_bytes() == before

        # ⑦ 生效逐项（第六项 log level 是本步骤追加的）
        assert [item["name"] for item in body["reload"]["results"]] == RELOAD_ITEMS
        assert all(item["ok"] for item in body["reload"]["results"])
        assert body["reload"]["ok"] is True

        # ⑨ 事件（global scope；指纹 = 落盘后的新指纹）
        emitted = [e for e in events if isinstance(e, SettingsChangedEvent)]
        assert len(emitted) == 1
        assert emitted[0].changed == ["images.max_bytes"]
        assert emitted[0].fingerprint == body["fingerprint"]
        assert emitted[0].target is not None
        assert emitted[0].target.scope == "global"

    def test_set_reports_restart_required_for_leaves(self, gateway):
        client, _, _ = gateway
        values, fingerprint = _get_doc(client)
        values["gateway"] = {"port": 40000}
        body = client.post(
            "/api/settings/set", json={"base": fingerprint, "document": values}
        ).json()
        assert body["ok"] is True
        assert body["changed"] == ["gateway.port"]
        assert body["restart_required"] == ["gateway.port"]

    def test_set_rejects_invalid_document_without_writing(self, gateway):
        """硬验证三：全有或全无——校验失败时一个字节都不写。"""
        client, config_path, _ = gateway
        before = _sha256(config_path)
        values, fingerprint = _get_doc(client)
        values["images"]["max_bytes"] = -1
        values["agents"][0]["model"] = "nope"

        resp = client.post(
            "/api/settings/set", json={"base": fingerprint, "document": values}
        )
        assert resp.status_code == 200, resp.text
        body = resp.json()
        assert body["ok"] is False
        assert body["fingerprint"] == fingerprint
        assert body["reload"] is None
        assert {(p["path"], p["kind"]) for p in body["problems"]} == {
            ("images.max_bytes", "invalid_value"),
            ("agents[0].model", "unknown_reference"),
        }
        assert _sha256(config_path) == before

    def test_set_rehome_bare_model_name_problem(self, gateway):
        """P9 走完整链路：`models: [""]` 定位到列表级（不猜下标）。"""
        client, _, _ = gateway
        values, fingerprint = _get_doc(client)
        values["providers"][0]["models"] = [""]
        body = client.post(
            "/api/settings/set", json={"base": fingerprint, "document": values}
        ).json()
        assert body["ok"] is False
        assert [p["path"] for p in body["problems"]] == ["providers[0].models"]
        assert body["problems"][0]["message"] == "model name must be non-empty"

    def test_set_conflict_leaves_file_untouched(self, gateway):
        """硬验证二：外部改文件后带旧指纹保存 → 409 且文件未被覆盖。"""
        client, config_path, _ = gateway
        _, fingerprint = _get_doc(client)
        external = VALID_YAML.replace("max_bytes: 1000000", "max_bytes: 424242")
        config_path.write_text(external, encoding="utf-8")

        values = {"images": {"max_bytes": 999}}
        resp = client.post(
            "/api/settings/set", json={"base": fingerprint, "document": values}
        )
        assert resp.status_code == 409, resp.text
        body = resp.json()
        assert body["error"] == "conflict"
        assert _sha256(config_path) == hashlib.sha256(external.encode()).hexdigest()
        assert "424242" in config_path.read_text(encoding="utf-8")

    def test_set_without_base_skips_conflict_check(self, gateway):
        client, config_path, _ = gateway
        values, _ = _get_doc(client)
        values["images"]["max_bytes"] = 22
        resp = client.post("/api/settings/set", json={"base": None, "document": values})
        assert resp.status_code == 200, resp.text
        assert resp.json()["ok"] is True
        assert "max_bytes: 22" in config_path.read_text(encoding="utf-8")

    def test_set_write_failure_is_500(self, gateway):
        client, config_path, _ = gateway
        before = _sha256(config_path)
        values, fingerprint = _get_doc(client)
        values["images"]["max_bytes"] = 33
        with patch(
            "wing.runtime.atomic_write_text",
            side_effect=OSError("disk full"),
        ):
            resp = client.post(
                "/api/settings/set", json={"base": fingerprint, "document": values}
            )
        assert resp.status_code == 500, resp.text
        assert resp.json()["error"] == "internal_error"
        assert _sha256(config_path) == before

    def test_set_requires_document(self, gateway):
        client, _, _ = gateway
        resp = client.post("/api/settings/set", json={"base": None})
        assert resp.status_code == 422
        assert resp.json()["error"] == "validation_error"

    def test_set_keeps_freeform_map_readable_on_disk(self, gateway):
        """AD7（B1 回归）：单键平铺 map 落盘后，config.yaml 仍是**能解析、能加载**的文件。

        `extra_body: {top_p: 0.9}` 是自由 map 最常见的形态；emitter 曾把这种「恰好单行的
        容器载荷」当标量内联，写出 `extra_body: top_p: 0.9` 让整个文件解析失败——用户视角
        就是「我保存了一次，配置就坏了」。这条测试从保存事务一路钉到磁盘字节。
        """
        client, config_path, _ = gateway
        values, fingerprint = _get_doc(client)
        values["providers"][0]["extra_body"] = {"top_p": 0.9}
        body = client.post(
            "/api/settings/set", json={"base": fingerprint, "document": values}
        ).json()
        assert body["ok"] is True

        raw = yaml.safe_load(config_path.read_text(encoding="utf-8"))
        config = Config(**raw)  # 落盘文件必须能被加载期构造（不是"能写不能读"）
        assert config.providers[0].extra_body == {"top_p": 0.9}

    def test_set_keeps_unknown_keys(self, gateway):
        """未知键由服务端回填（D17：新版本写的键，旧版本编辑时不能吃掉）。"""
        client, config_path, _ = gateway
        config_path.write_text(
            VALID_YAML + "\nfuture_section:\n  future_key: keep-me\n", encoding="utf-8"
        )
        values, fingerprint = _get_doc(client)
        values["images"]["max_bytes"] = 44
        body = client.post(
            "/api/settings/set", json={"base": fingerprint, "document": values}
        ).json()
        assert body["ok"] is True
        text = config_path.read_text(encoding="utf-8")
        assert "future_key: keep-me" in text
        assert "unknown key (not recognized by this wing version)" in text


# ============================================================
# A1：密文按身份配对（删除 / 重命名 provider 后的回执形态）
# ============================================================


class TestSettingsSecretPairing:
    """密文 ``null`` 哨兵按身份回填——两把不同 key 的端到端回执。

    单 provider / 单 key 的配置在结构上看不见「A 的密钥被写给 B」，所以这里有两条：
    ① 删除一个 provider（身份配对成功）→ 回执 ``warnings`` 为空、盘上密钥没错配；
    ② 删除 + 改名（配不上）→ 宁可不猜：密钥被移除 → 校验失败（api_key 必填）+
       ``warnings`` 点名路径要求重填。
    """

    def test_delete_a_provider_keeps_the_other_key_without_warnings(
        self, two_provider_gateway
    ):
        client, config_path, _ = two_provider_gateway
        values, fingerprint = _get_doc(client)
        assert [p["name"] for p in values["providers"]] == ["p1", "p2"]
        assert values["providers"][0]["api_key"] is None  # 掩码（get 不回显）
        assert values["providers"][1]["api_key"] is None

        del values["providers"][0]  # 面板的列表删除 / CLI 的 remove
        body = client.post(
            "/api/settings/set", json={"base": fingerprint, "document": values}
        ).json()

        assert body["ok"] is True, body
        assert body["problems"] == []
        # 身份（name=p2）配对成功 → 没有丢任何东西 → 不打扰用户。
        assert body["warnings"] == []
        assert "providers[0].name" in body["changed"]

        written = yaml.safe_load(config_path.read_text(encoding="utf-8"))
        assert [p["name"] for p in written["providers"]] == ["p2"]
        assert written["providers"][0]["api_key"] == "KEY-P2-BBBBBBBB"

    def test_delete_and_rename_reports_the_dropped_key_in_warnings(
        self, two_provider_gateway
    ):
        client, config_path, _ = two_provider_gateway
        before = config_path.read_bytes()
        values, fingerprint = _get_doc(client)
        values["providers"] = [values["providers"][1]]  # 删掉 p1
        values["providers"][0]["name"] = "p2-renamed"  # 同时改名 → 身份配不上

        body = client.post(
            "/api/settings/set", json={"base": fingerprint, "document": values}
        ).json()

        # 宁可不猜：密钥被移除（没有写给任何人）→ 必填字段缺失 → 全有或全无（不写盘）。
        assert body["ok"] is False, body
        assert [p["path"] for p in body["problems"]] == ["providers[0].api_key"]
        assert body["warnings"] == [
            "无法确定 providers[0].api_key 属于哪一项（列表结构变化且无法按身份配对），"
            "已移除该密钥，请重新填写"
        ]
        assert config_path.read_bytes() == before  # 一个字节都没写

    def test_delete_and_append_a_null_item_is_refused_not_hijacked(
        self, two_provider_gateway
    ):
        """删一项 + 同一次保存里加一项（长度相等）：新项不许继承密钥。

        新项的 ``null`` 哨兵落在**已被 p2 认领**的槽位上——(b) 的 consumed 守卫必须让它
        落「不猜」：不许抄走 p2 的密钥（旧行为：`p3` 静默继承 `KEY-P2-BBBBBBBB`），
        回执点名 + 全有或全无。
        """
        client, config_path, _ = two_provider_gateway
        before = config_path.read_bytes()
        values, fingerprint = _get_doc(client)
        del values["providers"][0]  # 删 p1
        values["providers"].append(  # 同一次保存里追加新项（api_key 是 null 哨兵）
            {
                "name": "p3",
                "protocol": "openai",
                "base_url": "https://p3.example.com",
                "api_key": None,
                "models": ["m3"],
            }
        )
        body = client.post(
            "/api/settings/set", json={"base": fingerprint, "document": values}
        ).json()

        assert body["ok"] is False, body
        assert [p["path"] for p in body["problems"]] == ["providers[1].api_key"]
        assert body["warnings"] == [
            "无法确定 providers[1].api_key 属于哪一项（列表结构变化且无法按身份配对），"
            "已移除该密钥，请重新填写"
        ]
        assert config_path.read_bytes() == before

    def test_whole_list_replacement_reports_the_positional_keys(
        self, two_provider_gateway
    ):
        """整表替换（长度相等）：consumed 守卫关不掉，回执必须**出声**（AD18 第 2 点）。

        两条警告各点名一个路径，并把两种读法（重命名 / 替换成新的）都写出来——
        这既是 N1 残余的处置，也是「保留 (b) 而不是撤掉它」的代价说明。
        """
        client, config_path, _ = two_provider_gateway
        values, fingerprint = _get_doc(client)
        values["providers"] = [
            {**values["providers"][0], "name": "p3"},
            {**values["providers"][1], "name": "p4"},
        ]
        body = client.post(
            "/api/settings/set", json={"base": fingerprint, "document": values}
        ).json()

        assert body["ok"] is True, body
        assert body["warnings"] == [
            "providers[0].api_key 按位置保留了磁盘上的值"
            "（该项的 name 与磁盘上的项不一致）——若这是重命名，无需处理；"
            "若是替换成了新的项，请重新填写该密钥",
            "providers[1].api_key 按位置保留了磁盘上的值"
            "（该项的 name 与磁盘上的项不一致）——若这是重命名，无需处理；"
            "若是替换成了新的项，请重新填写该密钥",
        ]
        written = yaml.safe_load(config_path.read_text(encoding="utf-8"))
        assert [p["name"] for p in written["providers"]] == ["p3", "p4"]
        # 位置保留把两把 key 按原下标一起搬了过来（旧值 = 新值，`changed` 看不见它们）。
        assert [p["api_key"] for p in written["providers"]] == [
            "KEY-P1-AAAA",
            "KEY-P2-BBBBBBBB",
        ]

    def test_rename_reports_the_positional_key_and_keeps_it(self, two_provider_gateway):
        """重命名（长度相等、无增删）：密钥按位置保留 + 一条「按位置保留」的说明。"""
        client, config_path, _ = two_provider_gateway
        values, fingerprint = _get_doc(client)
        values["providers"][1]["name"] = "p2-renamed"
        body = client.post(
            "/api/settings/set", json={"base": fingerprint, "document": values}
        ).json()

        assert body["ok"] is True, body
        assert body["warnings"] == [
            "providers[1].api_key 按位置保留了磁盘上的值"
            "（该项的 name 与磁盘上的项不一致）——若这是重命名，无需处理；"
            "若是替换成了新的项，请重新填写该密钥"
        ]
        written = yaml.safe_load(config_path.read_text(encoding="utf-8"))
        assert [p["name"] for p in written["providers"]] == ["p1", "p2-renamed"]
        assert written["providers"][1]["api_key"] == "KEY-P2-BBBBBBBB"


# ============================================================
# 鉴权（写端点要求 admin —— 由既有 RBAC 天然满足，见 design.md D9）
# ============================================================


class TestSettingsAuth:
    def test_read_requires_key_when_enabled(self, auth_gateway):
        client, _, _ = auth_gateway
        assert client.get("/api/settings/get").status_code == 401
        assert client.get("/api/settings/status").status_code == 401
        assert client.get("/api/settings/schema").status_code == 401
        assert (
            client.get(
                "/api/settings/get", headers={"X-API-Key": ADMIN_KEY}
            ).status_code
            == 200
        )

    def test_tool_runtime_role_is_denied(self, auth_gateway):
        client, _, _ = auth_gateway
        headers = {"X-API-Key": TOOL_KEY}
        assert client.get("/api/settings/get", headers=headers).status_code == 403
        resp = client.post(
            "/api/settings/set", json={"base": None, "document": {}}, headers=headers
        )
        assert resp.status_code == 403

    def test_admin_role_can_save(self, auth_gateway):
        client, config_path, _ = auth_gateway
        headers = {"X-API-Key": ADMIN_KEY}
        resp = client.get("/api/settings/get", headers=headers)
        values, fingerprint = resp.json()["values"], resp.json()["fingerprint"]
        values["images"]["max_bytes"] = 55
        saved = client.post(
            "/api/settings/set",
            json={"base": fingerprint, "document": values},
            headers=headers,
        )
        assert saved.status_code == 200, saved.text
        assert saved.json()["ok"] is True
        assert "max_bytes: 55" in config_path.read_text(encoding="utf-8")


# ============================================================
# reload_system 的逐项名字序（R3：追加在末尾，既有五项一字不动）
# ============================================================


class TestReloadSystemContract:
    def test_reload_reports_six_items_in_order(self, gateway):
        client, _, _ = gateway
        resp = client.post("/api/system/reload")
        assert resp.status_code == 200, resp.text
        body = resp.json()
        assert body["ok"] is True
        assert [item["name"] for item in body["results"]] == RELOAD_ITEMS
        # 既有五项的 ok 与顺序都不许变
        assert [item["ok"] for item in body["results"][:5]] == [True] * 5
        assert body["results"][3]["detail"] == "rebuilt 1 provider(s)"


# ============================================================
# SettingsChangedEvent
# ============================================================


class TestSettingsEvent:
    def test_registered_in_event_types(self):
        assert EVENT_TYPES["settings_changed"] is SettingsChangedEvent

    def test_not_a_fact_event(self):
        """它是网关级时点通知，不进任何会话链，也不进 resume 重放。"""
        assert SettingsChangedEvent.persist is False
        assert "settings_changed" not in FACT_EVENTS

    def test_wire_dump_shape(self):
        event = SettingsChangedEvent(
            changed=["gateway.port", "images.max_bytes"],
            restart_required=["gateway.port"],
            fingerprint="abc123",
        )
        data = wire_dump(event)
        assert data["type"] == "settings_changed"
        assert data["changed"] == ["gateway.port", "images.max_bytes"]
        assert data["restart_required"] == ["gateway.port"]
        assert data["setup_mode_exited"] is False
        assert data["fingerprint"] == "abc123"
        # 存储 / 传输元数据不进帧
        assert "target" not in data
        assert "role" not in data
        assert "parent_uuid" not in data

    def test_setup_mode_exited_can_be_serialized_true(self):
        event = SettingsChangedEvent(
            changed=[], restart_required=[], setup_mode_exited=True, fingerprint="x"
        )
        assert wire_dump(event)["setup_mode_exited"] is True
