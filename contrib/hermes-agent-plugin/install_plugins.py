#!/usr/bin/env python3
"""Verify or explicitly refresh the installed Hermes mempal plugins."""

from __future__ import annotations

import argparse
import errno
import hashlib
import os
import shutil
import stat
import sys
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Dict, Iterable, Optional, Sequence, Tuple


PLUGIN_NAMES = ("mempal", "mempal-hooks")
_REQUIRED_FILES = {
    "mempal": {
        "__init__.py",
        "_backoff.py",
        "_conclude.py",
        "_intelligence.py",
        "_rest_errors.py",
        "_write_spool.py",
        "plugin.yaml",
    },
    "mempal-hooks": {"__init__.py", "plugin.yaml"},
}
_IGNORED_DIRS = {"__pycache__"}
_IGNORED_SUFFIXES = {".pyc", ".pyo"}


class InstallerError(RuntimeError):
    """Content-free installer failure with a stable diagnostic class."""

    def __init__(self, kind: str) -> None:
        super().__init__(kind)
        self.kind = kind


@dataclass(frozen=True)
class PluginCheck:
    """Difference summary; paths are available to tests but never printed."""

    plugin: str
    expected_digest: str
    missing: Tuple[str, ...]
    extra: Tuple[str, ...]
    changed: Tuple[str, ...]

    @property
    def current(self) -> bool:
        return not (self.missing or self.extra or self.changed)


def check_plugins(source_root: Path, hermes_home: Path) -> Tuple[PluginCheck, ...]:
    """Compare only the two plugin directories without traversing other state."""
    source_root = _validate_root(source_root, must_exist=True)
    hermes_home = _validate_root(hermes_home, must_exist=True)
    plugins_root = hermes_home / "plugins"
    _reject_symlink(plugins_root)
    results = []
    for plugin in PLUGIN_NAMES:
        source = source_root / plugin
        target = plugins_root / plugin
        expected = _source_manifest(plugin, source)
        expected_digest = _manifest_digest(expected)
        if not target.exists():
            results.append(PluginCheck(
                plugin, expected_digest, tuple(expected), (), (),
            ))
            continue
        actual_paths = set(_list_files(target))
        expected_paths = set(expected)
        common = expected_paths & actual_paths
        changed = tuple(sorted(
            relative
            for relative in common
            if _digest_file(target / relative) != expected[relative]
        ))
        results.append(PluginCheck(
            plugin,
            expected_digest,
            tuple(sorted(expected_paths - actual_paths)),
            tuple(sorted(actual_paths - expected_paths)),
            changed,
        ))
    return tuple(results)


def refresh_plugins(
    source_root: Path,
    hermes_home: Path,
    *,
    before_activate: Optional[Callable[[str], None]] = None,
) -> Tuple[PluginCheck, ...]:
    """Stage, validate, and transactionally replace both plugin directories."""
    source_root = _validate_root(source_root, must_exist=True)
    hermes_home = _validate_root(hermes_home, must_exist=True)
    plugins_root = hermes_home / "plugins"
    _reject_symlink(plugins_root)
    try:
        plugins_root.mkdir(mode=0o700, parents=False, exist_ok=True)
    except OSError as exc:
        raise InstallerError(_os_error_kind(exc)) from None

    transaction = uuid.uuid4().hex
    stages: Dict[str, Path] = {}
    backups: Dict[str, Path] = {}
    try:
        for plugin in PLUGIN_NAMES:
            source = source_root / plugin
            expected = _source_manifest(plugin, source)
            stage = plugins_root / f".{plugin}.stage-{transaction}"
            _copy_tree(source, stage, expected)
            staged = _manifest(stage)
            if staged != expected:
                raise InstallerError("stage_verification_failed")
            stages[plugin] = stage

        for plugin in PLUGIN_NAMES:
            target = plugins_root / plugin
            _reject_tree_symlinks(target)
            if target.exists():
                backup = plugins_root / f".{plugin}.backup-{transaction}"
                os.replace(target, backup)
                backups[plugin] = backup
            if before_activate is not None:
                before_activate(plugin)
            os.replace(stages[plugin], target)
            _fsync_dir(plugins_root)

        results = check_plugins(source_root, hermes_home)
        if not all(result.current for result in results):
            raise InstallerError("post_refresh_verification_failed")
    except InstallerError:
        _rollback(plugins_root, stages, backups)
        raise
    except OSError as exc:
        _rollback(plugins_root, stages, backups)
        raise InstallerError(_os_error_kind(exc)) from None
    except Exception:
        _rollback(plugins_root, stages, backups)
        raise InstallerError("activation_failed") from None

    for backup in backups.values():
        _remove_tree(backup)
    _fsync_dir(plugins_root)
    return results


def _validate_root(path: Path, *, must_exist: bool) -> Path:
    candidate = Path(path).expanduser()
    _reject_symlink(candidate)
    if must_exist and (not candidate.exists() or not candidate.is_dir()):
        raise InstallerError("root_unavailable")
    return candidate


def _reject_symlink(path: Path) -> None:
    if path.is_symlink():
        raise InstallerError("symlink_rejected")


def _reject_tree_symlinks(root: Path) -> None:
    if root.exists():
        _list_files(root)


def _list_files(root: Path) -> Tuple[str, ...]:
    _reject_symlink(root)
    if not root.exists() or not root.is_dir():
        raise InstallerError("plugin_tree_unavailable")
    files = []
    stack = [(root, Path())]
    while stack:
        directory, relative_dir = stack.pop()
        try:
            entries = sorted(os.scandir(directory), key=lambda item: item.name)
        except OSError as exc:
            raise InstallerError(_os_error_kind(exc)) from None
        for entry in entries:
            relative = relative_dir / entry.name
            if entry.is_symlink():
                raise InstallerError("symlink_rejected")
            if entry.is_dir(follow_symlinks=False):
                if entry.name not in _IGNORED_DIRS:
                    stack.append((Path(entry.path), relative))
            elif entry.is_file(follow_symlinks=False):
                if Path(entry.name).suffix not in _IGNORED_SUFFIXES:
                    files.append(relative.as_posix())
            else:
                raise InstallerError("special_file_rejected")
    return tuple(sorted(files))


def _manifest(root: Path) -> Dict[str, str]:
    return {relative: _digest_file(root / relative) for relative in _list_files(root)}


def _source_manifest(plugin: str, root: Path) -> Dict[str, str]:
    manifest = _manifest(root)
    if not _REQUIRED_FILES[plugin].issubset(manifest):
        raise InstallerError("source_incomplete")
    return manifest


def _manifest_digest(manifest: Dict[str, str]) -> str:
    digest = hashlib.sha256()
    for relative, file_digest in sorted(manifest.items()):
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(file_digest.encode("ascii"))
        digest.update(b"\n")
    return digest.hexdigest()


def _digest_file(path: Path) -> str:
    digest = hashlib.sha256()
    descriptor = _open_read_only(path)
    with os.fdopen(descriptor, "rb") as handle:
        for chunk in iter(lambda: handle.read(128 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _open_read_only(path: Path) -> int:
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            os.close(descriptor)
            raise InstallerError("special_file_rejected")
        return descriptor
    except InstallerError:
        raise
    except OSError as exc:
        raise InstallerError(_os_error_kind(exc)) from None


def _copy_tree(source: Path, target: Path, manifest: Dict[str, str]) -> None:
    try:
        target.mkdir(mode=0o700)
        for relative in manifest:
            destination = target / relative
            destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            source_fd = _open_read_only(source / relative)
            with os.fdopen(source_fd, "rb") as source_handle:
                flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
                destination_fd = os.open(destination, flags, 0o600)
                with os.fdopen(destination_fd, "wb") as destination_handle:
                    shutil.copyfileobj(source_handle, destination_handle, 128 * 1024)
                    destination_handle.flush()
                    os.fsync(destination_handle.fileno())
        _fsync_dir(target)
    except InstallerError:
        _remove_tree(target)
        raise
    except OSError as exc:
        _remove_tree(target)
        raise InstallerError(_os_error_kind(exc)) from None


def _rollback(
    plugins_root: Path,
    stages: Dict[str, Path],
    backups: Dict[str, Path],
) -> None:
    rollback_failed = False
    for plugin in reversed(PLUGIN_NAMES):
        target = plugins_root / plugin
        backup = backups.get(plugin)
        try:
            if backup is not None and backup.exists():
                if target.exists():
                    _remove_tree(target)
                os.replace(backup, target)
            elif plugin in stages and not stages[plugin].exists() and target.exists():
                _remove_tree(target)
        except (OSError, InstallerError):
            rollback_failed = True
    for stage in stages.values():
        if stage.exists():
            try:
                _remove_tree(stage)
            except InstallerError:
                rollback_failed = True
    if rollback_failed:
        raise InstallerError("rollback_failed")
    _fsync_dir(plugins_root)


def _remove_tree(path: Path) -> None:
    if not path.exists():
        return
    _reject_tree_symlinks(path)
    try:
        shutil.rmtree(path)
    except OSError as exc:
        raise InstallerError(_os_error_kind(exc)) from None


def _fsync_dir(path: Path) -> None:
    try:
        descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
    except OSError as exc:
        raise InstallerError(_os_error_kind(exc)) from None


def _os_error_kind(exc: OSError) -> str:
    if exc.errno in {errno.EACCES, errno.EPERM}:
        return "permission_denied"
    if exc.errno in {errno.ENOSPC, errno.EDQUOT}:
        return "no_space"
    return "filesystem_error"


def _print_checks(results: Iterable[PluginCheck]) -> bool:
    current = True
    for result in results:
        if result.current:
            print(
                f"current plugin={result.plugin} "
                f"digest={result.expected_digest[:12]}"
            )
        else:
            current = False
            print(
                f"stale plugin={result.plugin} missing={len(result.missing)} "
                f"extra={len(result.extra)} changed={len(result.changed)}"
            )
    return current


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--refresh", action="store_true")
    parser.add_argument(
        "--hermes-home",
        type=Path,
        default=Path(os.environ.get("HERMES_HOME", "~/.hermes")),
    )
    args = parser.parse_args(argv)
    source_root = Path(__file__).resolve().parent
    try:
        if args.refresh:
            results = refresh_plugins(source_root, args.hermes_home)
        else:
            results = check_plugins(source_root, args.hermes_home)
    except InstallerError as exc:
        print(f"installer_error kind={exc.kind}", file=sys.stderr)
        return 2
    return 0 if _print_checks(results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
