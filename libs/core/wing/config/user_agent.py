# wing/config/user_agent.py
from .loader import get_config


# ── User-Agent constants ─────────────────────────────────────

_OPENCODE_VERSION = "1.3.17"
_QWEN_CODE_VERSION = "0.14.3"


def _get_platform_info() -> tuple[str, str]:
    import platform

    return platform.system().lower(), platform.machine().lower()


def _build_opencode_ua(system: str, arch: str) -> str:
    import platform

    release = platform.release()
    return f"opencode/{_OPENCODE_VERSION} ({system} {release}; {arch})"


def _build_qwen_code_ua(system: str, arch: str) -> str:
    return f"QwenCode/{_QWEN_CODE_VERSION} ({system}; {arch})"


def get_headers() -> dict[str, str]:
    config = get_config()
    preset = config.user_agent.preset
    system, arch = _get_platform_info()

    if preset == "opencode":
        ua = _build_opencode_ua(system, arch)
    elif preset == "qwen-code":
        ua = _build_qwen_code_ua(system, arch)
    else:
        raise ValueError(f"Unknown user_agent preset: {preset}")

    if preset == "qwen-code":
        return {
            "User-Agent": ua,
            "X-DashScope-CacheControl": "enable",
            "X-DashScope-UserAgent": ua,
            "X-DashScope-AuthType": "qwen-oauth",
        }

    return {"User-Agent": ua}
