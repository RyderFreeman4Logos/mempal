import contextlib
import errno
import io
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


PLUGIN_DIR = Path(__file__).resolve().parent
if str(PLUGIN_DIR) not in sys.path:
    sys.path.insert(0, str(PLUGIN_DIR))

import install_plugins as installer  # noqa: E402


class PluginInstallerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.hermes_home = Path(self.temporary.name) / "hermes"
        self.hermes_home.mkdir()

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def test_refresh_then_check_reports_current_for_both_plugins(self) -> None:
        refreshed = installer.refresh_plugins(PLUGIN_DIR, self.hermes_home)
        checked = installer.check_plugins(PLUGIN_DIR, self.hermes_home)

        self.assertTrue(all(result.current for result in refreshed))
        self.assertTrue(all(result.current for result in checked))
        self.assertEqual({result.plugin for result in checked}, set(installer.PLUGIN_NAMES))

    def test_check_detects_short_missing_and_extra_provider_files(self) -> None:
        installer.refresh_plugins(PLUGIN_DIR, self.hermes_home)
        provider = self.hermes_home / "plugins" / "mempal"
        (provider / "__init__.py").write_text("short", encoding="utf-8")
        (provider / "_write_spool.py").unlink()
        (provider / "_backoff.py").unlink()
        (provider / "_intelligence.py").unlink()
        (provider / "stale-provider-copy.py").write_text("stale", encoding="utf-8")

        result = self._result("mempal")

        self.assertIn("__init__.py", result.changed)
        self.assertIn("_write_spool.py", result.missing)
        self.assertIn("_backoff.py", result.missing)
        self.assertIn("_intelligence.py", result.missing)
        self.assertIn("stale-provider-copy.py", result.extra)

    def test_check_detects_and_refreshes_stale_hooks_copy(self) -> None:
        installer.refresh_plugins(PLUGIN_DIR, self.hermes_home)
        hooks = self.hermes_home / "plugins" / "mempal-hooks"
        (hooks / "__init__.py").write_text("stale hooks", encoding="utf-8")

        self.assertFalse(self._result("mempal-hooks").current)
        refreshed = installer.refresh_plugins(PLUGIN_DIR, self.hermes_home)

        self.assertTrue(all(result.current for result in refreshed))

    def test_activation_failure_rolls_back_both_original_copies(self) -> None:
        installer.refresh_plugins(PLUGIN_DIR, self.hermes_home)
        provider_marker = self.hermes_home / "plugins" / "mempal" / "local-marker"
        hooks_marker = self.hermes_home / "plugins" / "mempal-hooks" / "local-marker"
        provider_marker.write_text("provider-old", encoding="utf-8")
        hooks_marker.write_text("hooks-old", encoding="utf-8")

        def fail_second(plugin: str) -> None:
            if plugin == "mempal-hooks":
                raise OSError(errno.ENOSPC, "SECRET_PATH_OR_CONTENT")

        with self.assertRaisesRegex(installer.InstallerError, "no_space"):
            installer.refresh_plugins(
                PLUGIN_DIR,
                self.hermes_home,
                before_activate=fail_second,
            )

        self.assertEqual(provider_marker.read_text(encoding="utf-8"), "provider-old")
        self.assertEqual(hooks_marker.read_text(encoding="utf-8"), "hooks-old")
        self.assertEqual(self._transaction_artifacts(), [])

    def test_permission_failure_is_explicit_and_preserves_original_copy(self) -> None:
        installer.refresh_plugins(PLUGIN_DIR, self.hermes_home)
        marker = self.hermes_home / "plugins" / "mempal" / "local-marker"
        marker.write_text("original", encoding="utf-8")

        def deny_activation(_plugin: str) -> None:
            raise OSError(errno.EACCES, "SECRET_PATH_OR_CONTENT")

        with self.assertRaisesRegex(installer.InstallerError, "permission_denied"):
            installer.refresh_plugins(
                PLUGIN_DIR,
                self.hermes_home,
                before_activate=deny_activation,
            )

        self.assertEqual(marker.read_text(encoding="utf-8"), "original")
        self.assertEqual(self._transaction_artifacts(), [])

    def test_check_never_reads_extra_plugin_or_other_hermes_secrets(self) -> None:
        installer.refresh_plugins(PLUGIN_DIR, self.hermes_home)
        secret_paths = {
            self.hermes_home / "config-secret.json",
            self.hermes_home / "sessions" / "prompt-secret.jsonl",
            self.hermes_home / "state" / "spool-secret.sqlite3",
            self.hermes_home / "logs" / "hermes-secret.log",
            self.hermes_home / "plugins" / "mempal" / "extra-secret.log",
        }
        for path in secret_paths:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("SECRET_CONTENT", encoding="utf-8")
        original_digest = installer._digest_file
        digested = []

        def recording_digest(path: Path) -> str:
            digested.append(path)
            return original_digest(path)

        with mock.patch.object(installer, "_digest_file", side_effect=recording_digest):
            result = self._result("mempal")

        self.assertIn("extra-secret.log", result.extra)
        self.assertTrue(secret_paths.isdisjoint(digested))

    def test_check_and_refresh_reject_plugin_symlink(self) -> None:
        plugins = self.hermes_home / "plugins"
        plugins.mkdir()
        outside = Path(self.temporary.name) / "outside"
        outside.mkdir()
        (plugins / "mempal").symlink_to(outside, target_is_directory=True)

        with self.assertRaisesRegex(installer.InstallerError, "symlink_rejected"):
            installer.check_plugins(PLUGIN_DIR, self.hermes_home)
        with self.assertRaisesRegex(installer.InstallerError, "symlink_rejected"):
            installer.refresh_plugins(PLUGIN_DIR, self.hermes_home)

    def test_cli_check_and_explicit_refresh_use_selected_temporary_home(self) -> None:
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            self.assertEqual(installer.main([
                "--refresh", "--hermes-home", str(self.hermes_home),
            ]), 0)
            self.assertEqual(installer.main([
                "--check", "--hermes-home", str(self.hermes_home),
            ]), 0)

        rendered = output.getvalue()
        self.assertIn("current plugin=mempal", rendered)
        self.assertNotIn(str(self.hermes_home), rendered)

    def _result(self, plugin: str):
        return next(
            result
            for result in installer.check_plugins(PLUGIN_DIR, self.hermes_home)
            if result.plugin == plugin
        )

    def _transaction_artifacts(self):
        plugins = self.hermes_home / "plugins"
        return sorted(
            path.name
            for path in plugins.iterdir()
            if ".stage-" in path.name or ".backup-" in path.name
        )


if __name__ == "__main__":
    unittest.main()
