"""启动读取的磁盘异常面：模板写不出、文件读不了、endpoint 解析兜底。

`boot_config()` 的契约是"永不抛"——磁盘满 / 只读挂载 / 权限不足都只能降级成
一个带 reason 的 BootResult，让网关还能起来给用户修（见 docs/dev/settings.md）。
"""

from __future__ import annotations

from pathlib import Path

import pytest

from wing.config import boot as boot_mod
from wing.config.boot import BootFailure, boot_config
from wing.config.loader import reset_config


@pytest.fixture(autouse=True)
def _fresh_loader():
    reset_config()
    yield
    reset_config()


def _wing_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.setenv("WING_HOME", str(tmp_path))
    return tmp_path / "core" / "config.yaml"


class TestTemplateWriteFailure:
    def test_unwritable_home_reports_missing_file(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        monkeypatch.setenv("WING_HOME", str(tmp_path))

        def _denied(path: Path, text: str) -> None:
            raise OSError(30, "Read-only file system")

        monkeypatch.setattr(boot_mod, "atomic_write_text", _denied)

        result = boot_config()

        assert result.ok is False
        assert result.reason is BootFailure.MISSING_FILE
        assert "模板也写不出来" in result.problems[0].message


class TestUnreadableConfig:
    def test_read_failure_reports_parse_error(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        path = _wing_home(tmp_path, monkeypatch)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("providers: []\n", encoding="utf-8")

        original = Path.read_bytes

        def _denied(self: Path) -> bytes:
            if self.name == "config.yaml":
                raise OSError(5, "Input/output error")
            return original(self)

        monkeypatch.setattr(Path, "read_bytes", _denied)

        result = boot_config()

        assert result.ok is False
        assert result.reason is BootFailure.PARSE_ERROR
        assert "读不出来" in result.problems[0].message

    def test_endpoint_is_none_when_file_unreadable(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        path = _wing_home(tmp_path, monkeypatch)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("gateway:\n  host: 0.0.0.0\n  port: 1234\n", encoding="utf-8")

        def _denied(self: Path) -> bytes:
            raise OSError("gone")

        monkeypatch.setattr(Path, "read_bytes", _denied)

        assert boot_mod._endpoint_from_file(path) is None

    def test_endpoint_is_none_on_invalid_yaml(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        path = _wing_home(tmp_path, monkeypatch)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("gateway:\n  host: [unclosed\n", encoding="utf-8")

        assert boot_mod._endpoint_from_file(path) is None


class TestEndpointOf:
    """endpoint 解析与 Rust 侧 ``#[serde(default)]`` 语义对齐。"""

    def test_non_mapping_document_falls_back(self):
        assert boot_mod._endpoint_of(["not", "a", "mapping"]) is None

    def test_missing_gateway_section_uses_defaults(self):
        endpoint = boot_mod._endpoint_of({})
        assert endpoint is not None
        host, port = endpoint
        assert (host, port) == (
            boot_mod.GatewayConfig.model_fields["host"].default,
            boot_mod.GatewayConfig.model_fields["port"].default,
        )

    def test_gateway_not_a_mapping_falls_back(self):
        assert boot_mod._endpoint_of(
            {"gateway": "127.0.0.1:1"}
        ) == boot_mod._endpoint_of({})

    def test_wrong_type_field_falls_back_entirely(self):
        assert boot_mod._endpoint_of({"gateway": {"host": 123, "port": 1}}) == (
            boot_mod._endpoint_of({})
        )
        assert boot_mod._endpoint_of({"gateway": {"host": "h", "port": "1"}}) == (
            boot_mod._endpoint_of({})
        )

    def test_boolean_port_is_rejected(self):
        """bool 是 int 的子类，但端口不能是 True/False。"""
        assert boot_mod._endpoint_of({"gateway": {"host": "h", "port": True}}) == (
            boot_mod._endpoint_of({})
        )

    def test_out_of_range_port_falls_back(self):
        assert boot_mod._endpoint_of(
            {"gateway": {"port": 70000}}
        ) == boot_mod._endpoint_of({})

    def test_empty_host_falls_back(self):
        assert boot_mod._endpoint_of(
            {"gateway": {"host": ""}}
        ) == boot_mod._endpoint_of({})

    def test_partial_fields_use_per_field_defaults(self):
        endpoint = boot_mod._endpoint_of({"gateway": {"host": "0.0.0.0"}})
        assert endpoint is not None
        host, port = endpoint
        assert host == "0.0.0.0"
        assert port == boot_mod.GatewayConfig.model_fields["port"].default
