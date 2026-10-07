#!/usr/bin/env python3
"""Upsert the sticky ``wing-perf`` comment on a pull request (via the ``gh`` CLI)。

perf workflow 每轮出一条 markdown 报告；本工具保证全 PR 上只有**一条**这样的评论：

* 查找 —— ``GET repos/{repo}/issues/{pr}/comments``，每页 100 条，命中 marker
  （``<!-- wing-perf -->``）或翻到短页即停（首个匹配 = 最老的一条 = sticky）；
* 命中 —— ``PATCH repos/{repo}/issues/comments/{id}``（原地更新）；
* 未命中 —— ``POST repos/{repo}/issues/{pr}/comments``（创建）。

``--dry-run`` 只打印将执行的调用（含更新与创建两个分支），不联网、不需要 gh 与 token。

非 dry-run 需要 ``gh`` 在 PATH 上，并且 ``GH_TOKEN``（或已登录的 gh）。
退出码：0 = 已 upsert / 演练完成；1 = 任何失败（原因打到 stderr）。
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

#: sticky comment 的标记。全 PR 唯一一条，report.py 渲染正文时带上、这里据此查找/更新。
MARKER = "<!-- wing-perf -->"
#: 分页大小（与 gh api 的 per_page 上限一致）。
PAGE_SIZE = 100
#: 分页护栏：正常 PR 远到不了这个页数，只在 API 行为异常时兜底。
MAX_PAGES = 50


class CommentError(Exception):
    """预期内的失败：打印消息并退 1，不打 traceback。"""


def positive_int(text: str) -> int:
    """argparse type：严格正整数。"""
    try:
        value = int(text)
    except ValueError:
        raise argparse.ArgumentTypeError(f"not an integer: {text!r}") from None
    if value <= 0:
        raise argparse.ArgumentTypeError(f"must be positive: {value}")
    return value


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="comment.py",
        description=(
            "Upsert the sticky wing-perf comment on a pull request: page through the PR's issue "
            f"comments looking for the marker {MARKER}, then PATCH the oldest match — or POST a new "
            "comment when none exists."
        ),
        epilog=(
            "examples:\n"
            "  comment.py --repo owner/repo --pr 42 --body-file comment.md\n"
            "  comment.py --repo owner/repo --pr 42 --body-file comment.md --dry-run\n"
            "\n"
            "Outside --dry-run, gh must be on PATH and authenticated (GH_TOKEN)."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--repo",
        required=True,
        metavar="OWNER/REPO",
        help="repository holding the pull request",
    )
    parser.add_argument(
        "--pr",
        required=True,
        type=positive_int,
        metavar="N",
        help="pull request number",
    )
    parser.add_argument(
        "--body-file",
        required=True,
        metavar="PATH",
        help="markdown file to post as the comment body",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the gh api calls instead of making them (never touches the network)",
    )
    return parser.parse_args(argv)


def gh_api(method: str, path: str, payload: dict[str, str] | None = None) -> object:
    """执行 ``gh api --method <method> <path>``，返回解析后的 JSON 响应（无响应体则 None）。"""
    cmd = ["gh", "api", "--method", method, path]
    stdin = None
    if payload is not None:
        cmd += ["--input", "-"]
        stdin = json.dumps(payload, ensure_ascii=False)
    try:
        proc = subprocess.run(
            cmd,
            input=stdin,
            capture_output=True,
            text=True,
            encoding="utf-8",
            check=False,
        )
    except FileNotFoundError:
        raise CommentError(
            "gh CLI not found on PATH (needed for the upsert; use --dry-run to print the calls)"
        ) from None
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "").strip()[-500:]
        raise CommentError(
            f"gh api {method} {path} failed (exit {proc.returncode}): {detail}"
        )
    text = proc.stdout.strip()
    if not text:
        return None
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        raise CommentError(
            f"gh api {method} {path} returned non-JSON output: {text[:200]!r}"
        ) from None


def find_sticky_comment(repo: str, pr: int) -> int | None:
    """分页查找带 marker 的评论；返回最老一条的 id，找不到返回 None。"""
    for page in range(1, MAX_PAGES + 1):
        path = f"repos/{repo}/issues/{pr}/comments?per_page={PAGE_SIZE}&page={page}"
        batch = gh_api("GET", path)
        if not isinstance(batch, list):
            raise CommentError(f"unexpected response for {path}: expected a JSON array")
        for comment in batch:
            body = comment.get("body") if isinstance(comment, dict) else None
            if (
                isinstance(body, str)
                and MARKER in body
                and isinstance(comment.get("id"), int)
            ):
                return int(comment["id"])
        if len(batch) < PAGE_SIZE:
            return None
    raise CommentError(
        f"marker {MARKER} not found within {MAX_PAGES} pages on {repo}#{pr}"
    )


def print_dry_run(repo: str, pr: int, body: str) -> None:
    """打印真实运行会发出的 API 调用（离线；不需要 gh / token）。"""
    payload = json.dumps({"body": body}, ensure_ascii=False)
    search = f"gh api --method GET 'repos/{repo}/issues/{pr}/comments?per_page={PAGE_SIZE}&page=<n>'"
    update = f"gh api --method PATCH 'repos/{repo}/issues/comments/<id>' --input -"
    create = f"gh api --method POST 'repos/{repo}/issues/{pr}/comments' --input -"
    print("dry-run: nothing is sent; a real run would make these calls")
    print(
        f"  body    : {len(body)} chars (marker {'present' if MARKER in body else 'prepended'})"
    )
    print(
        f"  payload : {len(payload.encode('utf-8'))} bytes of JSON on stdin (--input -)"
    )
    print(
        f"  search  : {search}  # pages 1..{MAX_PAGES}; stops at the first match or a short page"
    )
    print(f"  update  : {update}  # marker found → PATCH the oldest match")
    print(f"  create  : {create}  # marker absent → POST a new comment")


def load_body(path: str) -> str:
    """读入并校验正文文件。"""
    file = Path(path)
    if not file.is_file():
        raise CommentError(f"body file not found: {path}")
    body = file.read_text(encoding="utf-8").strip()
    if not body:
        raise CommentError(f"body file is empty: {path}")
    return body


def ensure_marker(body: str) -> tuple[str, bool]:
    """正文缺 marker 时前置一个——否则贴出去将永远搜不到，下次就多出一条重复评论。"""
    if MARKER in body:
        return body, False
    return f"{MARKER}\n\n{body}", True


def upsert(repo: str, pr: int, body: str) -> str:
    """PATCH 已存在的 sticky 评论，或创建一条；返回一行结果摘要。"""
    comment_id = find_sticky_comment(repo, pr)
    if comment_id is None:
        response = gh_api("POST", f"repos/{repo}/issues/{pr}/comments", {"body": body})
        action = "created"
    else:
        response = gh_api(
            "PATCH", f"repos/{repo}/issues/comments/{comment_id}", {"body": body}
        )
        action = "updated"
    if isinstance(response, dict) and isinstance(response.get("html_url"), str):
        return f"sticky comment {action}: {response['html_url']}"
    return f"sticky comment {action} (no html_url in the gh api response)"


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        parts = args.repo.split("/")
        if len(parts) != 2 or not all(parts):
            raise CommentError(f"--repo must be OWNER/REPO, got {args.repo!r}")
        body = load_body(args.body_file)
        body, added = ensure_marker(body)
        if added:
            print(
                f"warning: body had no {MARKER} marker; prepended it", file=sys.stderr
            )
        if args.dry_run:
            print_dry_run(args.repo, args.pr, body)
            return 0
        print(upsert(args.repo, args.pr, body))
        return 0
    except CommentError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
