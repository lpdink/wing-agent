#!/usr/bin/env python3
"""速度演示的假 Provider：把语料按目标 tok/s 灌进 TUI（OpenAI 兼容，stdlib only）。

它回答的是另一个问题：**TUI 在高速流下什么观感**（README「Built for machine speed」
那一节的 GIF 与延迟数字）。剧情是假的、速度是真的：

* **不看请求内容**：每次请求从语料游标往后取一段，先 ``reasoning_content`` 再
  ``content``，永不发 tool_calls；
* **发射按绝对 deadline 自校正**（`env.sleep_until`）——``--tps`` 是真实目标速率，
  不是被 sleep 粒度污染的名义值；
* ``--usage-every`` 决定 usage 怎么带：``1`` = 每帧带累计 usage，网关据此实时算
  ``tokens_per_sec``（vLLM 的 ``continuous_usage_stats`` 就是这个形态），TUI 底栏的
  ``↓N out · M t/s`` 会在**流式过程中**跳——GIF 要的就是这个；``0``（默认）= 只在回合
  末尾带一次，与真 provider 的常规形态一致，量延迟时必须用这个（每帧带会让事件量翻倍，
  3000 tok/s 下显示延迟从 ~15ms 涨到 ~150ms）。

    uv run python scripts/demo/stream.py --tps 3000       # 起 provider + 网关
    # 录制：uv run python scripts/demo/record.py --serve stream.py --serve-arg=--tps=3000
    # 量延迟：uv run python scripts/demo/latency.py --steps 3000,30000,45000

``--marker-every`` / ``--emit-log`` 是给 `latency.py` 的：每 N 帧插一行
``⟦M#####⟧``，并把每帧的计划/发射时刻写 JSONL。
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import IO

import env
from corpus import build_corpus

#: 与主流 provider 的量级一致：一个 token 平均 2.83 个字符（fast-stream 实测口径）。
DEFAULT_CPT = 2.83
#: 每帧携带的 token 数。1 = 一 token 一帧：和真 provider 同形，也是渲染的最坏情形
#: （3000 tok/s ⇒ 3000 帧/s，latency.py 与 README 的数字都按这个口径）。
DEFAULT_CHUNK_TOKENS = 1
#: 首帧前的等待：真模型有 ttft，底栏会显示它。放在**响应头之前**——网关的 ttft
#: 口径是"发请求 → 收到响应头"，放在头之后会让底栏显示 1ms，一眼假。
DEFAULT_TTFT = 0.35
#: reasoning 前缀的长度（字符）：够看出"先想后答"，又不至于让 30 tok/s 那一档
#: 十秒里只看到思考。切在段落边界上。
REASON_CHARS = 380

WS_SRC = Path(__file__).resolve().parent / "workspace"

STATE: dict[str, object] = {}


def _log_emit(seq: int, kind: str, chars: int, planned: float) -> None:
    """每帧的计划/实际发射时刻写 JSONL（量显示延迟用；同机 perf_counter 跨进程可比）。"""
    handle: IO[str] | None = STATE.get("emit")  # type: ignore[assignment]
    if handle is None:
        return
    handle.write(
        json.dumps(
            {
                "seq": seq,
                "kind": kind,
                "chars": chars,
                "planned": planned,
                "sent": time.perf_counter(),
            }
        )
        + "\n"
    )
    handle.flush()


def split_reasoning(text: str, limit: int | None = None) -> tuple[str, str]:
    """按段落边界切出 reasoning 前缀，剩下的当正文。

    ``limit`` 语义：``<= 0`` → 全正文（没有 reasoning）；``>= len(text)`` → 全 reasoning；
    否则在前缀按段落边界切开。
    """
    if limit is None:
        limit = int(STATE.get("reason_chars", REASON_CHARS))
    if limit <= 0:
        return "", text
    if limit >= len(text):
        return text.strip(), ""
    cut = text.find("\n\n", limit)
    if cut == -1:
        return text.strip(), ""
    return text[:cut].strip(), text[cut:].strip()


def next_turn() -> tuple[str, str]:
    """本次请求要吐的 (reasoning, content)。语料按游标往前取一段，取完循环。"""
    text = str(STATE["corpus"])
    cursor = int(STATE.get("cursor", 0))
    window = text[cursor : cursor + int(STATE["turn_chars"])]
    STATE["cursor"] = 0 if cursor + len(window) >= len(text) else cursor + len(window)
    return split_reasoning(window)


def completion_id() -> str:
    return "chatcmpl-" + os.urandom(8).hex()


def usage(completion_tokens: int, prompt_tokens: int) -> dict[str, object]:
    """累计 usage（每帧都发，前端底栏才有实时 t/s）。"""
    return {
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
        "prompt_tokens_details": {"cached_tokens": 0},
    }


def chunk_payload(
    *,
    cid: str,
    created: int,
    model: str,
    delta: dict[str, str] | None = None,
    finish_reason: str | None = None,
    usage: dict[str, object] | None = None,
) -> dict[str, object]:
    payload: dict[str, object] = {
        "id": cid,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [
            {"index": 0, "delta": dict(delta or {}), "finish_reason": finish_reason}
        ],
    }
    if usage is not None:
        payload["usage"] = usage
    return payload


class Handler(BaseHTTPRequestHandler):
    protocol_version = (
        "HTTP/1.0"  # 无 Content-Length + Connection: close，客户端读到 EOF
    )
    disable_nagle_algorithm = True

    def log_message(self, *_args: object) -> None:  # noqa: ANN002 - 基类签名
        return

    def do_GET(self) -> None:  # noqa: N802 - 基类命名
        # 没有模型发现端点：wing 的模型目录来自配置声明（远端 GET /models 已退役）。
        self.send_error(404)

    def do_POST(self) -> None:  # noqa: N802
        if not self.path.rstrip("/").endswith("/chat/completions"):
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length") or 0)
        try:
            body = json.loads(self.rfile.read(length) or b"{}")
        except json.JSONDecodeError:
            body = {}
        if body.get("stream"):
            self._stream(body)
        else:
            self._whole(body)

    def _send(self, body: bytes) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    # ---- 流式 ------------------------------------------------------------
    def _stream(self, body: dict[str, object]) -> None:
        cid = completion_id()
        created = int(time.time())
        model = str(body.get("model") or STATE["model"])
        tps = float(STATE["tps"])
        cpt = float(STATE["cpt"])
        chunk_tokens = max(1, int(STATE["chunk_tokens"]))
        per_frame_chars = max(1, int(round(cpt * chunk_tokens)))
        interval = chunk_tokens / tps
        prompt_tokens = int(STATE["prompt_tokens_hint"])
        marker_every = int(STATE.get("marker_every", 0))
        usage_every = int(STATE.get("usage_every", 0))
        reason, content = next_turn()

        env.sleep_until(time.perf_counter() + float(STATE["ttft"]))  # ttft
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()

        start = time.perf_counter()
        seq = 0
        try:
            for field, phase in (("reasoning_content", reason), ("content", content)):
                pos = 0
                while pos < len(phase):
                    piece = phase[pos : pos + per_frame_chars]
                    pos += len(piece)
                    env.sleep_until(start + seq * interval)
                    if marker_every and seq % marker_every == 0:
                        # 独占一行的进度标记：短行不折行，所以在 TUI 的终端字节流里
                        # 必然是连续串 —— latency.py 靠它认领"这一帧画到屏幕上了"。
                        piece = f"\n⟦M{seq // marker_every:05d}⟧\n" + piece
                    frame = chunk_payload(
                        cid=cid,
                        created=created,
                        model=model,
                        delta={field: piece},
                        usage=(
                            usage(
                                max(1, int(round((seq + 1) * chunk_tokens))),
                                prompt_tokens,
                            )
                            if usage_every and seq % usage_every == 0
                            else None
                        ),
                    )
                    self.wfile.write(
                        (
                            "data: " + json.dumps(frame, ensure_ascii=False) + "\n\n"
                        ).encode()
                    )
                    self.wfile.flush()
                    _log_emit(seq, field, len(piece), start + seq * interval)
                    seq += 1
            tail = chunk_payload(
                cid=cid,
                created=created,
                model=model,
                delta={},
                finish_reason="stop",
                usage=usage(max(1, int(round(seq * chunk_tokens))), prompt_tokens),
            )
            self.wfile.write(
                ("data: " + json.dumps(tail, ensure_ascii=False) + "\n\n").encode()
            )
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass

    # ---- 非流式（压缩等路径用同一份语料） --------------------------------
    def _whole(self, body: dict[str, object]) -> None:
        model = str(body.get("model") or STATE["model"])
        reason, content = next_turn()
        payload = {
            "id": completion_id(),
            "object": "chat.completion",
            "created": int(time.time()),
            "model": model,
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": content,
                        "reasoning_content": reason,
                    },
                    "finish_reason": "stop",
                }
            ],
            "usage": usage(
                int(len(content) / float(STATE["cpt"])),
                int(STATE["prompt_tokens_hint"]),
            ),
        }
        self._send(json.dumps(payload, ensure_ascii=False).encode())


def serve(args: argparse.Namespace) -> tuple[ThreadingHTTPServer, str]:
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://{args.host}:{port}/v1"


async def main() -> int:
    parser = argparse.ArgumentParser(description="speed-demo provider + gateway")
    parser.add_argument("--tps", type=float, default=float(os.environ.get("TPS", 3000)))
    parser.add_argument("--cpt", type=float, default=DEFAULT_CPT)
    parser.add_argument("--chunk-tokens", type=int, default=DEFAULT_CHUNK_TOKENS)
    parser.add_argument("--ttft", type=float, default=DEFAULT_TTFT)
    parser.add_argument("--prompt-tokens", type=int, default=1_240)
    parser.add_argument("--corpus-bytes", type=int, default=260_000)
    parser.add_argument(
        "--reason-chars",
        type=int,
        default=REASON_CHARS,
        help=(
            "reasoning 前缀长度（字符）：<=0 = 全正文，>=len(语料) = 全 reasoning，"
            "否则在段落边界切开（默认 380）"
        ),
    )
    parser.add_argument(
        "--turn-chars",
        type=int,
        default=400_000,
        help="每轮吐多少字符（默认覆盖整份语料；调小可让回合在 N 秒内收尾）",
    )
    parser.add_argument(
        "--marker-every",
        type=int,
        default=0,
        help="每 N 帧插一行 ⟦M#####⟧ 标记（latency.py 用它量显示延迟；0 = 不插）",
    )
    parser.add_argument(
        "--emit-log", default=None, help="每帧的发射时刻写 JSONL（量延迟用）"
    )
    parser.add_argument(
        "--usage-every",
        type=int,
        default=0,
        help=(
            "每 N 帧带一次累计 usage；0 = 只在回合末尾带一次（真 provider 的常规形态，"
            "也与 fast-stream 的口径一致）。给 1 = 每帧都带 → TUI 底栏实时跳 t/s"
            "（GIF 用），代价是事件量翻倍"
        ),
    )
    parser.add_argument("--model", default="stream", help="状态栏里的模型名")
    parser.add_argument("--provider", default="demo", help="状态栏里的 provider 名")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=0)
    args = parser.parse_args()

    STATE.update(
        tps=args.tps,
        cpt=args.cpt,
        chunk_tokens=args.chunk_tokens,
        ttft=args.ttft,
        prompt_tokens_hint=args.prompt_tokens,
        marker_every=args.marker_every,
        usage_every=args.usage_every,
        emit=open(args.emit_log, "w", encoding="utf-8") if args.emit_log else None,
        model=args.model,
        reason_chars=args.reason_chars,
        corpus=build_corpus(args.corpus_bytes),
        turn_chars=args.turn_chars,
        cursor=0,
    )
    # 与 serve.py 对称：录制器会 ``cd`` 进工作区再起 TUI，目录不存在时 pane 直接退出
    # （现场表现为等首屏 20s 超时）。速度演示不调工具，但目录得在。
    env.copy_workspace(WS_SRC)
    env.clean_sessions()
    server, base_url = serve(args)
    env.say(
        f"[demo] speed provider → {base_url} ({args.tps:g} tok/s, {args.cpt:g} char/tok)"
    )

    gateway = env.Gateway(
        base_url,
        provider_name=args.provider,
        model=args.model,
        display_name=args.model,
        system_prompt=(
            "You are wing, a coding agent. Answer in depth, with concrete code and numbers."
        ),
    )
    await gateway.start()
    env.say(f"[demo] config → {gateway.config_path}")
    env.say(
        f"[demo] gateway → http://127.0.0.1:{gateway.port} (pid {gateway.proc.pid})"
    )
    env.say(env.ready_line(env.WS_RUN, gateway.port))
    try:
        await env.run_until_signal()
    finally:
        env.say("[demo] shutting down")
        server.shutdown()
        gateway.stop()
    return 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
