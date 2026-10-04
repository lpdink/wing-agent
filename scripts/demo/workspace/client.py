"""Tiny HTTP client — the subject of this demo workspace.

``test_client.py`` pins the behaviour we want from ``fetch``.
"""

from __future__ import annotations

import time
import urllib.error
import urllib.request

DEFAULT_ATTEMPTS = 3
DEFAULT_BACKOFF = 0.5


class TransientError(RuntimeError):
    """The remote end stayed unavailable for every attempt."""


def _sleep(seconds: float) -> None:
    """Indirection so the tests never actually wait."""
    time.sleep(seconds)


def fetch(url: str, *, timeout: float = 5.0) -> str:
    """GET *url* and return the body as text."""
    with urllib.request.urlopen(url, timeout=timeout) as resp:
        return resp.read().decode("utf-8")
