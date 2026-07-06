# wing/magic_command/commands/reload.py

from typing import TYPE_CHECKING

from wing.event import CommandInfo, CommandListEvent

from ..registry import magic_registry

if TYPE_CHECKING:
    from wing.agent import WingAgent


def _emit_command_list(agent: "WingAgent") -> None:
    """发出 CommandListEvent 以刷新前端命令缓存。"""
    commands = [
        CommandInfo(
            name=cmd.name,
            aliases=cmd.aliases,
            description=cmd.description,
            params=cmd.params,
        )
        for cmd in magic_registry.list_all()
    ]
    agent.emit(CommandListEvent(session_id=agent.session_id, commands=commands))


@magic_registry.register(
    name="reload", description="重载配置、hooks、命令、skills、rules"
)
async def cmd_reload(agent: "WingAgent", args: str) -> str:
    from wing.config import load_config, load_hooks
    from wing.hook_registry import hooks
    from wing.magic_command.prompt_commands import register_prompt_commands

    results: list[str] = []
    total = 5
    success = 0

    # 1. Reload config
    try:
        config = load_config(reload=True)
        results.append("✅ config.yaml")
        success += 1
    except Exception as e:
        results.append(f"❌ config.yaml: {e}")
        return "🔄 Reload aborted (config failed):\n" + "\n".join(results)

    # 2. Reload hooks
    try:
        hooks.clear()
        load_hooks(config.hooks)
        results.append("✅ hooks")
        success += 1
    except Exception as e:
        results.append(f"❌ hooks: {e}")

    # 3. Reload prompt commands
    try:
        magic_registry.remove_by_source("prompt")
        register_prompt_commands(config.commands.paths)
        results.append("✅ prompt commands")
        success += 1
    except Exception as e:
        results.append(f"❌ prompt commands: {e}")

    # 4. Reload OpenAI provider (base_url, api_key, timeouts, reasoning_effort)
    try:
        changes = agent.model_provider.reload()
        if changes:
            results.append(f"✅ provider ({', '.join(changes)})")
        else:
            results.append("✅ provider (unchanged)")
        success += 1
    except Exception as e:
        results.append(f"❌ provider: {e}")

    # 5. Reload skills & rules (current session)
    try:
        agent.context_manager.reload_skills_and_rules()
        results.append("✅ skills & rules")
        success += 1
    except Exception as e:
        results.append(f"❌ skills & rules: {e}")

    # 6. Emit CommandListEvent to refresh frontend cache
    _emit_command_list(agent)

    prefix = (
        "🔄 Reload complete"
        if success == total
        else f"⚠️ Reload partial ({success}/{total})"
    )
    return f"{prefix}:\n" + "\n".join(results)
