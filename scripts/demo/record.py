#!/usr/bin/env python3
"""Record the README demos: real TUI in tmux → screen frames → cast → GIF/PNG.

    # ① 剧情回放（README 顶部 hero）：假 Provider 按剧本吐工具调用
    uv run python scripts/demo/record.py

    # ② 速度演示（渲染性能那一节）：同一份语料按不同 tok/s 灌进来（96 列是为了让
    #    状态栏那栏累积 token 让出去，见 scripts/demo/README.md）
    uv run python scripts/demo/record.py --serve stream.py --serve-arg=--tps=3000 \
        --serve-arg=--usage-every=1 --seconds 8 --cols 96 --rows 24 --fps 10 \
        --fps-cap 10 --name speed-3000 --release

    uv run python scripts/demo/record.py --no-render     # 只抓 cast
    uv run python scripts/demo/record.py --inspect       # 抓完打印逐帧摘要（调镜头）

Pipeline
--------
1. ``serve.py``（剧本）或 ``stream.py``（速度）以子进程启动：假 Provider + 真网关；
2. tmux 里跑真 ``wing``，敲进问题，然后按 ``fps`` 逐帧抓 ``capture-pane -e``
   （真彩 ANSI），只在变化的行上写字节 → asciinema v2 cast；
3. ``agg`` 把 cast 渲染成 GIF；静态图走同一条渲染路径（单帧 cast → GIF → PNG）。

收尾方式两种：剧情模式等「剧本耗尽 + 会话回 idle」，速度模式按 ``--seconds`` 计时。

``font_size`` 与 ``cols`` 直接决定成图宽度（``cols × 0.6 × font_size`` px）：README
正文显示宽度约 880px，成图别离太远，否则 GitHub 会把它压糊。

依赖：tmux、``agg``（首次运行自动下到 ``target/demo-tools/``，``$DEMO_AGG`` 可覆盖）、
出 PNG 需要 ``sips``（macOS）或 ImageMagick。
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import shlex
import shutil
import subprocess
import sys
import threading
import time
import urllib.request
from dataclasses import dataclass
from dataclasses import field
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
#: agg 落在 target/ 下：`target/` 本来就在 .gitignore 里，工具链缓存不污染工作区。
TOOLS = REPO / "target" / "demo-tools"
AGG_VERSION = "v1.9.0"
#: agg 的 release 产物按 <arch>-<os>-<abi> 命名；下载地址按本机平台拼。
AGG_TARGETS = {
    ("Darwin", "arm64"): "aarch64-apple-darwin",
    ("Darwin", "x86_64"): "x86_64-apple-darwin",
    ("Linux", "aarch64"): "aarch64-unknown-linux-gnu",
    ("Linux", "x86_64"): "x86_64-unknown-linux-gnu",
}


def agg_url() -> str:
    triple = AGG_TARGETS.get((platform.system(), platform.machine()))
    if triple is None:
        raise SystemExit(
            f"[record] no prebuilt agg for {platform.system()}/{platform.machine()}: "
            "install agg yourself and point $DEMO_AGG at it"
        )
    return (
        f"https://github.com/asciinema/agg/releases/download/{AGG_VERSION}/agg-{triple}"
    )


#: 剧情模式默认抓的静态图：按**屏幕内容**挑帧（比按时间猜稳：转录快慢会抖）。
#: 每项 = (帧里要出现的文本, 输出名)；``<last>`` 是特殊标记：取最后一帧。
STILLS: list[tuple[str, str]] = [
    ("Adding retry", "todos"),
    ("+ def fetch(", "diff"),
    ("<last>", "final"),
]

STORY_PROMPT = "make fetch retry transient failures with exponential backoff, and get the suite green"
SPEED_PROMPT = "walk me through the design of a reliable job queue"

#: 录完之后必须在某一帧里出现的内容——没有就说明这次录制是坏的（工具 schema 漂了、
#: 沙箱拦了 Bash、provider 没起来），此时**不能**把 GIF 写出去覆盖上一版资产。
STORY_EXPECT = ("+ def fetch(", "OK", "All three tests pass.")
SPEED_EXPECT = (" t/s ",)


@dataclass
class Config:
    """一次录制用到的全部旋钮（默认 = README 顶部的剧情 hero）。"""

    # ── 画面 ──────────────────────────────────────────────────────
    cols: int = 130
    rows: int = 40
    fps: float = 12.0
    font: str = os.environ.get("DEMO_FONT", "MesloLGS NF")
    font_size: int = 16
    line_height: float = 1.25
    fps_cap: int = 12  # GIF 帧率上限：12 在"够顺"和"体积可控"之间（15 会多 ~30% 字节）
    idle_limit: float = 0.4  # 长停顿压缩到 0.4s（开屏、收尾那种静止段）
    last_frame: float = 2.5  # 末帧停留（agg --last-frame-duration）
    theme: str = "github-dark"  # 背景 #0d1117 == GitHub 暗色画布，GIF 直接融进页面

    # ── 时序 ──────────────────────────────────────────────────────
    hold_welcome: float = 1.8  # 开屏：海鸥 + 扫光前段
    type_chunk: int = 7
    type_delay: float = 0.045  # ≈155 字符/秒，像手很快地敲
    hold_before_enter: float = 0.5
    hold_after: float = 2.2  # 收尾停留，让人把结论读完
    seconds: float | None = None  # 给定则按计时收尾（速度演示），而不是看剧本

    # ── 内容 ──────────────────────────────────────────────────────
    prompt: str = STORY_PROMPT
    server: str = "serve.py"
    server_args: list[str] = field(default_factory=list)
    session: str = "wing-demo"
    name: str = "demo"  # 产物基名：<name>.gif / <name>.cast
    #: 必须在某一帧里出现的内容（坏录制的守门；见 main 里的用法）。
    expect: tuple[str, ...] = ()
    #: 录完传一份到 rolling release（README 的图从这里来）。
    publish: bool = False
    stills: list[tuple[str, str]] = field(default_factory=lambda: list(STILLS))


def log(message: str) -> None:
    print(message, flush=True)


# ── tmux ────────────────────────────────────────────────────────


def tmux(*args: str, check: bool = True) -> str:
    result = subprocess.run(
        ["tmux", *args], capture_output=True, text=True, check=False
    )
    if check and result.returncode != 0:
        raise RuntimeError(f"tmux {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout


class Terminal:
    """tmux 里的一个 pane：抓屏 + 回答终端的「光标在哪」查询。

    真终端会回 ``ESC[6n``；tmux 对 detached 会话不代答，而 ratatui 的
    ``Terminal::clear()``（启动时必调）会一直等这条回包 —— 表现为首帧永远不画。
    所以这里用 ``pipe-pane`` 旁路到原始字节流，看到查询就补一条 ``ESC[1;1R``
    （位置本身无所谓：TUI 之后会自己把光标藏起来）。
    """

    def __init__(self, name: str, raw: Path) -> None:
        self.name = name
        self.raw = raw
        self.offset = 0
        self._seen = 0
        #: 待答查询：``[下次应答时刻, 排队时的输出字节数, 已试次数]``。
        self._pending: list[list[float]] = []

    def watch(self) -> None:
        """把 pane 的输出旁路到 ``raw``（必须在会话建好之后调）。"""
        self.raw.write_bytes(b"")
        dump = Path(__file__).with_name("rawdump.py")
        command = f"{shlex.quote(sys.executable)} {shlex.quote(str(dump))} {shlex.quote(str(self.raw))}"
        tmux("pipe-pane", "-t", self.name, "-o", command)

    def capture(self) -> list[str]:
        """当前屏幕，带 SGR 颜色（行尾空白要保留：背景色靠它撑）。"""
        return tmux("capture-pane", "-t", self.name, "-e", "-p").split("\n")[:-1]

    def text(self) -> str:
        return tmux("capture-pane", "-t", self.name, "-p", check=False)

    def answer_queries(self, stall: float = 0.35, attempts: int = 12) -> int:
        """回掉待答的光标位置查询（``ESC[6n``），返回答了几条。

        答一次不够：crossterm 读光标位置前会冲掉已到达的输入，回包抢在它冲之前就白给，
        应用会卡死在自己的超时里（现场就是 pane 里只有那 130 字节启动序列、首帧永不画）。
        所以没人继续画就隔 `stall` 秒补发一次，看到输出增长才撤销。
        """
        with self.raw.open("rb") as handle:
            handle.seek(self.offset)
            data = handle.read()
        self.offset += len(data)
        self._seen += len(data)
        now = time.monotonic()
        for _ in range(data.count(b"\x1b[6n")):
            self._pending.append([now, self._seen, 0])
        answered = 0
        for item in list(self._pending):
            due, mark, tries = item
            if self._seen > mark + 200 or tries >= attempts:
                self._pending.remove(item)
            elif now >= due:
                tmux("send-keys", "-t", self.name, "-l", "\x1b[1;1R")
                item[0] = now + stall
                item[2] = tries + 1
                answered += 1
        return answered

    def wait_for(self, marker: str, timeout: float = 20.0) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.answer_queries()
            if marker in self.text():
                return
            time.sleep(0.1)
        raise TimeoutError(f"marker not seen within {timeout}s: {marker!r}")


# ── cast ────────────────────────────────────────────────────────


class Cast:
    """asciinema v2 写出器：只在变化的行上发字节（逐帧绝对定位重画）。"""

    def __init__(self, path: Path, cols: int, rows: int) -> None:
        self.path = path
        self.cols, self.rows = cols, rows
        self.frames: list[tuple[float, list[str]]] = []
        self.changed = 0
        self._prev: list[str] | None = None
        self._file = path.open("w", encoding="utf-8")
        self._file.write(
            json.dumps(
                {
                    "version": 2,
                    "width": cols,
                    "height": rows,
                    "timestamp": int(time.time()),
                    "env": {"TERM": "xterm-256color", "SHELL": "/bin/sh"},
                }
            )
            + "\n"
        )

    def add(self, at: float, lines: list[str]) -> None:
        lines = lines[: self.rows] + [""] * max(0, self.rows - len(lines))
        self.frames.append((at, lines))
        buf: list[str] = []
        for row in range(self.rows):
            line = lines[row]
            if self._prev is not None and self._prev[row] == line:
                continue
            buf.append(f"\x1b[{row + 1};1H\x1b[K" + line)
        if buf:
            self._file.write(json.dumps([round(at, 3), "o", "".join(buf)]) + "\n")
            self.changed += 1
        self._prev = lines

    def close(self) -> None:
        self._file.close()

    def first_with(self, marker: str) -> list[str] | None:
        """第一次出现 ``marker`` 的那一帧（纯文本比对，忽略颜色）。"""
        for _at, lines in self.frames:
            if marker in "\n".join(strip_sgr(line) for line in lines):
                return lines
        return None

    def timeline(self) -> str:
        """逐帧摘要（调镜头用）：最后一行的变化 + 每帧非空行数。"""
        out = []
        for at, lines in self.frames:
            text = [line for line in lines if strip_sgr(line).strip()]
            out.append(
                f"{at:6.2f}s  {len(text):3d}行  {strip_sgr(text[-1])[:96] if text else ''}"
            )
        return "\n".join(out)


SGR = re.compile(r"\x1b\[[0-9;]*m")


def strip_sgr(line: str) -> str:
    return SGR.sub("", line)


def frame_cast(path: Path, lines: list[str], cols: int, rows: int) -> None:
    cast = Cast(path, cols, rows)
    cast.add(0.0, lines)
    cast.close()


# ── 渲染 ────────────────────────────────────────────────────────


def agg_bin() -> Path:
    override = os.environ.get("DEMO_AGG")
    if override:
        return Path(override)
    local = TOOLS / "agg"
    if local.is_file():
        return local
    log(f"[record] downloading agg → {local}")
    TOOLS.mkdir(exist_ok=True)
    # curl 认 https_proxy / http_proxy：网络需要代理时（如国内直连 GitHub 不通）
    # 在执行本脚本前导出即可。--max-time 只是别让它无限挂着。
    subprocess.run(
        [
            "curl",
            "-fSL",
            "--connect-timeout",
            "15",
            "--max-time",
            "300",
            "-o",
            str(local),
            os.environ.get("DEMO_AGG_URL") or agg_url(),
        ],
        check=True,
    )
    local.chmod(0o755)
    return local


#: README 里的 GIF 挂在同一个 rolling release 上：同名资产用 --clobber 覆盖，URL 不变，
#: 也就不必把几 MB 的二进制塞进 git 历史。
RELEASE_TAG = "readme-assets"
RELEASE_NOTES = (
    "Automatically published GIFs used by the README (see `scripts/demo/`).\n\n"
    "Assets are re-uploaded to this tag with `--clobber`, so the raw URLs stay stable.\n"
    "Nothing here is a source release."
)


def publish_release(gif: Path) -> None:
    """把渲染好的 GIF 传到 rolling release（缺 release 就先建）。"""
    if shutil.which("gh") is None:
        raise SystemExit("[record] --release needs the `gh` CLI on PATH")
    probe = subprocess.run(
        ["gh", "release", "view", RELEASE_TAG],
        cwd=str(REPO),
        capture_output=True,
        text=True,
        check=False,
    )
    if probe.returncode != 0:
        log(f"[record] creating rolling release {RELEASE_TAG}")
        subprocess.run(
            [
                "gh",
                "release",
                "create",
                RELEASE_TAG,
                "--title",
                "README assets",
                "--notes",
                RELEASE_NOTES,
                "--prerelease",
            ],
            cwd=str(REPO),
            check=True,
        )
    subprocess.run(
        ["gh", "release", "upload", RELEASE_TAG, str(gif), "--clobber"],
        cwd=str(REPO),
        check=True,
    )
    url = f"https://github.com/lpdink/wing-agent/releases/download/{RELEASE_TAG}/{gif.name}"
    log(f"[record] published → {url}")


def agg_render(cfg: Config, src: Path, dst: Path) -> None:
    subprocess.run(
        [
            str(agg_bin()),
            "--font-family",
            cfg.font,
            "--font-size",
            str(cfg.font_size),
            "--line-height",
            str(cfg.line_height),
            "--theme",
            cfg.theme,
            "--fps-cap",
            str(cfg.fps_cap),
            "--idle-time-limit",
            str(cfg.idle_limit),
            "--last-frame-duration",
            str(cfg.last_frame),
            "--quiet",
            str(src),
            str(dst),
        ],
        check=True,
    )


def gif_to_png(gif: Path, png: Path) -> bool:
    """GIF 首帧 → PNG（sips / ImageMagick；都没有就只留 GIF）。"""
    if shutil.which("sips"):
        return (
            subprocess.run(
                ["sips", "-s", "format", "png", str(gif), "--out", str(png)],
                capture_output=True,
            ).returncode
            == 0
        )
    for tool in ("magick", "convert"):
        if shutil.which(tool):
            return (
                subprocess.run(
                    [tool, f"{gif}[0]", str(png)], capture_output=True
                ).returncode
                == 0
            )
    return False


# ── 录制 ────────────────────────────────────────────────────────


def wing_bin() -> Path:
    for key in ("WING_BIN", "DEMO_WING_BIN"):
        if os.environ.get(key):
            return Path(os.environ[key])
    for candidate in (
        REPO / "target/release/wing",
        REPO / "target/debug/wing",
        REPO / ".venv/bin/wing",
    ):
        if candidate.is_file():
            return candidate
    found = shutil.which("wing")
    if found:
        return Path(found)
    raise SystemExit(
        "[record] no wing binary — cargo build --release -p wing, or set $WING_BIN"
    )


def start_demo(
    cfg: Config,
) -> tuple[subprocess.Popen[str], dict[str, str], threading.Event]:
    """起假 Provider + 网关：读它的 stdout 拿 ready 信息与「剧本耗尽」信号。"""
    server = Path(__file__).with_name(cfg.server)
    proc = subprocess.Popen(
        [sys.executable, "-u", str(server), *cfg.server_args],
        cwd=str(REPO),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    info: dict[str, str] = {}
    exhausted = threading.Event()

    def pump() -> None:
        assert proc.stdout is not None
        for raw in proc.stdout:
            line = raw.rstrip()
            if line.startswith("[demo]"):
                log(f"    {line}")
            found = re.search(
                r"WING_HOME=(\S+) WING_WORKSPACE=(\S+) WING_GATEWAY_PORT=(\d+)", line
            )
            if found:
                info["wing_home"], info["workspace"], info["port"] = found.groups()
                info["ready"] = "1"
            if "script exhausted" in line:
                exhausted.set()

    threading.Thread(target=pump, daemon=True).start()
    deadline = time.monotonic() + 45
    while "ready" not in info and time.monotonic() < deadline:
        if proc.poll() is not None:
            raise SystemExit(f"[record] {cfg.server} exited early")
        time.sleep(0.1)
    if "ready" not in info:
        proc.terminate()
        raise SystemExit(f"[record] {cfg.server} did not become ready")
    return proc, info, exhausted


def session_idle(port: str) -> bool:
    """会话是否已经回到 idle（绕开代理直连 loopback）。

    从盘上加载的历史会话报 ``inactive``：只要**没有**在跑的（working/waiting），
    且当前会话是 idle，就算收尾了。
    """
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        with opener.open(
            f"http://127.0.0.1:{port}/api/session/list", timeout=2
        ) as resp:
            sessions = json.load(resp).get("sessions", [])
    except Exception:
        return False
    statuses = [s.get("status") for s in sessions]
    return "idle" in statuses and not any(s in ("working", "waiting") for s in statuses)


def record(cfg: Config, out: Path) -> tuple[Path, Cast]:
    serve, info, exhausted = start_demo(cfg)
    tmux("kill-session", "-t", cfg.session, check=False)
    try:
        wing = wing_bin()
        workspace = Path(info["workspace"])
        log(f"[record] wing → {wing}")
        term = Terminal(cfg.session, out / "pane.raw")
        # 闸门：先让 pane 停在 shell 里等信号，把 pipe-pane 挂上再真正 exec wing。
        # 否则应用可能在旁路挂上之前就发出 ESC[6n，那一条没人答 → 首帧永不来。
        gate = out / "gate"
        gate.unlink(missing_ok=True)
        # tmux 新会话**继承客户端环境**：录制器为下 agg 导出的 http(s)_proxy 会漏进
        # pane，于是 TUI 连自己的 loopback 网关也要走代理 → health 失败 → 报
        # "Port … already in use"。这里显式清干净：TUI 只跟本机网关说话。
        tmux(
            "new-session",
            "-d",
            "-s",
            cfg.session,
            "-x",
            str(cfg.cols),
            "-y",
            str(cfg.rows),
            *[
                arg
                for name in (
                    "http_proxy",
                    "https_proxy",
                    "all_proxy",
                    "HTTP_PROXY",
                    "HTTPS_PROXY",
                    "ALL_PROXY",
                )
                for arg in ("-e", f"{name}=")
            ],
            "-e",
            "no_proxy=127.0.0.1,localhost",
            "-e",
            "NO_PROXY=127.0.0.1,localhost",
            "-e",
            f"WING_HOME={info['wing_home']}",
            "-e",
            "COLORTERM=truecolor",
            "-e",
            "TERM=xterm-256color",
            f"while [ ! -e {gate} ]; do sleep 0.05; done; cd {workspace} && exec {wing}",
        )
        term.watch()
        gate.touch()
        term.wait_for("Esc 中断")

        cast = Cast(out / f"{cfg.name}.cast", cfg.cols, cfg.rows)
        started = time.monotonic()
        typed = 0
        sent_enter = False
        enter_at: float | None = None
        stop_at: float | None = None
        next_frame = started

        log(f"[record] capturing {cfg.cols}x{cfg.rows} @ {cfg.fps:g}fps")
        while True:
            now = time.monotonic()
            elapsed = now - started
            if cfg.seconds is None:
                # 收尾判定：最后一轮已消费（剧本耗尽）且会话回到 idle —— 此时画面上
                # 就是最终状态，停留一段时间让人读完，然后停录。
                if (
                    stop_at is None
                    and exhausted.is_set()
                    and session_idle(info["port"])
                ):
                    stop_at = now + cfg.hold_after
            elif enter_at is not None and now >= enter_at + cfg.seconds:
                stop_at = now  # 速度模式：计时到点直接停（末帧由 agg 停留）
            if (stop_at is not None and now >= stop_at) or elapsed > 180:
                if elapsed > 180:
                    log("[record] safety timeout")
                break

            # 打字：按时间轴推进（和抓帧同一条循环，打字期间照样出帧）
            want = min(
                len(cfg.prompt),
                max(0, int((elapsed - cfg.hold_welcome) / cfg.type_delay))
                * cfg.type_chunk,
            )
            if want > typed:
                tmux("send-keys", "-t", term.name, "-l", cfg.prompt[typed:want])
                typed = want
            if (
                not sent_enter
                and elapsed
                >= cfg.hold_welcome
                + len(cfg.prompt) * cfg.type_delay
                + cfg.hold_before_enter
            ):
                tmux("send-keys", "-t", term.name, "Enter")
                sent_enter = True
                enter_at = now

            term.answer_queries()  # 每次都得应答：clear() 之类的路径会再问一次
            cast.add(elapsed, term.capture())
            next_frame += 1 / cfg.fps
            delay = next_frame - time.monotonic()
            if delay > 0:
                time.sleep(delay)
            else:
                next_frame = time.monotonic()

        cast.close()
        log(
            f"[record] cast → {cast.path} ({len(cast.frames)} frames, {cast.changed} changed)"
        )
        (out / "timeline.txt").write_text(cast.timeline(), encoding="utf-8")
        return cast.path, cast
    finally:
        tmux("kill-session", "-t", cfg.session, check=False)
        serve.terminate()
        try:
            serve.wait(timeout=10)
        except subprocess.TimeoutExpired:
            serve.kill()


def render_stills(cfg: Config, cast: Cast, out: Path) -> list[Path]:
    """静态图：按内容标记挑一帧 → 单帧 cast → agg → GIF → PNG。

    走的是和 GIF 完全相同的渲染路径，所以静态图和动画里那一瞬间长得一样。
    """
    stills_dir = out / "stills"
    stills_dir.mkdir(parents=True, exist_ok=True)
    written = []
    for marker, name in cfg.stills:
        frame = cast.frames[-1][1] if marker == "<last>" else cast.first_with(marker)
        if frame is None:
            log(f"[record] still {name!r}: no frame contains {marker!r}")
            continue
        single = stills_dir / f"{name}.cast"
        frame_cast(single, frame, cfg.cols, cfg.rows)
        gif = stills_dir / f"{name}.gif"
        agg_render(cfg, single, gif)
        png = stills_dir / f"{name}.png"
        if not gif_to_png(gif, png):
            log(f"[record] no PNG tool (sips / ImageMagick) — kept {gif.name}")
            written.append(gif)
            continue
        gif.unlink()
        single.unlink()
        written.append(png)
        log(f"[record] still {name} → {png.name}")
    return written


def parse_args() -> tuple[Config, argparse.Namespace]:
    parser = argparse.ArgumentParser(description="record the wing README demos")
    parser.add_argument("--out", default=str(REPO / "target" / "demo"), help="输出目录")
    parser.add_argument(
        "--name", default="demo", help="产物基名（<name>.gif / <name>.cast）"
    )
    parser.add_argument(
        "--serve", default="serve.py", help="假 Provider 脚本（serve.py / stream.py）"
    )
    parser.add_argument(
        "--serve-arg",
        action="append",
        default=[],
        metavar="ARG",
        help="透传给假 Provider 脚本的参数（可重复，如 --serve-arg --tps=3000）",
    )
    parser.add_argument("--prompt", default=STORY_PROMPT, help="敲进去的问题")
    parser.add_argument(
        "--seconds",
        type=float,
        default=None,
        help="计时收尾（秒，Enter 之后算起）；不给则按剧本耗尽 + 会话 idle 收尾",
    )
    parser.add_argument("--cols", type=int, default=130)
    parser.add_argument("--rows", type=int, default=40)
    parser.add_argument("--fps", type=float, default=12.0, help="抓帧频率")
    parser.add_argument("--fps-cap", type=int, default=12, help="GIF 帧率上限")
    parser.add_argument("--font-size", type=int, default=16)
    parser.add_argument("--last-frame", type=float, default=None, help="末帧停留秒数")
    parser.add_argument(
        "--expect",
        action="append",
        default=[],
        metavar="TEXT",
        help="要求某一帧里出现 TEXT，否则本次录制算失败（可重复；有默认值）",
    )
    parser.add_argument(
        "--release",
        action="store_true",
        help="渲染完把 GIF 传到 readme-assets 这个 rolling release（URL 不变）",
    )
    parser.add_argument("--no-stills", action="store_true", help="不出静态图")
    parser.add_argument("--no-render", action="store_true", help="只出 cast")
    parser.add_argument("--inspect", action="store_true", help="打印逐帧摘要后退出")
    parser.add_argument(
        "--still",
        action="append",
        default=[],
        metavar="MARKER=NAME",
        help="改抽静态图：帧里含 MARKER 的第一帧 → NAME.png（可重复）",
    )
    args = parser.parse_args()

    speed = args.seconds is not None
    cfg = Config(
        cols=args.cols,
        rows=args.rows,
        fps=args.fps,
        fps_cap=args.fps_cap,
        font_size=args.font_size,
        seconds=args.seconds,
        prompt=args.prompt or (SPEED_PROMPT if speed else STORY_PROMPT),
        server=args.serve,
        server_args=args.serve_arg,
        session=f"wing-demo-{args.name}",
        name=args.name,
        last_frame=args.last_frame
        if args.last_frame is not None
        else (1.2 if speed else 2.5),
        expect=tuple(args.expect)
        if args.expect
        else (SPEED_EXPECT if speed else STORY_EXPECT),
        publish=args.release,
    )
    if speed:
        # 速度演示不需要"哪个片段"的静态图，默认不出（要的话显式给 --still）。
        cfg.stills = []
        cfg.idle_limit = 0.25
        # 前戏压短：GIF 的预算花在流上，而不是"看开屏"（开屏有 hero 那张）。
        cfg.hold_welcome = 1.2
        cfg.hold_before_enter = 0.3
    if args.no_stills:
        cfg.stills = []
    if args.still:
        cfg.stills = []
        for spec in args.still:
            marker, _, name = spec.partition("=")
            if not marker or not name:
                raise SystemExit(f"--still expects MARKER=NAME, got {spec!r}")
            cfg.stills.append((marker, name))
    return cfg, args


def main() -> int:
    cfg, args = parse_args()
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    cast_path, cast = record(cfg, out)
    if args.inspect:
        print((out / "timeline.txt").read_text(encoding="utf-8"))
        return 0

    missing = [marker for marker in cfg.expect if cast.first_with(marker) is None]
    if missing:
        # 坏录制不许覆盖上一版资产：这里就退出，GIF 不渲染、不拷。
        log(f"[record] FAILED: no frame contains {missing!r} — recording looks broken")
        log(f"[record]        cast kept for inspection: {cast_path}")
        return 1
    log(f"[record] checks passed: {list(cfg.expect)}")
    if args.no_render:
        return 0

    gif = out / f"{cfg.name}.gif"
    agg_render(cfg, cast_path, gif)
    log(f"[record] gif → {gif} ({gif.stat().st_size / 1024:.0f} KiB)")
    if cfg.stills:
        render_stills(cfg, cast, out)
    if cfg.publish:
        publish_release(gif)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
