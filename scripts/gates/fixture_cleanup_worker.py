"""Identity-owned, deadline-bounded removal of one gate fixture tree."""

from __future__ import annotations

import errno
import os
import signal
import stat
import sys
import time
from collections.abc import Callable

from fixture_cleanup_identity import (
    FixtureIdentity,
    TreeSnapshot,
    entry_identity,
    fixture_identity_matches,
    mount_id,
    parent_path,
)

__all__ = ["remove_owned_root"]


def _remove_snapshot(
    directory_fd: int,
    relative: tuple[str, ...],
    snapshot: TreeSnapshot,
    removed_links: dict[tuple[int, int, int], int],
    check_cleanup: Callable[[], None],
) -> None:
    check_cleanup()
    expected = snapshot[relative]
    seen = 0
    with os.scandir(directory_fd) as entries:
        iterator = iter(entries)
        while True:
            check_cleanup()
            try:
                name = next(iterator).name
            except StopIteration:
                break
            if name not in expected:
                raise OSError(errno.EBUSY, "fixture tree entries changed during cleanup")
            seen += 1
    if seen != len(expected):
        raise OSError(errno.EBUSY, "fixture tree entries changed during cleanup")
    for name in expected:
        check_cleanup()
        captured = expected[name]
        inode = captured[:3]
        removed = removed_links.get(inode, 0)
        expected_live = (*inode, captured[3] - removed)
        current = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
        if entry_identity(current) != expected_live:
            raise OSError(errno.EBUSY, "fixture tree entry identity changed during cleanup")
        if stat.S_ISDIR(current.st_mode):
            child_fd = os.open(
                name,
                os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                dir_fd=directory_fd,
            )
            try:
                if entry_identity(os.fstat(child_fd)) != expected_live:
                    raise OSError(errno.EBUSY, "fixture directory changed while opening")
                _remove_snapshot(
                    child_fd,
                    relative + (name,),
                    snapshot,
                    removed_links,
                    check_cleanup,
                )
                if entry_identity(
                    os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
                ) != entry_identity(os.fstat(child_fd)):
                    raise OSError(errno.EBUSY, "fixture directory changed before removal")
                os.rmdir(name, dir_fd=directory_fd)
            finally:
                os.close(child_fd)
        else:
            leaf_fd = os.open(
                name,
                os.O_PATH | os.O_NOFOLLOW | os.O_CLOEXEC,
                dir_fd=directory_fd,
            )
            try:
                if entry_identity(os.fstat(leaf_fd)) != expected_live:
                    raise OSError(errno.EBUSY, "fixture entry changed while opening")
                os.unlink(name, dir_fd=directory_fd)
                if os.fstat(leaf_fd).st_nlink != expected_live[3] - 1:
                    raise OSError(errno.EBUSY, "fixture entry changed during final removal")
                removed_links[inode] = removed + 1
            finally:
                os.close(leaf_fd)


def _remove_owned_root(
    identity: FixtureIdentity,
    deadline: float,
    cancelled: Callable[[], bool],
    capture_tree: Callable[[int, int, Callable[[], None]], TreeSnapshot | None],
) -> bool:
    # ponytail: Unix UID is the trust boundary; callers must quiesce same-UID fixture
    # mutation. These checks fail closed on observed drift, but only a kernel-enforced
    # credential/LSM/private-backing boundary can prevent a final pathname exchange.
    parent_fd = root_fd = None

    def check_cleanup() -> None:
        if cancelled():
            raise OSError(errno.EINTR, "fixture cleanup cancelled")
        if time.monotonic() >= deadline:
            raise OSError(errno.ETIMEDOUT, "fixture cleanup deadline exceeded")

    try:
        check_cleanup()
        parent_fd = os.open(
            parent_path(identity.path), os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC
        )
        parent_st = os.fstat(parent_fd)
        if (
            (parent_st.st_dev, parent_st.st_ino) != identity.parent
            or mount_id(parent_fd) != identity.mount_id
        ):
            raise OSError(errno.EBUSY, "fixture parent identity changed")
        name = os.path.basename(identity.path.rstrip("/"))
        root_fd = os.open(
            name,
            os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
            dir_fd=parent_fd,
        )
        root_st = os.fstat(root_fd)
        if (
            root_st.st_dev,
            root_st.st_ino,
            root_st.st_uid,
            stat.S_IMODE(root_st.st_mode),
        ) != (identity.dev, identity.ino, identity.uid, identity.mode):
            raise OSError(errno.EBUSY, "fixture root identity changed")
        if mount_id(root_fd) != identity.mount_id:
            raise OSError(errno.EBUSY, "fixture root mount identity changed")
        snapshot = capture_tree(root_fd, identity.mount_id, check_cleanup)
        if snapshot is None:
            raise OSError(errno.EBUSY, "fixture root contains a mount or unreadable entry")
        if not fixture_identity_matches(identity.path, identity):
            raise OSError(errno.EBUSY, "fixture root identity changed before removal")
        check_cleanup()
        _remove_snapshot(root_fd, (), snapshot, {}, check_cleanup)
        check_cleanup()
        if entry_identity(
            os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
        ) != entry_identity(os.fstat(root_fd)):
            raise OSError(errno.EBUSY, "fixture root identity changed before removal")
        os.rmdir(name, dir_fd=parent_fd)
        if os.fstat(root_fd).st_nlink != 0:
            raise OSError(errno.EBUSY, "fixture root changed during final removal")
    except (OSError, RecursionError) as error:
        print(f"failed to remove fixture root {identity.path}: {error}", file=sys.stderr)
        return False
    finally:
        if root_fd is not None:
            os.close(root_fd)
        if parent_fd is not None:
            os.close(parent_fd)
    if os.path.lexists(identity.path):
        print(f"fixture root still present: {identity.path}", file=sys.stderr)
        return False
    return True


def _read_worker(pid: int) -> tuple[int, str] | None:
    try:
        data = open(f"/proc/{pid}/stat", "rb").read()
    except FileNotFoundError:
        return None
    closing_paren = data.rfind(b") ")
    if closing_paren < 0:
        raise ValueError(f"malformed /proc/{pid}/stat")
    fields = data[closing_paren + 2 :].split()
    if len(fields) < 20:
        raise ValueError(f"malformed /proc/{pid}/stat")
    return int(fields[19]), fields[0].decode("ascii")


def _signal_worker(pid: int, start_time: int, signum: int) -> bool:
    try:
        current = _read_worker(pid)
    except (OSError, UnicodeError, ValueError):
        return False
    if current is None or current[1] in ("Z", "X"):
        return True
    if current[0] != start_time:
        return False
    try:
        os.kill(pid, signum)
    except ProcessLookupError:
        return True
    except OSError:
        return False
    return True


def _kill_direct_child(pid: int) -> None:
    # An unreaped direct child's PID cannot be recycled before waitpid observes it.
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except OSError as error:
        print(f"cannot stop fixture cleanup worker: {error}", file=sys.stderr)


def remove_owned_root(
    identity: FixtureIdentity,
    deadline: float,
    cancelled: Callable[[], bool],
    poll_interval: float,
    capture_tree: Callable[[int, int, Callable[[], None]], TreeSnapshot | None],
) -> bool:
    sys.stdout.flush()
    sys.stderr.flush()
    try:
        worker_pid = os.fork()
    except OSError as error:
        print(f"failed to start fixture cleanup worker: {error}", file=sys.stderr)
        return False
    if worker_pid == 0:
        for signum in (signal.SIGHUP, signal.SIGQUIT, signal.SIGINT, signal.SIGTERM):
            signal.signal(signum, signal.SIG_DFL)
        succeeded = _remove_owned_root(identity, deadline, cancelled, capture_tree)
        sys.stderr.flush()
        os._exit(0 if succeeded else 1)

    try:
        worker = _read_worker(worker_pid)
    except (OSError, UnicodeError, ValueError) as error:
        print(f"cannot capture fixture cleanup worker identity: {error}", file=sys.stderr)
        worker = None
    if worker is None:
        _kill_direct_child(worker_pid)
        start_time = None
    else:
        start_time, _state = worker

    was_cancelled = False
    term_sent = False
    kill_sent = start_time is None
    term_at = max(time.monotonic(), deadline - 0.25)
    kill_at = max(term_at, deadline - 0.1)
    while True:
        waited_pid, status = os.waitpid(worker_pid, os.WNOHANG)
        if waited_pid == worker_pid:
            return (
                start_time is not None
                and not (was_cancelled or cancelled())
                and os.waitstatus_to_exitcode(status) == 0
            )

        now = time.monotonic()
        was_cancelled = was_cancelled or cancelled()
        if not term_sent and (was_cancelled or now >= term_at):
            if start_time is None or not _signal_worker(worker_pid, start_time, signal.SIGTERM):
                _kill_direct_child(worker_pid)
                kill_sent = True
            term_sent = True
            if was_cancelled:
                kill_at = min(kill_at, now + (2 * poll_interval))
        if term_sent and not kill_sent and now >= kill_at:
            if start_time is None or not _signal_worker(worker_pid, start_time, signal.SIGKILL):
                _kill_direct_child(worker_pid)
            kill_sent = True
        if now >= deadline:
            waited_pid, _status = os.waitpid(worker_pid, os.WNOHANG)
            if waited_pid != worker_pid:
                print(
                    "fixture cleanup worker termination requested but kernel completion unconfirmed",
                    file=sys.stderr,
                )
            return False
        time.sleep(min(poll_interval, max(0.0, deadline - now)))
