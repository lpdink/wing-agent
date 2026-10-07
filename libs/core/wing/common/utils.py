import uuid
from datetime import datetime


# ============================================================
# Session ID 生成与校验
# ============================================================

#: session id 长度上限（**字节**，不是字符）。与"id 是**单层路径组件**"同源：
#: 会话目录名就是它，claude 转录投影还用 ``<id>.jsonl``（+6 字节）。Linux 与
#: macOS 的 ``NAME_MAX`` 都是 255 **字节**——按**字符**限长兜不住：``"收" * 128``
#: 是 384 字节，在 Linux 上会在首次写入炸 ``ENAMETOOLONG``（而 create 早已回 200）。
#: 128 字节远超任何真实编排方的 id（UUID = 36 字节），也给投影后缀留了余量。
SESSION_ID_MAX_BYTES = 128


class InvalidInputError(ValueError):
    """请求携带了**无法表示**的输入（当前唯一来源：不可编码为 UTF-8 的文本）。

    继承 ``ValueError``：既有的"非法输入 → 400"捕获点（create / update / tag）
    不需要改动；需要把 400 的口径钉死的地方（如 send 路由只该为"正文非法"回
    400，不该吞掉链上未来出现的其它 ``ValueError``）可以精确捕获本类型。
    """


def is_utf8_encodable(value: str) -> bool:
    """字符串能否编码为 UTF-8（孤立代理字符不能）。

    "合法 JSON ≠ 合法 UTF-8"：``"\\ud800"`` 是合法 JSON 转义，pydantic 会把它
    解成一个**孤立代理字符**，而任何落盘（``ensure_ascii=False`` 的 JSON 写入）
    或出网序列化都会在 ``encode("utf-8")`` 处炸。所有用户输入的闸门共用这一条
    判据（会话 id / 文本覆盖 / 工作目录 / 标题 / 标签 / 消息正文）。
    """
    try:
        value.encode("utf-8")
    except UnicodeEncodeError:
        return False
    return True


def require_utf8(value: str, *, field: str) -> str:
    """可编码为 UTF-8 就原样返回，否则 ``InvalidInputError``（指出是哪个输入）。"""
    if not is_utf8_encodable(value):
        raise InvalidInputError(
            f"{field} must be UTF-8 encodable (lone surrogates are not valid UTF-8)"
        )
    return value


def is_valid_session_id(value: object) -> bool:
    """判断值是否可作为 session id（不抛异常，供解析层做闸门判断）。

    闸门口径是"**仅防路径穿越 + 基本卫生**"：id 由后端生成（默认形态见
    ``generate_session_id``）**或由编排方自带**（Claude Agent SDK 系消费方
    用自己的 UUID 建会话 / 续链），因此不再限定某一种形态。接受：非空、
    **可编码为 UTF-8**、≤ ``SESSION_ID_MAX_BYTES`` 字节、其余任意字符。拒绝：

    - 含路径分隔符（``/`` ``\\``）或父目录引用 ``..``——id 是单层路径组件，
      防穿越（这也是它必须拒绝而非清洗的原因）；
    - 以 ``.`` 开头——覆盖 ``.`` / ``..`` 两个特例，并保留存储自己的点命名
      空间（``<sessions root>/.media`` 是媒体池，绝不能被寻址成会话）；
    - 含 ASCII 控制字符（0x00–0x1F、0x7F）——路径与终端展示的卫生；
    - **无法编码为 UTF-8**（孤立代理字符 ``"\\ud800"``：合法 JSON 但不是合法
      UTF-8——放行的话 create 会成功、随后在落盘与 HTTP 序列化处炸 500）；
    - 空串 / 超过字节上限。
    """
    if not isinstance(value, str) or not value:
        return False
    if not is_utf8_encodable(value):
        return False
    if len(value.encode("utf-8")) > SESSION_ID_MAX_BYTES:
        return False
    if value.startswith("."):
        return False
    if "/" in value or "\\" in value or ".." in value:
        return False
    return not any(ord(ch) < 0x20 or ord(ch) == 0x7F for ch in value)


def validate_session_id(session_id: str) -> str:
    """校验 session id 并原样返回；不合规即 raise ValueError。

    与 ``validate_media_id`` 同一精神：id 直接充当存储路径组件（会话目录名），
    脏值说明调用方逻辑已错（或是对抗输入），必须大声失败——静默清洗只会把
    问题藏起来，还留下路径穿越口子。

    错误文案里的 id 用 ``repr``（转义形式，恒为 ASCII）：原始代理字符直接拼进
    消息会在 HTTP 错误体序列化处再次炸。
    """
    if not is_valid_session_id(session_id):
        raise ValueError(
            f"invalid session id: {session_id!r} "
            f"(expect non-empty UTF-8 encodable id, ≤ {SESSION_ID_MAX_BYTES} bytes, "
            "no path separator / '..' / control char / leading '.')"
        )
    return session_id


def generate_session_id() -> str:
    """生成唯一 session id：{YYYYMMDD-HHMMSS}-{8位uuid}。

    这是**默认形态**（后端自生成时使用），不是唯一合法形态——编排方可自带
    id（见 ``is_valid_session_id``）。产出一律通过校验（生成与校验同处一份
    规则定义）。
    """
    timestamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    short_uuid = str(uuid.uuid4()).replace("-", "")[:8]
    return f"{timestamp}-{short_uuid}"


# ============================================================
# 路径安全校验
# ============================================================


def _is_safe_path_component(name: str) -> bool:
    """校验字符串是否可作为安全的文件路径单层组件。

    拒绝空串、含路径分隔符、含父目录引用（..）的值，
    防止路径穿越攻击。
    """
    if not name:
        return False
    if "/" in name or "\\" in name or ".." in name:
        return False
    return True


# ============================================================
# 异常链格式化
# ============================================================


def _cheap_context(exc: BaseException) -> str:
    """尽力为异常补一点廉价上下文（鸭子类型，不依赖传输层）。

    只认 httpx 风格的 `.request`（`method` + `url`）——空消息异常多半来自
    传输层（如 `httpx.ReadError("")`），"哪个请求"是它唯一能说出的信息。

    `.request` 可能是个**会抛异常**的 property（httpx 在未绑定请求时抛
    RuntimeError）——取上下文属于尽力而为，绝不能因为它把格式化本身搞崩。
    """
    try:
        request = getattr(exc, "request", None)
        url = getattr(request, "url", None)
    except Exception:
        return ""
    if url is None:
        return ""
    method = str(getattr(request, "method", "") or "")
    return f"{method} {url}".strip()


def _describe_exception(exc: BaseException) -> str:
    """单个异常的一行描述：`Type: message`；消息为空时不留空尾。

    空消息（`ReadError: ` 这种）信息量为零，至少给类型名，并尽量补上下文。
    """
    name = type(exc).__name__
    message = str(exc).strip()
    if message:
        return f"{name}: {message}"
    context = _cheap_context(exc)
    return f"{name} ({context})" if context else name


def format_exception_chain(exc: BaseException, max_depth: int = 5) -> str:
    """格式化完整异常链：遍历 __cause__ 和 __context__，返回可读字符串。

    例如：APIConnectionError: Connection error. (caused by ConnectError: Connection refused)
    深层链：A: msg (caused by B: msg (caused by C: msg))

    空消息异常不会产出 `Type: ` 这样的空尾文案（见 `_describe_exception`）。
    """
    parts: list[str] = []
    seen: set[int] = set()
    current: BaseException | None = exc

    while current is not None and len(parts) < max_depth:
        exc_id = id(current)
        if exc_id in seen:
            break
        seen.add(exc_id)

        parts.append(_describe_exception(current))

        current = current.__cause__ or current.__context__

    if not parts:
        return str(exc)

    result = parts[0]
    for cause in parts[1:]:
        result += f" (caused by {cause})"
    return result
