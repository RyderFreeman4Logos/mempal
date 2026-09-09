"""Shared fail-closed boundaries for full-smoke unit tests."""
from unittest import TestCase, mock


class FailClosedExternalCalls(TestCase):
    """Reject subprocess and network escapes even through broad Exception handlers."""

    def setUp(self) -> None:
        for target in ("subprocess.Popen", "urllib.request.urlopen"):
            escape = BaseException(f"unexpected test escape: {target}")
            patcher = mock.patch(target, side_effect=escape)
            patcher.start()
            self.addCleanup(patcher.stop)
