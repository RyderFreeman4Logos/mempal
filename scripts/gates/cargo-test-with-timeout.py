#!/usr/bin/env python3
"""Run one command with Linux process-tree ownership and bounded cleanup."""

from __future__ import annotations

import ctypes
import errno
import os
import select
import shlex
import signal
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path


PR_SET_CHILD_SUBREAPER = 36
SYS_PIDFD_SEND_SIGNAL = 424
POLL_INTERVAL = 0.01
DISCOVERY_INTERVAL = 0.25
_pending_signal: int | None = None


@dataclass(frozen=True)
class Identity:
    pid: int
    start_time: int


@dataclass(frozen=True)
class Snapshot:
    identity: Identity
    parent_pid: int
    process_group: int
    session: int
    state: str


@dataclass
class OwnedProcess:
    identity: Identity | None
    pidfd: int | None = None
    exited: bool = False


class Supervisor:
    def __init__(self, child: subprocess.Popen[bytes], grace: float) -> None:
        self.child = child
        self.supervisor_pid = os.getpid()
        self.grace = grace
        self.owned: dict[int, OwnedProcess] = {}
        self.seen_identities: dict[int, Identity] = {}
        self.ownership_uncertain = False
        self._libc = ctypes.CDLL(None, use_errno=True)
        self._libc.syscall.restype = ctypes.c_long
        self._libc.syscall.argtypes = [
            ctypes.c_long,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_void_p,
            ctypes.c_uint,
        ]
        self._adopt_root_immediately()

    def _adopt_root_immediately(self) -> None:
        self.owned[self.child.pid] = OwnedProcess(None)
        try:
            snapshot = read_snapshot(self.child.pid)
        except (OSError, ValueError):
            self.ownership_uncertain = True
            return
        if snapshot is not None:
            self._adopt(snapshot)

    def _adopt(self, snapshot: Snapshot) -> None:
        pid = snapshot.identity.pid
        existing = self.owned.get(pid)
        if existing is not None:
            if existing.identity is not None:
                if existing.identity != snapshot.identity:
                    self.ownership_uncertain = True
                return
        previous = self.seen_identities.get(pid)
        if previous is not None and previous != snapshot.identity:
            self.ownership_uncertain = True
            return
        try:
            pidfd = open_pidfd(snapshot.identity)
        except (OSError, ValueError):
            self.ownership_uncertain = True
            return
        self.seen_identities[pid] = snapshot.identity
        if existing is not None:
            existing.identity = snapshot.identity
            existing.pidfd = pidfd
        else:
            self.owned[pid] = OwnedProcess(snapshot.identity, pidfd)

    def discover(self) -> dict[int, Snapshot]:
        try:
            snapshots = scan_snapshots()
        except (OSError, ValueError):
            self.ownership_uncertain = True
            root = self.owned.get(self.child.pid)
            if root is None or root.identity is None:
                return {}
            return {
                root.identity.pid: Snapshot(
                    identity=root.identity,
                    parent_pid=self.supervisor_pid,
                    process_group=root.identity.pid,
                    session=root.identity.pid,
                    state="R",
                )
            }
        owned_pids = set(self.owned)
        changed = True
        while changed:
            changed = False
            for snapshot in snapshots.values():
                pid = snapshot.identity.pid
                if pid == self.supervisor_pid or pid in self.owned:
                    continue
                if snapshot.parent_pid == self.supervisor_pid or snapshot.parent_pid in owned_pids:
                    before = len(self.owned)
                    self._adopt(snapshot)
                    if len(self.owned) != before:
                        owned_pids.add(pid)
                        changed = True
        return snapshots

    def reap_owned_children(self) -> None:
        root_status = self.child.poll()
        root = self.owned.get(self.child.pid)
        if root_status is not None and root is not None:
            root.exited = True
        for pid in tuple(self.owned):
            if pid == self.child.pid:
                continue
            while True:
                try:
                    waited_pid, _status = os.waitpid(pid, os.WNOHANG)
                except ChildProcessError:
                    break
                except OSError as error:
                    if error.errno in (errno.ECHILD, errno.ESRCH):
                        break
                    self.ownership_uncertain = True
                    break
                if waited_pid == 0:
                    break
                self.owned[pid].exited = True
                break

    def _group_targets(self, snapshots: dict[int, Snapshot]) -> list[tuple[int, Identity]]:
        identities = {handle.identity for handle in self.owned.values() if handle.identity}
        groups: dict[tuple[int, int], set[Identity]] = {}
        for snapshot in snapshots.values():
            if snapshot.identity in identities:
                groups.setdefault((snapshot.session, snapshot.process_group), set()).add(
                    snapshot.identity
                )
        safe_groups: list[tuple[int, Identity]] = []
        for (session, process_group), members in groups.items():
            if process_group <= 1 or session <= 1:
                continue
            leader = self.owned.get(process_group)
            if leader is None or leader.exited or leader.identity is None:
                continue
            session_members = {
                snapshot.identity
                for snapshot in snapshots.values()
                if snapshot.session == session and snapshot.process_group == process_group
            }
            if (
                leader.identity.pid == process_group
                and leader.identity in members
                and session_members
                and session_members <= identities
                and members == session_members
            ):
                safe_groups.append((process_group, leader.identity))
        return safe_groups

    def _leader_is_current(self, process_group: int, identity: Identity) -> bool:
        handle = self.owned.get(process_group)
        if handle is None or handle.exited or handle.identity != identity:
            return False
        if handle.pidfd is not None:
            try:
                ready, _write, _error = select.select([handle.pidfd], [], [], 0)
            except OSError:
                return False
            if ready:
                return False
        try:
            snapshot = read_snapshot(process_group)
        except (OSError, ValueError):
            self.ownership_uncertain = True
            return False
        return (
            snapshot is not None
            and snapshot.identity == identity
            and snapshot.process_group == process_group
            and snapshot.state not in ("Z", "X")
        )

    def _signal_pidfd(self, pidfd: int, signum: int) -> bool | None:
        result = self._libc.syscall(
            SYS_PIDFD_SEND_SIGNAL,
            pidfd,
            signum,
            ctypes.c_void_p(),
            0,
        )
        if result == 0:
            return True
        error_number = ctypes.get_errno()
        if error_number == errno.ESRCH:
            return True
        if error_number in (errno.ENOSYS, errno.EINVAL):
            return None
        return False

    def _signal_process(self, handle: OwnedProcess, signum: int) -> bool:
        if handle.identity is None:
            return False
        if handle.pidfd is None:
            # A start-time check followed by kill(2) has a PID-reuse race. Without
            # pidfd support, fail closed instead of signalling an unproven PID.
            return False
        result = self._signal_pidfd(handle.pidfd, signum)
        if result is True:
            return True
        if result is False:
            return False
        return False

    def signal_owned(self, signum: int, snapshots: dict[int, Snapshot]) -> bool:
        proved = True
        # Group signaling is only used for a complete, session-owned group. Escaped
        # sessions are handled by the identity-checked pass below.
        for process_group, leader_identity in self._group_targets(snapshots):
            if not self._leader_is_current(process_group, leader_identity):
                continue
            try:
                os.killpg(process_group, signum)
            except ProcessLookupError:
                pass
            except OSError:
                proved = False
        for handle in tuple(self.owned.values()):
            if handle.exited:
                continue
            if not self._signal_process(handle, signum):
                proved = False
        return proved

    def live_status(self, snapshots: dict[int, Snapshot]) -> tuple[bool, bool]:
        live = False
        unknown = self.ownership_uncertain
        for handle in self.owned.values():
            if handle.exited:
                continue
            identity = handle.identity
            if identity is None:
                unknown = True
                continue
            snapshot = snapshots.get(identity.pid)
            if snapshot is not None and snapshot.identity == identity:
                if snapshot.state not in ("Z", "X"):
                    live = True
                continue
            if handle.pidfd is not None:
                try:
                    ready, _write, _error = select.select([handle.pidfd], [], [], 0)
                except OSError:
                    unknown = True
                else:
                    if not ready:
                        live = True
                continue
            unknown = True
        return live, unknown

    def cleanup(self) -> bool:
        snapshots = self.discover()
        term_proved = self.signal_owned(signal.SIGTERM, snapshots)
        term_deadline = time.monotonic() + self.grace
        while time.monotonic() < term_deadline:
            snapshots = self.discover()
            self.reap_owned_children()
            live, unknown = self.live_status(snapshots)
            if not live and not unknown and term_proved:
                return True
            time.sleep(min(POLL_INTERVAL, max(0.0, term_deadline - time.monotonic())))

        snapshots = self.discover()
        kill_proved = self.signal_owned(signal.SIGKILL, snapshots)
        kill_deadline = time.monotonic() + self.grace
        while time.monotonic() < kill_deadline:
            snapshots = self.discover()
            self.reap_owned_children()
            live, unknown = self.live_status(snapshots)
            if not live and not unknown and kill_proved:
                return True
            time.sleep(min(POLL_INTERVAL, max(0.0, kill_deadline - time.monotonic())))

        snapshots = self.discover()
        self.reap_owned_children()
        live, unknown = self.live_status(snapshots)
        return not live and not unknown and kill_proved

    def process_context(self, snapshots: dict[int, Snapshot]) -> None:
        print("process tree:", file=sys.stderr)
        for snapshot in sorted(snapshots.values(), key=lambda item: item.identity.pid):
            if snapshot.identity.pid in self.owned:
                print(
                    f" pid={snapshot.identity.pid} ppid={snapshot.parent_pid}"
                    f" pgid={snapshot.process_group} sid={snapshot.session}"
                    f" state={snapshot.state}",
                    file=sys.stderr,
                )

    def close(self) -> None:
        for handle in self.owned.values():
            if handle.pidfd is not None:
                os.close(handle.pidfd)
                handle.pidfd = None

    def run(self, timeout: float) -> int:
        deadline = time.monotonic() + timeout
        next_discovery = time.monotonic()
        snapshots: dict[int, Snapshot] = {}
        try:
            while True:
                if _pending_signal is not None:
                    signum = _pending_signal
                    clean = self.cleanup()
                    if not clean:
                        print("failed to prove owned process cleanup", file=sys.stderr)
                        return 125
                    return 128 + signum
                status = self.child.poll()
                if status is not None:
                    clean = self.cleanup()
                    if not clean:
                        print("failed to prove owned process cleanup", file=sys.stderr)
                        return 125
                    return shell_status(status)
                if time.monotonic() >= deadline:
                    snapshots = self.discover()
                    timed_out = True
                    print(f"cargo test command timed out after {timeout:g}s", file=sys.stderr)
                    print(f"active command: {shlex.join(self.child.args)}", file=sys.stderr)
                    self.process_context(snapshots)
                    clean = self.cleanup()
                    if not clean:
                        print("failed to prove owned process cleanup", file=sys.stderr)
                        return 125
                    return 124
                if time.monotonic() >= next_discovery:
                    snapshots = self.discover()
                    next_discovery = time.monotonic() + DISCOVERY_INTERVAL
                time.sleep(POLL_INTERVAL)
        finally:
            self.close()


def shell_status(status: int) -> int:
    return 128 - status if status < 0 else status


def ensure_linux_subreaper() -> None:
    if sys.platform != "linux" or not Path("/proc").is_dir():
        raise RuntimeError("complete process ownership requires Linux /proc")
    libc = ctypes.CDLL(None, use_errno=True)
    result = libc.prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0)
    if result != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))


def open_pidfd(identity: Identity) -> int | None:
    if not hasattr(os, "pidfd_open"):
        return None
    try:
        pidfd = os.pidfd_open(identity.pid, 0)
    except OSError as error:
        if error.errno in (errno.ESRCH, errno.ENOSYS, errno.EINVAL, errno.EPERM):
            return None
        return None
    try:
        snapshot = read_snapshot(identity.pid)
    except (OSError, ValueError):
        os.close(pidfd)
        raise
    if snapshot is None or snapshot.identity != identity:
        os.close(pidfd)
        raise ValueError("pid identity changed while opening pidfd")
    return pidfd


def install_signal_handlers() -> None:
    def request_cleanup(signum: int, _frame: object) -> None:
        global _pending_signal
        _pending_signal = signum

    for signum in (signal.SIGHUP, signal.SIGQUIT, signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, request_cleanup)


def read_snapshot(pid: int) -> Snapshot | None:
    try:
        data = Path(f"/proc/{pid}/stat").read_bytes()
    except OSError as error:
        if error.errno in (errno.ENOENT, errno.ESRCH):
            return None
        raise
    closing_paren = data.rfind(b") ")
    if closing_paren < 0:
        raise ValueError(f"malformed /proc/{pid}/stat")
    fields = data[closing_paren + 2 :].split()
    if len(fields) < 20:
        raise ValueError(f"malformed /proc/{pid}/stat")
    try:
        snapshot_pid = int(data.split(b" (", 1)[0])
        state = fields[0].decode("ascii")
        if snapshot_pid != pid or len(state) != 1:
            raise ValueError
        return Snapshot(
            identity=Identity(snapshot_pid, int(fields[19])),
            parent_pid=int(fields[1]),
            process_group=int(fields[2]),
            session=int(fields[3]),
            state=state,
        )
    except (UnicodeError, ValueError) as error:
        raise ValueError(f"malformed /proc/{pid}/stat") from error


def scan_snapshots() -> dict[int, Snapshot]:
    snapshots: dict[int, Snapshot] = {}
    for path in Path("/proc").glob("[0-9]*/stat"):
        try:
            pid = int(path.parent.name)
        except ValueError:
            continue
        snapshot = read_snapshot(pid)
        if snapshot is not None:
            snapshots[pid] = snapshot
    return snapshots


def parse_positive_seconds(name: str, default: str) -> float:
    value = os.environ.get(name, default)
    if not value.isdigit() or int(value) <= 0:
        raise ValueError(f"{name} must be a positive integer")
    return float(int(value))


def main(argv: list[str]) -> int:
    if not argv:
        print(f"usage: {sys.argv[0]} <cargo-test-command> [args...]", file=sys.stderr)
        return 2
    try:
        timeout = parse_positive_seconds("MEMPAL_CARGO_TEST_TIMEOUT_SECS", "1800")
        grace = parse_positive_seconds("MEMPAL_CARGO_TEST_KILL_GRACE_SECS", "30")
        ensure_linux_subreaper()
    except (OSError, RuntimeError, ValueError) as error:
        print(str(error), file=sys.stderr)
        return 2

    install_signal_handlers()
    if _pending_signal is not None:
        return 128 + _pending_signal

    try:
        child = subprocess.Popen(argv, start_new_session=True)
    except OSError as error:
        print(f"failed to launch {shlex.join(argv)}: {error}", file=sys.stderr)
        return 127

    supervisor = Supervisor(child, grace)
    return supervisor.run(timeout)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
