# wing/tools/internal/__init__.py
"""工具基础设施——不是工具，是工具实现共用的内部件。

  - ``utils``：``resolve_path``——相对路径按会话 workspace（ctx.cwd）解析；
  - ``rg``：``_run_rg``——ripgrep 子进程封装（Glob / Grep 共用）；
  - ``diff_window``：``DiffContentEvent`` 载荷窗口化（Edit 的 diff 事件窗口）；
  - ``shell_safety``：Bash 命令安全审查（白名单放行 / 默认拦截）。

本包模块**不注册任何工具**；被 ``tools/builtin/*`` 直接 import。
"""
