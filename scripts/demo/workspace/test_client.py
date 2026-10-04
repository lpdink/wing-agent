"""Behaviour ``client.fetch`` has to grow into: retry + exponential backoff."""

from __future__ import annotations

import unittest
import urllib.error
from unittest import mock

import client


class _Response:
    """Minimal stand-in for the ``urlopen`` context manager."""

    def __init__(self, body: str) -> None:
        self._body = body.encode("utf-8")

    def read(self) -> bytes:
        return self._body

    def __enter__(self) -> "_Response":
        return self

    def __exit__(self, *_exc: object) -> None:
        return None


class FetchTests(unittest.TestCase):
    def setUp(self) -> None:
        self.sleeps: list[float] = []
        patcher = mock.patch.object(client, "_sleep", self.sleeps.append)
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_returns_the_body_on_the_first_try(self) -> None:
        with mock.patch.object(
            urllib.request, "urlopen", return_value=_Response("hello")
        ) as opened:
            self.assertEqual(client.fetch("https://example.test/"), "hello")
        self.assertEqual(opened.call_count, 1)
        self.assertEqual(self.sleeps, [])

    def test_retries_transient_failures_with_backoff(self) -> None:
        failures = [urllib.error.URLError("boom"), urllib.error.URLError("boom")]
        with mock.patch.object(
            urllib.request, "urlopen", side_effect=[*failures, _Response("ok")]
        ) as opened:
            self.assertEqual(client.fetch("https://example.test/"), "ok")
        self.assertEqual(opened.call_count, 3)
        self.assertEqual(self.sleeps, [0.5, 1.0])

    def test_gives_up_after_the_last_attempt(self) -> None:
        with mock.patch.object(
            urllib.request, "urlopen", side_effect=urllib.error.URLError("down")
        ) as opened:
            with self.assertRaises(client.TransientError):
                client.fetch("https://example.test/")
        self.assertEqual(opened.call_count, client.DEFAULT_ATTEMPTS)
        self.assertEqual(self.sleeps, [0.5, 1.0])


if __name__ == "__main__":
    unittest.main()
