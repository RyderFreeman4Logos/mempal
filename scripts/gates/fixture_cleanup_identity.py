"""Linux mount and inode identity checks for owned gate fixtures."""

from __future__ import annotations

import errno
import os
import stat
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

__all__ = [
    "FixtureIdentity",
    "TreeSnapshot",
    "capture_fixture_identity",
    "entry_identity",
    "fixture_identity_matches",
    "fixture_tree_stays_on_mount",
    "mount_id",
    "parent_path",
]


@dataclass(frozen=True)
class FixtureIdentity:
    path: str
    dev: int
    ino: int
    uid: int
    mode: int
    parent: tuple[int, int]
    mount_id: int


def parent_path(path: str) -> str:
    parent = os.path.dirname(path.rstrip("/"))
    return parent if parent else "/"


def mount_id(fd: int) -> int:
    try:
        lines = Path(f"/proc/self/fdinfo/{fd}").read_bytes().splitlines()
    except OSError as error:
        raise OSError(errno.EOPNOTSUPP, "cannot read Linux mount identity") from error
    for line in lines:
        fields = line.split()
        if len(fields) == 2 and fields[0] == b"mnt_id:":
            try:
                return int(fields[1])
            except ValueError as error:
                raise OSError(errno.EINVAL, "invalid Linux mount identity") from error
    raise OSError(errno.EOPNOTSUPP, "Linux mount identity unavailable")


def capture_fixture_identity(path: str) -> FixtureIdentity:
    parent_fd = root_fd = None
    try:
        parent_fd = os.open(parent_path(path), os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        name = os.path.basename(path.rstrip("/"))
        root_fd = os.open(
            name,
            os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
            dir_fd=parent_fd,
        )
        root_stat = os.fstat(root_fd)
        if not stat.S_ISDIR(root_stat.st_mode):
            raise OSError("fixture root is not a directory")
        if stat.S_IMODE(root_stat.st_mode) != 0o700:
            raise OSError("fixture root mode is not 0700")
        parent_stat = os.fstat(parent_fd)
        root_mount_id = mount_id(root_fd)
        if root_mount_id != mount_id(parent_fd):
            raise OSError("fixture root is a mount point")
        return FixtureIdentity(
            path,
            root_stat.st_dev,
            root_stat.st_ino,
            root_stat.st_uid,
            stat.S_IMODE(root_stat.st_mode),
            (parent_stat.st_dev, parent_stat.st_ino),
            root_mount_id,
        )
    finally:
        if root_fd is not None:
            os.close(root_fd)
        if parent_fd is not None:
            os.close(parent_fd)


def fixture_identity_matches(path: str, expected: FixtureIdentity) -> bool:
    if path != expected.path:
        return False
    parent_fd = root_fd = None
    try:
        parent_fd = os.open(parent_path(path), os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        root_fd = os.open(
            os.path.basename(path.rstrip("/")),
            os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
            dir_fd=parent_fd,
        )
        root_stat = os.fstat(root_fd)
        parent_stat = os.fstat(parent_fd)
        return (
            stat.S_ISDIR(root_stat.st_mode)
            and stat.S_IMODE(root_stat.st_mode) == 0o700
            and root_stat.st_dev == expected.dev
            and root_stat.st_ino == expected.ino
            and root_stat.st_uid == expected.uid
            and (parent_stat.st_dev, parent_stat.st_ino) == expected.parent
            and mount_id(root_fd) == expected.mount_id
            and mount_id(parent_fd) == expected.mount_id
        )
    except OSError:
        return False
    finally:
        if root_fd is not None:
            os.close(root_fd)
        if parent_fd is not None:
            os.close(parent_fd)


def entry_identity(metadata: os.stat_result) -> tuple[int, int, int, int]:
    return metadata.st_dev, metadata.st_ino, stat.S_IFMT(metadata.st_mode), metadata.st_nlink


TreeSnapshot = dict[tuple[str, ...], dict[str, tuple[int, int, int, int]]]
MAX_FIXTURE_DEPTH = 512
MAX_FIXTURE_ENTRIES = 100_000


def fixture_tree_stays_on_mount(
    root_fd: int,
    expected_mount_id: int,
    check_cleanup: Callable[[], None],
) -> TreeSnapshot | None:
    snapshot: TreeSnapshot = {}
    entry_count = 0

    def capture(directory_fd: int, relative: tuple[str, ...], depth: int) -> bool:
        nonlocal entry_count
        check_cleanup()
        if depth > MAX_FIXTURE_DEPTH:
            return False
        children: dict[str, tuple[int, int, int, int]] = {}
        snapshot[relative] = children
        try:
            with os.scandir(directory_fd) as entries:
                iterator = iter(entries)
                while True:
                    check_cleanup()
                    try:
                        name = next(iterator).name
                    except StopIteration:
                        break
                    entry_count += 1
                    if entry_count > MAX_FIXTURE_ENTRIES:
                        return False
                    entry_fd = None
                    try:
                        metadata = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
                        flags = os.O_PATH | os.O_NOFOLLOW | os.O_CLOEXEC
                        if stat.S_ISDIR(metadata.st_mode):
                            flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
                        entry_fd = os.open(name, flags, dir_fd=directory_fd)
                        if entry_identity(os.fstat(entry_fd)) != entry_identity(metadata):
                            return False
                        children[name] = entry_identity(metadata)
                        if mount_id(entry_fd) != expected_mount_id:
                            return False
                        if stat.S_ISDIR(metadata.st_mode) and not capture(
                            entry_fd, relative + (name,), depth + 1
                        ):
                            return False
                    except OSError:
                        return False
                    finally:
                        if entry_fd is not None:
                            os.close(entry_fd)
        except OSError:
            return False
        return True

    try:
        return snapshot if capture(root_fd, (), 0) else None
    except RecursionError:
        return None
