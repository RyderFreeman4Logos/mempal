#!/usr/bin/env python3
"""Crash-safe runtime primitives for the aggregate smoke test.

This module deliberately knows nothing about mempal commands or response
payloads.  It owns the two process-local contracts that must survive failures:
the cleanup receipt and the registry of child processes started by the runner.
"""
from __future__ import annotations

import json
import os
import select
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any, Callable, MutableMapping


MAX_CLEANUP_DRAWER_IDS = 1024
MAX_CLEANUP_DRAWER_ID_BYTES = 512
MAX_CLEANUP_MANIFEST_BYTES = 256 * 1024

__all__ = [
    "CleanupManifest",
    "MAX_CLEANUP_DRAWER_IDS",
    "MAX_CLEANUP_DRAWER_ID_BYTES",
    "MAX_CLEANUP_MANIFEST_BYTES",
    "McpClient",
    "OwnedSubprocessRegistry",
    "finalize_cleanup_manifest",
    "terminate_and_reap_owned_processes",
]


class CleanupManifest:
    """Persist only cleanup-authorized drawer IDs using atomic replacement."""

    def __init__(self, path: Path | None = None) -> None:
        if path is None:
            name = f"mempal-smoke-cleanup-{os.getpid()}-{os.urandom(8).hex()}.json"
            path = Path("/tmp") / name
        self.path = path
        self._pending: list[str] = []

    @property
    def pending_count(self) -> int:
        return len(self._pending)

    def checkpoint(self) -> None:
        """Atomically persist the current cleanup receipt with mode 0600."""
        self._checkpoint(self._pending)

    def _checkpoint(self, pending: list[str]) -> None:
        serialized = self._serialize(pending)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        temporary_path: Path | None = None
        try:
            with tempfile.NamedTemporaryFile(
                mode="w",
                encoding="utf-8",
                dir=self.path.parent,
                prefix=f".{self.path.name}.",
                delete=False,
            ) as handle:
                temporary_path = Path(handle.name)
                os.chmod(handle.fileno(), 0o600)
                handle.write(serialized)
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(temporary_path, self.path)
            self._pending = list(pending)
            self._fsync_parent()
        finally:
            if temporary_path is not None and temporary_path.exists():
                temporary_path.unlink()

    def add_created_ids(self, drawer_ids: list[str]) -> None:
        pending = list(self._pending)
        for drawer_id in drawer_ids:
            if not isinstance(drawer_id, str):
                raise ValueError("cleanup drawer IDs must be strings")
            drawer_id_bytes = len(drawer_id.encode("utf-8"))
            if drawer_id_bytes == 0 or drawer_id_bytes > MAX_CLEANUP_DRAWER_ID_BYTES:
                raise ValueError(
                    "cleanup drawer IDs must be non-empty and no longer than "
                    f"{MAX_CLEANUP_DRAWER_ID_BYTES} bytes"
                )
            if drawer_id not in pending:
                pending.append(drawer_id)
            if len(pending) > MAX_CLEANUP_DRAWER_IDS:
                raise ValueError(
                    f"cleanup manifest accepts at most {MAX_CLEANUP_DRAWER_IDS} drawer IDs"
                )
        self._checkpoint(pending)

    def mark_cleaned(self, drawer_ids: list[str]) -> None:
        cleaned = set(drawer_ids)
        pending = [drawer_id for drawer_id in self._pending if drawer_id not in cleaned]
        if pending:
            self._checkpoint(pending)
        else:
            self.discard()

    def discard(self) -> None:
        try:
            self.path.unlink()
        except FileNotFoundError:
            self._pending = []
            return
        self._pending = []
        self._fsync_parent()

    def _fsync_parent(self) -> None:
        directory_fd = os.open(self.path.parent, os.O_RDONLY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)

    @staticmethod
    def _serialize(pending: list[str]) -> str:
        serialized = json.dumps(
            {"cleanup_drawer_ids": pending},
            sort_keys=True,
            separators=(",", ":"),
        ) + "\n"
        if len(serialized.encode("utf-8")) > MAX_CLEANUP_MANIFEST_BYTES:
            raise ValueError(
                f"cleanup manifest exceeds {MAX_CLEANUP_MANIFEST_BYTES} bytes"
            )
        return serialized


def finalize_cleanup_manifest(
    manifest: CleanupManifest | None,
    summary: MutableMapping[str, Any],
) -> None:
    """Expose a recovery receipt only while cleanup-authorized IDs remain."""
    summary.pop("cleanup_manifest_path", None)
    summary.pop("cleanup_pending_count", None)
    if manifest is None:
        return
    if manifest.pending_count > 0:
        manifest.checkpoint()
        summary["cleanup_manifest_path"] = str(manifest.path)
        summary["cleanup_pending_count"] = manifest.pending_count
    else:
        manifest.discard()


class OwnedSubprocessRegistry:
    """Track runner-owned subprocesses and provide a bounded shutdown sweep."""

    def __init__(self) -> None:
        self.processes: dict[int, subprocess.Popen[Any]] = {}
        self._roles: dict[int, str] = {}

    def register(self, process: subprocess.Popen[Any], role: str) -> None:
        self.processes[process.pid] = process
        self._roles[process.pid] = role

    def unregister(self, process: subprocess.Popen[Any]) -> None:
        self.processes.pop(process.pid, None)
        self._roles.pop(process.pid, None)

    def terminate_and_reap(self, timeout: float = 1.0) -> dict[str, Any]:
        initial_ids = list(self.processes)
        role_counts: dict[str, int] = {}
        for process_id in initial_ids:
            role = self._roles.get(process_id, "unclassified")
            role_counts[role] = role_counts.get(role, 0) + 1
        receipt = terminate_and_reap_owned_processes(self.processes, timeout)
        for process_id in initial_ids:
            if process_id not in self.processes:
                self._roles.pop(process_id, None)
        receipt["roles"] = role_counts
        return receipt


class McpClient:
    """Owned JSON-RPC stdio client with bounded, observable shutdown."""

    def __init__(
        self,
        *,
        command: list[str],
        cwd: Path,
        registry: OwnedSubprocessRegistry,
        read_proc_io: Callable[[int | None], dict[str, int] | None],
        record_proc_io_delta: Callable[
            [str, dict[str, int] | None, dict[str, int] | None], None
        ],
        json_shape: Callable[[Any], Any],
        lifecycle_receipt: MutableMapping[str, Any],
        process_factory: Callable[..., subprocess.Popen[Any]] = subprocess.Popen,
    ) -> None:
        self.stderr_file = tempfile.TemporaryFile()
        self._registry = registry
        self._read_proc_io = read_proc_io
        self._record_proc_io_delta = record_proc_io_delta
        self._json_shape = json_shape
        self._lifecycle_receipt = lifecycle_receipt
        self.proc = process_factory(
            command,
            cwd=str(cwd),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr_file,
            text=True,
            bufsize=1,
        )
        registry.register(self.proc, "mcp_stdio")
        self.proc_io_before = read_proc_io(self.proc.pid)
        self.next_id = 1
        self._closed = False
        self._close_result = False
        self._hard_killed = False

    def __del__(self) -> None:
        stderr_file = getattr(self, "stderr_file", None)
        if stderr_file is not None and not stderr_file.closed:
            try:
                stderr_file.close()
            except OSError:
                pass

    def send(self, message: dict[str, Any]) -> None:
        if self.proc.stdin is None:
            raise RuntimeError("mcp stdin unavailable")
        self.proc.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
        self.proc.stdin.flush()

    def call(
        self,
        method: str,
        params: dict[str, Any] | None = None,
        timeout: int = 120,
    ) -> dict[str, Any]:
        message_id = self.next_id
        self.next_id += 1
        self.send(
            {
                "jsonrpc": "2.0",
                "id": message_id,
                "method": method,
                "params": params or {},
            }
        )
        return self.read_response(message_id, timeout)

    def notify(self, method: str, params: dict[str, Any] | None = None) -> None:
        message: dict[str, Any] = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self.send(message)

    def read_response(self, message_id: int, timeout: int) -> dict[str, Any]:
        if self.proc.stdout is None:
            raise RuntimeError("mcp stdout unavailable")
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            remaining = max(0.1, deadline - time.monotonic())
            ready, _, _ = select.select([self.proc.stdout], [], [], remaining)
            if not ready:
                continue
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("mcp eof")
            message = json.loads(line)
            if message.get("id") == message_id:
                return message
        raise TimeoutError(f"mcp response timeout for {message_id}")

    def tool(
        self,
        name: str,
        arguments: dict[str, Any],
        timeout: int = 120,
    ) -> tuple[dict[str, Any] | None, dict[str, Any]]:
        started_at = time.monotonic()
        try:
            message = self.call(
                "tools/call",
                {"name": name, "arguments": arguments},
                timeout=timeout,
            )
            elapsed_ms = int((time.monotonic() - started_at) * 1000)
            if "error" in message:
                encoded_error = json.dumps(message["error"]).encode()
                return None, {
                    "ok": False,
                    "latency_ms": elapsed_ms,
                    "error_code": message["error"].get("code"),
                    "error_message_bytes": len(encoded_error),
                }
            structured = message.get("result", {}).get("structuredContent")
            return structured, {
                "ok": isinstance(structured, dict),
                "latency_ms": elapsed_ms,
                "shape": self._json_shape(structured),
            }
        except Exception as error:
            return None, {"ok": False, "error_type": type(error).__name__}

    def close(self) -> bool:
        """Close the server with wait, terminate, and kill escalation."""
        if self._closed:
            return self._close_result
        self._closed = True
        killed = self._hard_killed
        try:
            self.notify("notifications/exit")
        except Exception:
            pass
        try:
            if self.proc.stdin is not None and not self.proc.stdin.closed:
                self.proc.stdin.close()
        except OSError:
            pass
        proc_io_after = self._read_proc_io(self.proc.pid)
        try:
            self.proc.wait(timeout=1)
        except subprocess.TimeoutExpired:
            try:
                self.proc.terminate()
            except ProcessLookupError:
                pass
            try:
                self.proc.wait(timeout=1)
            except subprocess.TimeoutExpired:
                try:
                    killed = True
                    self.proc.kill()
                except ProcessLookupError:
                    pass
                try:
                    self.proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    pass
        except (ChildProcessError, OSError):
            pass
        proc_io_after = proc_io_after or self._read_proc_io(self.proc.pid)
        self._record_proc_io_delta(
            "mcp_stdio_child_processes", self.proc_io_before, proc_io_after
        )
        self._record_lifecycle(killed)
        try:
            self.stderr_file.close()
        except OSError:
            pass
        _close_process_streams(self.proc)
        if self.proc.returncode is not None:
            self._registry.unregister(self.proc)
        self._close_result = self.proc.returncode is not None
        return self._close_result

    def _record_lifecycle(self, killed: bool) -> None:
        receipt = self._lifecycle_receipt
        receipt["process_count"] = int(receipt.get("process_count", 0)) + 1
        roles = receipt.setdefault("roles", {})
        roles["mcp_stdio"] = int(roles.get("mcp_stdio", 0)) + 1
        if self.proc.returncode is not None:
            receipt["exited_count"] = int(receipt.get("exited_count", 0)) + 1
        else:
            receipt.setdefault("exited_count", 0)
        if killed:
            receipt["killed_count"] = int(receipt.get("killed_count", 0)) + 1
        else:
            receipt.setdefault("killed_count", 0)


def terminate_and_reap_owned_processes(
    registry: MutableMapping[int, subprocess.Popen[Any]],
    timeout: float = 1.0,
) -> dict[str, Any]:
    """Terminate, kill if necessary, and reap every process in ``registry``.

    The returned receipt intentionally contains counts only.  Process IDs are
    operational data and must not enter the smoke JSON lifecycle summary.
    """
    initial_count = len(registry)
    reaped_count = 0
    killed_count = 0
    deadline = time.monotonic() + max(timeout, 0.0)

    for process_id, process in list(registry.items()):
        if process.poll() is None:
            try:
                process.terminate()
            except ProcessLookupError:
                pass
        remaining = max(0.0, deadline - time.monotonic())
        try:
            process.wait(timeout=remaining)
        except subprocess.TimeoutExpired:
            try:
                process.kill()
                killed_count += 1
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=max(timeout, 0.1))
            except subprocess.TimeoutExpired:
                continue
        except (ChildProcessError, OSError):
            pass
        _close_process_streams(process)
        registry.pop(process_id, None)
        reaped_count += 1

    return {
        "initial_count": initial_count,
        "reaped_count": reaped_count,
        "remaining_count": len(registry),
        "killed_count": killed_count,
    }


def _close_process_streams(process: subprocess.Popen[Any]) -> None:
    for stream in (process.stdin, process.stdout, process.stderr):
        if stream is None or stream.closed:
            continue
        try:
            stream.close()
        except OSError:
            pass
