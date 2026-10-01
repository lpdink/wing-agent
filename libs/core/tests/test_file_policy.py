"""文件服务策略单测（`wing/gateway/file_policy.py` + `wing/gateway/static_host.py`）。

纯函数层：不起网关、不碰端口、不读真实 `~/.wing`（`WING_HOME` 用 monkeypatch 指到
tmp_path）。端点语义（状态码 / 响应头 / 字节）由 wing-probe 场景覆盖，这里只钉住
**判定**：包含性解析、扩展名白名单、静态根解析、保留路径、缓存头。
"""

from __future__ import annotations

from pathlib import Path

import pytest

from wing.config import GatewayConfig
from wing.gateway.file_policy import (
    CACHE_CONTROL_IMAGE,
    CACHE_CONTROL_IMMUTABLE,
    CACHE_CONTROL_NO_CACHE,
    IMAGE_CONTENT_TYPES,
    IMAGE_EXTENSIONS,
    IMAGE_MAX_BYTES,
    STATIC_CONTENT_TYPES,
    BadPathError,
    PathOutsideRootError,
    cache_control_for,
    image_content_type,
    is_within,
    resolve_within_root,
    static_content_type,
)
from wing.gateway.static_host import (
    RESERVED_EXACT,
    is_public_static_path,
    is_reserved_path,
    resolve_static_root,
    static_hosting_enabled,
)

# ── 包含性解析 ───────────────────────────────────────────────────


class TestResolveWithinRoot:
    def test_relative_inside(self, tmp_path: Path) -> None:
        ws = tmp_path / "ws"
        (ws / "sub").mkdir(parents=True)
        (ws / "sub" / "pic.png").write_bytes(b"x")

        target = resolve_within_root(ws, "sub/pic.png")
        assert target == (ws / "sub" / "pic.png").resolve()
        assert isinstance(target, Path)

    def test_nested_dotdot_stays_inside(self, tmp_path: Path) -> None:
        ws = tmp_path / "ws"
        (ws / "a" / "b").mkdir(parents=True)
        (ws / "pic.png").write_bytes(b"x")

        # a/b/../../pic.png 归一后仍在根内：不是"见到 .. 就拒"，而是看落点。
        assert (
            resolve_within_root(ws, "a/b/../../pic.png") == (ws / "pic.png").resolve()
        )

    def test_dotdot_escape(self, tmp_path: Path) -> None:
        ws = tmp_path / "ws"
        ws.mkdir()
        (tmp_path / "secret.png").write_bytes(b"secret")

        with pytest.raises(PathOutsideRootError):
            resolve_within_root(ws, "../secret.png")

    @pytest.mark.parametrize("raw", ["../ws-evil/f.png", "../wsevil/f.png"])
    def test_prefix_trap_is_outside(self, tmp_path: Path, raw: str) -> None:
        """字符串前缀比较会放行 `/…/ws-evil`——分量比较必须拦住。"""
        ws = tmp_path / "ws"
        evil = tmp_path / "ws-evil"
        evil.mkdir(parents=True)
        ws.mkdir()
        (evil / "f.png").write_bytes(b"x")

        with pytest.raises(PathOutsideRootError):
            resolve_within_root(ws, raw)

    def test_absolute_inside_and_outside(self, tmp_path: Path) -> None:
        ws = tmp_path / "ws"
        ws.mkdir()
        inside = ws / "pic.png"
        inside.write_bytes(b"x")
        outside = tmp_path / "other.png"
        outside.write_bytes(b"x")

        assert resolve_within_root(ws, str(inside)) == inside.resolve()
        with pytest.raises(PathOutsideRootError):
            resolve_within_root(ws, str(outside))

    def test_absolute_prefix_of_root_is_outside(self, tmp_path: Path) -> None:
        root = tmp_path / "ws"
        root.mkdir()
        sibling = tmp_path / "ws2"
        sibling.mkdir()
        (sibling / "pic.png").write_bytes(b"x")

        with pytest.raises(PathOutsideRootError):
            resolve_within_root(root, str(sibling / "pic.png"))

    def test_symlink_escape_refused(self, tmp_path: Path) -> None:
        ws = tmp_path / "ws"
        ws.mkdir()
        secret = tmp_path / "secret.png"
        secret.write_bytes(b"secret")
        (ws / "link.png").symlink_to(secret)

        with pytest.raises(PathOutsideRootError):
            resolve_within_root(ws, "link.png")

    def test_symlink_inside_allowed(self, tmp_path: Path) -> None:
        ws = tmp_path / "ws"
        (ws / "real").mkdir(parents=True)
        (ws / "real" / "pic.png").write_bytes(b"x")
        (ws / "link.png").symlink_to(ws / "real" / "pic.png")

        # 解析结果 = 真实文件（realpath 之后），仍在根内 → 放行。
        assert (
            resolve_within_root(ws, "link.png") == (ws / "real" / "pic.png").resolve()
        )

    def test_symlinked_directory_inside_allowed(self, tmp_path: Path) -> None:
        ws = tmp_path / "ws"
        (ws / "real").mkdir(parents=True)
        (ws / "real" / "pic.png").write_bytes(b"x")
        (ws / "alias").symlink_to(ws / "real", target_is_directory=True)

        assert (
            resolve_within_root(ws, "alias/pic.png")
            == (ws / "real" / "pic.png").resolve()
        )

    def test_nonexistent_inside_resolves(self, tmp_path: Path) -> None:
        """不存在也解析（存在性由调用方 stat 判定）——否则 404 与 403 无法区分。"""
        ws = tmp_path / "ws"
        ws.mkdir()
        assert resolve_within_root(ws, "nope.png") == (ws / "nope.png").resolve()

    def test_relative_root_is_resolved(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """相对根（会话 metadata 里可能存相对 workspace）按进程 cwd 解析。"""
        monkeypatch.chdir(tmp_path)
        (tmp_path / "ws").mkdir()
        assert (
            resolve_within_root("ws", "pic.png")
            == (tmp_path / "ws" / "pic.png").resolve()
        )

    @pytest.mark.parametrize("raw", ["", "a\x00b.png"])
    def test_bad_path(self, tmp_path: Path, raw: str) -> None:
        with pytest.raises(BadPathError):
            resolve_within_root(tmp_path, raw)

    def test_tilde_is_not_expanded(self, tmp_path: Path) -> None:
        """`~` 是普通目录名（请求路径不解释用户 home）。"""
        ws = tmp_path / "ws"
        ws.mkdir()
        assert resolve_within_root(ws, "~/pic.png") == (ws / "~" / "pic.png").resolve()

    def test_repeated_slashes_are_normalized(self, tmp_path: Path) -> None:
        ws = tmp_path / "ws"
        (ws / "sub").mkdir(parents=True)
        (ws / "sub" / "pic.png").write_bytes(b"x")
        assert (
            resolve_within_root(ws, "sub//./pic.png")
            == (ws / "sub" / "pic.png").resolve()
        )

    def test_space_and_unicode_names(self, tmp_path: Path) -> None:
        ws = tmp_path / "ws"
        ws.mkdir()
        (ws / "我的 图 片.png").write_bytes(b"x")
        assert resolve_within_root(ws, "我的 图 片.png").name == "我的 图 片.png"


class TestIsWithin:
    def test_component_wise(self, tmp_path: Path) -> None:
        assert is_within(tmp_path / "a" / "b", tmp_path)
        assert is_within(tmp_path, tmp_path)
        assert not is_within(str(tmp_path) + "-evil", tmp_path)
        assert not is_within(tmp_path / "..", tmp_path)

    def test_accepts_str_and_path(self, tmp_path: Path) -> None:
        assert is_within(str(tmp_path / "a"), str(tmp_path))
        assert is_within(tmp_path / "a", str(tmp_path))


# ── 类型表 ───────────────────────────────────────────────────────


class TestContentTypes:
    def test_image_whitelist_matches_vscode_extension(self) -> None:
        """与 `extensions/vscode/src/host/images.ts` 的 IMAGE_EXTENSIONS 同口径。"""
        assert IMAGE_EXTENSIONS == {
            ".png",
            ".jpg",
            ".jpeg",
            ".gif",
            ".webp",
            ".svg",
            ".bmp",
            ".ico",
            ".avif",
            ".apng",
        }
        assert set(IMAGE_CONTENT_TYPES) == set(IMAGE_EXTENSIONS)

    def test_uppercase_extension_allowed(self) -> None:
        assert image_content_type("PIC.PNG") == "image/png"

    def test_non_image_has_no_content_type(self) -> None:
        assert image_content_type("notes.txt") is None
        assert image_content_type("archive.tar.gz") is None
        assert image_content_type("no-extension") is None
        assert image_content_type("dir.with.dot/pic") is None

    def test_png_jpeg_aliases(self) -> None:
        assert image_content_type("a.jpg") == "image/jpeg"
        assert image_content_type("a.jpeg") == "image/jpeg"

    def test_image_max_bytes_is_10_mib(self) -> None:
        assert IMAGE_MAX_BYTES == 10 * 1024 * 1024

    def test_static_content_types_cover_build_artifacts(self) -> None:
        for suffix, expected in (
            (".html", "text/html; charset=utf-8"),
            (".js", "text/javascript; charset=utf-8"),
            (".css", "text/css; charset=utf-8"),
            (".json", "application/json"),
            (".svg", "image/svg+xml"),
            (".woff2", "font/woff2"),
        ):
            assert STATIC_CONTENT_TYPES[suffix] == expected

    def test_static_unknown_extension_falls_back(self) -> None:
        assert static_content_type("LICENSE") == "application/octet-stream"
        assert static_content_type("data.bin") == "application/octet-stream"

    def test_static_uppercase_extension(self) -> None:
        assert static_content_type("INDEX.HTML") == "text/html; charset=utf-8"


class TestCacheControl:
    @pytest.mark.parametrize(
        "relative",
        ["assets/app.js", "/assets/app.js", "assets/nested/chunk.css", "assets/"],
    )
    def test_assets_are_immutable(self, relative: str) -> None:
        assert cache_control_for(relative) == CACHE_CONTROL_IMMUTABLE

    @pytest.mark.parametrize(
        "relative",
        ["index.html", "", "favicon.ico", "assetsx/app.js", "sub/assets/app.js"],
    )
    def test_everything_else_revalidates(self, relative: str) -> None:
        assert cache_control_for(relative) == CACHE_CONTROL_NO_CACHE

    def test_image_endpoint_never_caches(self) -> None:
        assert CACHE_CONTROL_IMAGE == "no-store"


# ── 保留路径 ─────────────────────────────────────────────────────


class TestReservedPaths:
    @pytest.mark.parametrize(
        "path",
        [
            "/api",
            "/api/session/list",
            "/ws",
            "/docs",
            "/docs/",
            "/docs/oauth2-redirect",
            "/redoc",
            "/openapi.json",
        ],
    )
    def test_reserved(self, path: str) -> None:
        assert is_reserved_path(path)

    @pytest.mark.parametrize(
        "path",
        ["/", "/index.html", "/assets/app.js", "/apidocs", "/api-docs", "/wsx", "/API"],
    )
    def test_not_reserved(self, path: str) -> None:
        assert not is_reserved_path(path)

    def test_reserved_exact_list_is_documented(self) -> None:
        assert RESERVED_EXACT == {"/api", "/ws", "/docs", "/redoc", "/openapi.json"}


# ── 静态根解析 ───────────────────────────────────────────────────


class TestStaticRoot:
    def test_unset_is_disabled(self) -> None:
        assert resolve_static_root(GatewayConfig()) is None
        assert not static_hosting_enabled(GatewayConfig())

    @pytest.mark.parametrize("value", ["", "   ", None])
    def test_blank_is_disabled(self, value: str | None) -> None:
        assert resolve_static_root(GatewayConfig(static_dir=value)) is None

    def test_absolute_path(self, tmp_path: Path) -> None:
        dist = tmp_path / "dist"
        dist.mkdir()
        assert (
            resolve_static_root(GatewayConfig(static_dir=str(dist))) == dist.resolve()
        )

    def test_relative_path_resolves_against_wing_home(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv("WING_HOME", str(tmp_path))
        dist = tmp_path / "core" / "static"
        dist.mkdir(parents=True)
        assert resolve_static_root(GatewayConfig(static_dir="static")) == dist.resolve()

    def test_tilde_expanded(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv("HOME", str(tmp_path))
        (tmp_path / "dist").mkdir()
        assert (
            resolve_static_root(GatewayConfig(static_dir="~/dist"))
            == (tmp_path / "dist").resolve()
        )

    def test_missing_dir_is_disabled_but_configured(self, tmp_path: Path) -> None:
        """目录缺失 = 回 404（不报错）；但"配置了"这个事实仍成立（鉴权豁免看它）。"""
        config = GatewayConfig(static_dir=str(tmp_path / "missing"))
        assert resolve_static_root(config) is None
        assert static_hosting_enabled(config)

    def test_file_instead_of_dir_is_disabled(self, tmp_path: Path) -> None:
        target = tmp_path / "not-a-dir"
        target.write_text("x")
        assert resolve_static_root(GatewayConfig(static_dir=str(target))) is None

    def test_symlinked_dir_resolves_to_target(self, tmp_path: Path) -> None:
        real = tmp_path / "real-dist"
        real.mkdir()
        link = tmp_path / "dist"
        link.symlink_to(real, target_is_directory=True)
        assert (
            resolve_static_root(GatewayConfig(static_dir=str(link))) == real.resolve()
        )


class TestPublicStaticPath:
    def test_disabled_static_hosting_keeps_everything_protected(self) -> None:
        config = GatewayConfig()
        assert not is_public_static_path(config, "/")
        assert not is_public_static_path(config, "/assets/app.js")
        assert not is_public_static_path(config, "/api/session/list")

    def test_enabled_exposes_shell_only(self, tmp_path: Path) -> None:
        config = GatewayConfig(static_dir=str(tmp_path))
        assert is_public_static_path(config, "/")
        assert is_public_static_path(config, "/assets/app.js")
        assert is_public_static_path(config, "/deep/link")
        # API 与框架路由维持现状（auth 开启时仍要 key）。
        assert not is_public_static_path(config, "/api/workspace/image")
        assert not is_public_static_path(config, "/api")
        assert not is_public_static_path(config, "/docs")
        assert not is_public_static_path(config, "/openapi.json")
        assert not is_public_static_path(config, "/ws")
