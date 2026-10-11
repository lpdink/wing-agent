# wing/tools/experimental/__init__.py
"""实验性工具包——命名域 ``experimental``（导入即注册，与 ``tools/__init__.py``
的约定一致；启用与否由会话工具集决定）。

当前内容：一套行锚编辑实验（借鉴 oh-my-pi 的 hashline 格式）——

- ``HashlineRead``（注册名 Read）：读取并给出内容指纹 TAG + ``N→text`` 行号行；
- ``HashlineEdit``（注册名 Edit）：``[path#TAG]`` 段 + ``PUT/CUT/REM/MV`` 操作，
  应用前与磁盘现状对账（文件变过即拒绝）。

引用方式：``experimental.Read`` / ``experimental.Edit``（agent ``tools`` 配置或
``POST /api/session/update``）。核心格式逻辑在 ``hashline.py``。
"""

from wing.tools.experimental.edit import hashline_edit
from wing.tools.experimental.read import hashline_read

__all__ = ["hashline_edit", "hashline_read"]
