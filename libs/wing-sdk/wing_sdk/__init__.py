"""Wing SDK — 远程工具注册与编排。

核心用法：

    from wing_sdk import ToolHost

    host = ToolHost(client_id="my-host", gateway_url="http://127.0.0.1:32523")

    @host.tool(name="Bash")
    async def bash(command: str, timeout: int = 30) -> str:
        '''Execute a shell command.

        Args:
            command: The command to execute.
            timeout: Maximum wait time in seconds.
        '''
        ...

    await host.run()
"""

from wing_sdk.host import ToolHost
from wing_sdk.schema import ToolParam

__all__ = ["ToolHost", "ToolParam"]
