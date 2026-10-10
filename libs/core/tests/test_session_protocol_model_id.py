"""协议字段：`/info` 投影与（真实会话上的）model_id / provider_name 形状。

会话侧的身份 / 恢复链见 `test_session_model_id.py`；本文件只钉**协议投影**：
`gateway/projection.build_session_info` 的三件套（model 调用名 + model_id 引用词 +
provider_name 运行期事实），含「id 取不到时降级为 None」的形态。
"""

from __future__ import annotations

from pathlib import Path

import pytest

from wing.config import AgentConfig, Config, ModelSpec, ProviderConfig
from wing.gateway.projection import build_session_info
from wing.session import SessionManager
from wing.store import FileSessionStore, SessionMetadata


def _config(models: list[str | ModelSpec], *, agent_model: str) -> Config:
    return Config(
        providers=[
            ProviderConfig(name="p", base_url="http://x", api_key="k", models=models)
        ],
        agents=[AgentConfig(name="default", model=agent_model)],
    )


def _use_config(monkeypatch: pytest.MonkeyPatch, cfg: Config) -> None:
    import wing.config.loader as loader

    monkeypatch.setattr(loader, "_config", cfg)


class TestSessionInfoProjection:
    @pytest.mark.asyncio
    async def test_info_carries_id_provider_and_call_name(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        _use_config(
            monkeypatch,
            _config(
                [
                    ModelSpec(id="ds-flash", name="deepseek-flash"),
                    ModelSpec(
                        id="gpt-id",
                        name="gpt-4",
                        display_name="GPT-4 Flash",
                    ),
                ],
                agent_model="gpt-id",
            ),
        )
        sm = SessionManager({"file": FileSessionStore(tmp_path / "sessions")})
        session = sm.create_session()

        info = build_session_info(session)
        # 模板默认：id 命中 → 三件套齐全（模型第一帧就有引用词）
        assert info.model == "gpt-4"
        assert info.model_id == "gpt-id"
        assert info.provider_name == "p"
        assert info.model_display_name == "GPT-4 Flash"

        await session.update_state(model_id="ds-flash")

        info = build_session_info(session)
        assert info.model == "deepseek-flash"
        assert info.model_id == "ds-flash"
        assert info.provider_name == "p"
        assert info.model_display_name is None  # 未声明 → None（前端回落调用名）

    @pytest.mark.asyncio
    async def test_info_degrades_to_none_when_id_is_unavailable(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """记录里的 id 与调用名都不在配置里（快照继续跑）→ model_id 为 None。"""
        root = tmp_path / "sessions"
        _use_config(
            monkeypatch,
            _config([ModelSpec(name="other")], agent_model="other"),
        )
        sm = SessionManager({"file": FileSessionStore(root)})
        session = sm.create_session()
        session.store.save_metadata(
            session.session_id,
            SessionMetadata(model_name="ghost-name", provider_name="p"),
        )

        restored = SessionManager({"file": FileSessionStore(root)}).resume_session(
            session.session_id
        )
        info = build_session_info(restored)

        assert info.model == "ghost-name"  # 快照继续跑
        assert info.model_id is None
        assert info.provider_name == "p"
