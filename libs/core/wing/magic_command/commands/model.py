# wing/magic_command/commands/model.py

from typing import TYPE_CHECKING

from wing.event import ModelListEvent, ModelSwitchedEvent, EventTarget

from ..registry import magic_registry

if TYPE_CHECKING:
    from wing.agent import WingAgent


@magic_registry.register(
    name="model", aliases=["m"], description="查看/切换模型", params="[name]"
)
async def cmd_model(agent: "WingAgent", args: str) -> str:
    if not args:
        try:
            models = await agent.model_provider.list_models()
            agent.emit(
                ModelListEvent(
                    session_id=agent.session_id,
                    models=models,
                    current_model=agent.model,
                    target=EventTarget(scope="session"),
                )
            )
            model_list = "\n".join(f"  - {m}" for m in models)
            return f"当前模型: {agent.model}\n可用模型:\n{model_list}"
        except Exception as e:
            return f"当前模型: {agent.model}\n❌ 获取模型列表失败: {e}"
    old_model = agent.model
    agent.model = args
    agent.emit(
        ModelSwitchedEvent(
            session_id=agent.session_id, old_model=old_model, new_model=args
        )
    )
    return f"模型已切换: {old_model} → {args}"
