# wing_gateway/credential.py — 凭证管理与权限策略

"""
凭证是 Gateway 认证的核心。

设计基于 docs/insights/wss.md 的 TLS-PSK 方案：
  - 每个客户端拥有独立的凭证（identity + key）
  - 两种用户形态：账号+密码 / API Key（底层等价）
  - 权限策略绑定到凭证上，握手后自动生效

V1 实现：
  - token 认证（应用层，非 TLS-PSK）
  - 凭证存储在 YAML 文件中（~/.wing/credentials.yaml）
  - Python 3.12 的 ssl 模块不完整支持 TLS-PSK，
    等 Python 3.13+ 成为主流后升级到 TLS-PSK

凭证在连接时验证，之后绑定到 ws connection。
每个 ClientConnection 持有 identity 和权限策略，
Gateway 据此决定该 client 可以访问哪些 session。
"""

from __future__ import annotations

import hashlib
import os
import secrets
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from wing.common.logger import log


@dataclass
class PermissionPolicy:
    """凭证的权限策略——在连接时绑定到 ClientConnection。

    Gateway 据此决定该 client 可以：
      - 访问哪些 session（sessions: "*" 表示所有）
      - 发送什么类型的消息（allow_send_message: True/False）
      - 注册什么类型的工具（allow_register_tool: True/False）
    """

    sessions: str | list[str] = "*"  # 允许访问的 session，"*" 表示所有
    allow_send_message: bool = True  # 是否可以发送 user message
    allow_register_tool: bool = True  # 是否可以注册工具
    expires_at: float | None = None  # 过期时间（epoch seconds），None 表示永不过期


@dataclass
class Credential:
    """单个客户端的凭证。

    identity: 身份标识（明文传输，用于查找 key）
    key: 密钥（真正的秘密，绝不出现在网络上）
    policy: 权限策略

    账号+密码模式：identity=账号, key=密码
    API Key 模式：identity 和 key 编码进 sk-... 字符串
    """

    identity: str
    key: str  # 32 字节 hex
    policy: PermissionPolicy = field(default_factory=PermissionPolicy)

    def verify(self, token: str) -> bool:
        """验证 token 是否匹配此凭证。

        V1 用应用层 token 认证：
          token = hmac(identity, key) — 前端发送此值，Gateway 验证
        """
        expected = hashlib.sha256(f"{self.identity}:{self.key}".encode()).hexdigest()
        return secrets.compare_digest(token, expected)

    def is_expired(self) -> bool:
        """检查凭证是否过期。"""
        if self.policy.expires_at is None:
            return False
        import time

        return time.time() > self.policy.expires_at

    def to_api_key(self) -> str:
        """生成 API Key 格式：sk-<identity>:<key_base64>"""
        import base64

        encoded = base64.urlsafe_b64encode(self.key.encode()).decode().rstrip("=")
        return f"sk-{self.identity}:{encoded}"

    @classmethod
    def from_api_key(
        cls, api_key: str, policy: PermissionPolicy | None = None
    ) -> Credential:
        """从 API Key 字符串解析凭证。"""
        import base64

        if not api_key.startswith("sk-"):
            raise ValueError("Invalid API Key format")
        parts = api_key[3:].split(":", 1)
        if len(parts) != 2:
            raise ValueError("Invalid API Key format")
        identity = parts[0]
        # base64 解码，补齐 padding
        encoded = parts[1]
        padding = 4 - len(encoded) % 4
        if padding != 4:
            encoded += "=" * padding
        key = base64.urlsafe_b64decode(encoded).decode()
        return cls(identity=identity, key=key, policy=policy or PermissionPolicy())


class CredentialStore:
    """凭证库——存储所有客户端的凭证。

    V1 实现：YAML 文件存储（~/.wing/credentials.yaml）
    V2 可替换为数据库或加密存储。

    文件权限 600 — 只有 owner 可读写。
    """

    def __init__(self, path: Path | None = None) -> None:
        if path is None:
            path = Path.home() / ".wing" / "credentials.yaml"
        self._path = path
        self._credentials: dict[str, Credential] = {}

    def load(self) -> None:
        """从 YAML 文件加载凭证。"""
        if not self._path.exists():
            return
        import yaml

        data = yaml.safe_load(self._path.read_text())
        if not data:
            return
        for item in data.get("credentials", []):
            policy = PermissionPolicy(
                sessions=item.get("sessions", "*"),
                allow_send_message=item.get("allow_send_message", True),
                allow_register_tool=item.get("allow_register_tool", True),
                expires_at=item.get("expires_at"),
            )
            cred = Credential(
                identity=item["identity"],
                key=item["key"],
                policy=policy,
            )
            self._credentials[cred.identity] = cred
        log.info(f"Loaded {len(self._credentials)} credentials from {self._path}")

    def save(self) -> None:
        """保存凭证到 YAML 文件。"""
        import yaml

        data = {
            "credentials": [
                {
                    "identity": c.identity,
                    "key": c.key,
                    "sessions": c.policy.sessions,
                    "allow_send_message": c.policy.allow_send_message,
                    "allow_register_tool": c.policy.allow_register_tool,
                    "expires_at": c.policy.expires_at,
                }
                for c in self._credentials.values()
            ]
        }
        self._path.parent.mkdir(parents=True, exist_ok=True)
        self._path.write_text(yaml.dump(data, default_flow_style=False))
        os.chmod(self._path, 0o600)  # 只有 owner 可读写

    def get(self, identity: str) -> Credential | None:
        """根据 identity 查找凭证。"""
        cred = self._credentials.get(identity)
        if cred and cred.is_expired():
            return None
        return cred

    def verify_token(self, identity: str, token: str) -> Credential | None:
        """验证 identity + token，返回匹配的凭证或 None。"""
        cred = self.get(identity)
        if cred is None:
            return None
        if cred.verify(token):
            return cred
        return None

    def create(
        self, identity: str, key: str | None = None, **policy_kwargs: Any
    ) -> Credential:
        """创建新凭证。"""
        if key is None:
            key = secrets.token_hex(32)
        policy = PermissionPolicy(**policy_kwargs)
        cred = Credential(identity=identity, key=key, policy=policy)
        self._credentials[cred.identity] = cred
        self.save()
        return cred

    def delete(self, identity: str) -> bool:
        """删除凭证（吊销）。"""
        if identity in self._credentials:
            del self._credentials[identity]
            self.save()
            return True
        return False
