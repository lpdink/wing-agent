#!/usr/bin/env python3
"""``tmux pipe-pane`` 的出水管：无缓冲地把 pane 的原始输出追加到文件。

    rawdump.py <path>

为什么不是 ``cat``：``cat`` 写文件时是全缓冲的（管道进、文件出），几百字节的
启动序列会在它的缓冲区里躺着，录制器就看不到 ``ESC[6n`` ——而那是 TUI 首帧
在等的回包（见 ``record.py`` 里 ``Terminal.answer_queries``）。``cat`` 只有在
攒够 4~8 KiB 或者进程退出时才 flush，表现出来就是"有时能录、有时卡住"。
"""

from __future__ import annotations

import sys


def main() -> int:
    path = sys.argv[1]
    with open(path, "ab", buffering=0) as out:
        while True:
            chunk = sys.stdin.buffer.read1(65536)
            if not chunk:
                break
            out.write(chunk)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
