# wing/tools/builtin/__init__.py
"""内置工具实现——一工具一文件。

本包是**纯实现目录**：不 import 任何工具模块（导入即注册的触发点仍是
``import wing.tools``，见 ``tools/__init__.py``）；工具模块各自在 import 期
由 ``@tool_registry.register(...)`` 装饰器完成注册。

Bash / Read / Write / Edit / Glob / Grep / ReadImage / AskUserQuestion / TodoWrite
九个工具，文件名与工具名对应（工具重命名时同步文件名）。
"""
