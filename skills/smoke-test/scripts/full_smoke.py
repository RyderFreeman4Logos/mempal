#!/usr/bin/env python3
"""Aggregate-only mempal smoke runner for repo-local skills/smoke-test.

Exercises CLI and MCP CRUD without printing drawer content or raw command output.
"""
from __future__ import annotations

import json
import os
import select
import signal
import subprocess
import sys
import tempfile
import time
import resource
from pathlib import Path
from typing import Any

REPO = Path(__file__).resolve().parents[3]
MARKER = f"mempal-skill-smoke-{int(time.time())}-{os.getpid()}"
NONCE = os.urandom(16).hex()
SUMMARY: dict[str, Any] = {
    'marker_hash': None,
    'groups': {},
    'cleanup': {'cli_deleted_count': 0, 'mcp_deleted_count': 0, 'failures': 0},
    'created_counts': {'cli': 0, 'mcp': 0},
    'failures': [],
    'mcp_ingest_fallback_to_cli': 0,
    'io': {
        'schema': 'mempal_smoke_io_v2',
        'included_sources': [
            'daemon_proc_io_delta_when_pid_stable',
            'cli_child_proc_io_delta',
            'mcp_stdio_child_proc_io_delta',
            'children_resource_block_io_delta',
        ],
    },
}
OWNED_MCP_CHILDREN: dict[int, subprocess.Popen[Any]] = {}

PROC_IO_KEYS = ('read_bytes', 'write_bytes', 'cancelled_write_bytes', 'rchar', 'wchar')


def note(name: str, ok: bool, **fields: Any) -> None:
    safe = {'ok': ok}
    safe.update(fields)
    SUMMARY['groups'][name] = safe
    SUMMARY['failures'] = [failure for failure in SUMMARY['failures'] if failure != name]
    if not ok:
        SUMMARY['failures'].append(name)


def without_ok(info: dict[str, Any]) -> dict[str, Any]:
    return {key: value for key, value in info.items() if key != 'ok'}


def daemon_main_pid() -> int | None:
    try:
        proc = run_child_process(
            ['systemctl', '--user', 'show', 'mempal-daemon.service', '-p', 'MainPID'],
            timeout=10,
            io_category='cli_child_processes',
        )
        stdout = proc['stdout'].decode('utf-8', errors='replace')
        for line in stdout.splitlines():
            if line.startswith('MainPID='):
                value = int(line.split('=', 1)[1])
                return value or None
    except Exception:
        return None
    return None


def daemon_exe_path(pid: int | None) -> str | None:
    """Return the on-disk path of the running daemon's executable.

    Returns ``None`` if the PID is unknown or the link cannot be read.
    On Linux, ``/proc/<pid>/exe`` may carry a `` (deleted)`` suffix when
    the binary has been replaced on disk but the daemon has not been
    restarted — a silent regression that the smoke test must surface.
    """
    if pid is None or pid == 0:
        return None
    try:
        return os.readlink(f'/proc/{pid}/exe')
    except Exception:
        return None


def installed_binary_path() -> str | None:
    """Return the ``command -v mempal`` result, or None."""
    import shutil
    return shutil.which('mempal')


def read_proc_io(pid: int | None) -> dict[str, int] | None:
    if pid is None:
        return None
    try:
        data = Path(f'/proc/{pid}/io').read_text()
    except Exception:
        return None
    keys = {'read_bytes', 'write_bytes', 'cancelled_write_bytes', 'rchar', 'wchar'}
    parsed: dict[str, int] = {}
    for line in data.splitlines():
        if ':' not in line:
            continue
        key, value = line.split(':', 1)
        if key in keys:
            try:
                parsed[key] = int(value.strip())
            except ValueError:
                pass
    return parsed


def io_delta(before: dict[str, int] | None, after: dict[str, int] | None) -> dict[str, int] | None:
    if before is None or after is None:
        return None
    return {key: after.get(key, 0) - before.get(key, 0) for key in sorted(set(before) | set(after))}


def proc_io_aggregate() -> dict[str, Any]:
    return {
        'process_count': 0,
        'sampled_process_count': 0,
        'missing_process_count': 0,
        **{key: 0 for key in PROC_IO_KEYS},
    }


def record_proc_io_delta(category: str, before: dict[str, int] | None, after: dict[str, int] | None) -> None:
    aggregate = SUMMARY['io'].setdefault(category, proc_io_aggregate())
    aggregate['process_count'] += 1
    delta = io_delta(before, after)
    if delta is None:
        aggregate['missing_process_count'] += 1
        return
    aggregate['sampled_process_count'] += 1
    for key in PROC_IO_KEYS:
        aggregate[key] += max(0, int(delta.get(key, 0)))


def wait_exited_without_reap(pid: int, timeout: float) -> bool | None:
    if not hasattr(os, 'waitid') or not hasattr(os, 'P_PID'):
        return None
    deadline = time.monotonic() + timeout
    flags = os.WEXITED | os.WNOHANG | getattr(os, 'WNOWAIT', 0)
    while True:
        try:
            info = os.waitid(os.P_PID, pid, flags)
        except ChildProcessError:
            return None
        if info is not None:
            return True
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.02)


def read_tempfile_bytes(handle: Any) -> bytes:
    handle.flush()
    handle.seek(0)
    return handle.read()


def run_child_process(
    args: list[str],
    *,
    input_text: str | None = None,
    timeout: int = 120,
    io_category: str,
) -> dict[str, Any]:
    stdout_file = tempfile.TemporaryFile()
    stderr_file = tempfile.TemporaryFile()
    start = time.monotonic()
    timed_out = False
    killed = False
    proc: subprocess.Popen[bytes] | None = None
    try:
        proc = subprocess.Popen(
            args,
            cwd=REPO,
            stdin=subprocess.PIPE if input_text is not None else subprocess.DEVNULL,
            stdout=stdout_file,
            stderr=stderr_file,
        )
        before = read_proc_io(proc.pid)
        if input_text is not None and proc.stdin is not None:
            try:
                proc.stdin.write(input_text.encode())
                proc.stdin.close()
            except BrokenPipeError:
                pass

        exited = wait_exited_without_reap(proc.pid, timeout)
        if exited is False:
            timed_out = True
            killed = True
            proc.kill()
            wait_exited_without_reap(proc.pid, 5)
        elif exited is None:
            try:
                proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                timed_out = True
                killed = True
                proc.kill()
                proc.wait(timeout=5)

        after = read_proc_io(proc.pid)
        return_code = proc.wait(timeout=5)
        record_proc_io_delta(io_category, before, after)
    except subprocess.TimeoutExpired:
        timed_out = True
        killed = True
        if proc is not None:
            try:
                proc.kill()
            except Exception:
                pass
            try:
                proc.wait(timeout=5)
            except Exception:
                pass
        return_code = 124
        if proc is not None:
            record_proc_io_delta(io_category, None, None)
    except Exception as exc:
        if proc is not None:
            try:
                proc.kill()
            except Exception:
                pass
            try:
                proc.wait(timeout=5)
            except Exception:
                pass
            record_proc_io_delta(io_category, None, None)
        return {
            'returncode': 125,
            'stdout': read_tempfile_bytes(stdout_file),
            'stderr': read_tempfile_bytes(stderr_file),
            'latency_ms': int((time.monotonic() - start) * 1000),
            'timed_out': False,
            'killed': proc is not None,
            'error_type': type(exc).__name__,
        }
    finally:
        try:
            if proc is not None and proc.stdin is not None and not proc.stdin.closed:
                proc.stdin.close()
        except Exception:
            pass

    elapsed = int((time.monotonic() - start) * 1000)
    return {
        'returncode': 124 if timed_out else return_code,
        'stdout': read_tempfile_bytes(stdout_file),
        'stderr': read_tempfile_bytes(stderr_file),
        'latency_ms': elapsed,
        'timed_out': timed_out,
        'killed': killed,
    }


def child_io_blocks_snapshot() -> dict[str, int]:
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    return {'ru_inblock': int(usage.ru_inblock), 'ru_oublock': int(usage.ru_oublock)}


def child_io_blocks_delta(before: dict[str, int], after: dict[str, int]) -> dict[str, int]:
    return {key: after.get(key, 0) - before.get(key, 0) for key in sorted(set(before) | set(after))}


def json_shape(value: Any) -> dict[str, Any]:
    if isinstance(value, dict):
        out: dict[str, Any] = {'type': 'object', 'field_count': len(value), 'fields': sorted(value.keys())[:30]}
        # Count common collection fields without exposing values.
        for key in ('results', 'drawers', 'entries', 'facts', 'tools', 'system_warnings'):
            if isinstance(value.get(key), list):
                out[f'{key}_count'] = len(value[key])
        return out
    if isinstance(value, list):
        return {'type': 'array', 'count': len(value)}
    return {'type': type(value).__name__}


def parse_json_bytes(data: bytes) -> tuple[Any | None, dict[str, Any]]:
    text = data.decode('utf-8', errors='replace')
    try:
        value = json.loads(text or 'null')
        return value, {'ok': True, **json_shape(value)}
    except Exception as exc:
        lines = [line for line in text.splitlines() if line.strip()]
        parsed = []
        try:
            for line in lines:
                parsed.append(json.loads(line))
            return parsed, {'ok': True, 'type': 'ndjson', 'line_count': len(parsed)}
        except Exception:
            return None, {'ok': False, 'error_type': type(exc).__name__, 'line_count': len(lines)}


def run_cli(name: str, args: list[str], *, input_text: str | None = None, expect_json: bool = False, timeout: int = 120) -> tuple[int, bytes, bytes, Any | None, dict[str, Any]]:
    result = run_child_process(
        args,
        input_text=input_text,
        timeout=timeout,
        io_category='cli_child_processes',
    )
    parsed = None
    shape: dict[str, Any] = {}
    if expect_json:
        parsed, shape = parse_json_bytes(result['stdout'])
    ok = result['returncode'] == 0 and (not expect_json or shape.get('ok') is True)
    note(
        name,
        ok,
        exit_code=result['returncode'],
        latency_ms=result['latency_ms'],
        stdout_bytes=len(result['stdout']),
        stderr_bytes=len(result['stderr']),
        stderr_class=classify_stderr(result['stderr']) if result['stderr'] else None,
        timeout=result.get('timed_out') or None,
        killed=result.get('killed') or None,
        json=shape or None,
    )
    return result['returncode'], result['stdout'], result['stderr'], parsed, shape


def classify_stderr(data: bytes) -> str | None:
    """Classify stderr output, filtering known informational noise.

    ``config hot-reload: bootstrapped version ...`` is emitted by every mempal
    CLI invocation and carries no signal for smoke purposes. If we leave it
    unfiltered, every probe reports ``stderr_class=stderr_present`` with 53
    bytes of noise, which masks genuine errors hidden in the same payload.
    """
    text_raw = data.decode('utf-8', errors='replace')
    noise_prefixes = (
        'config hot-reload:',
    )
    filtered_lines = [
        line for line in text_raw.splitlines()
        if line.strip() and not any(line.strip().startswith(p) for p in noise_prefixes)
    ]
    if not filtered_lines:
        return None
    text = '\n'.join(filtered_lines).lower()
    if 'classification=extra_holder' in text or 'extra process holding' in text:
        return 'database_lock_extra_holder'
    if 'database is locked' in text or 'sqlite' in text and 'locked' in text:
        return 'database_locked'
    if 'operation' in text and 'not found' in text:
        return 'operation_not_found'
    if 'timed out' in text or 'timeout' in text:
        return 'timeout'
    if 'degraded' in text and 'write' in text:
        return 'write_degraded'
    if 'error:' in text:
        return 'error'
    return 'stderr_present'


def receipt_dicts_from(value: Any) -> list[dict[str, Any]]:
    """Return operation-style receipt dicts without parsing raw text payloads."""
    receipts: list[dict[str, Any]] = []
    if isinstance(value, dict):
        receipts.append(value)
        for key in ('structuredContent', 'result', 'payload', 'response'):
            nested = value.get(key)
            if isinstance(nested, (dict, list)):
                receipts.extend(receipt_dicts_from(nested))
        return receipts
    if isinstance(value, list):
        for item in value:
            receipts.extend(receipt_dicts_from(item))
    return receipts


def created_ids_from(value: Any) -> list[str]:
    ids: list[str] = []
    for receipt in receipt_dicts_from(value):
        for key in ('created_drawer_ids', 'cleanup_drawer_ids'):
            values = receipt.get(key)
            if isinstance(values, list):
                ids.extend(x for x in values if isinstance(x, str) and x)
    return list(dict.fromkeys(ids))


def terminal_state(value: Any) -> bool:
    return operation_state_from(value) in {'completed', 'rejected', 'failed'}


def operation_id_from(value: Any) -> str | None:
    for receipt in receipt_dicts_from(value):
        operation_id = receipt.get('operation_id')
        if isinstance(operation_id, str) and operation_id:
            return operation_id
    return None


def operation_state_from(value: Any) -> str | None:
    last_state: str | None = None
    for receipt in receipt_dicts_from(value):
        state = receipt.get('state')
        if isinstance(state, str) and state:
            if state in {'completed', 'rejected', 'failed'}:
                return state
            last_state = state
    return last_state


def count_marker_matches(value: Any, room: str) -> int:
    if isinstance(value, dict):
        results = value.get('results')
    else:
        results = value
    if not isinstance(results, list):
        return 0
    count = 0
    for item in results:
        if not isinstance(item, dict):
            continue
        if item.get('wing') == 'smoke' and item.get('room') == room and MARKER in str(item.get('content', '')):
            count += 1
    return count


def delete_exact_ids_cli(drawer_ids: list[str], label: str, room: str | None = None) -> dict[str, Any]:
    unique_ids = list(dict.fromkeys(drawer_ids))
    deleted = 0
    failed = 0
    stdout_bytes = 0
    stderr_bytes = 0
    for drawer_id in unique_ids:
        proc = run_child_process(
            ['mempal', 'delete', drawer_id],
            timeout=60,
            io_category='cli_child_processes',
        )
        stdout_bytes += len(proc['stdout'])
        stderr_bytes += len(proc['stderr'])
        if proc['returncode'] == 0:
            deleted += 1
        else:
            failed += 1
    active_matches_after_failed_deletes: int | None = None
    if failed > 0 and deleted == 0 and room is not None:
        rc, _out, _err, parsed, _shape = run_cli(
            label + '_post_cleanup_search',
            ['mempal', 'search', MARKER, '--top-k', '5', '--json'],
            expect_json=True,
            timeout=180,
        )
        if rc == 0:
            active_matches_after_failed_deletes = count_marker_matches(parsed, room)
    result = {
        'attempted_count': len(unique_ids),
        'deleted_count': deleted,
        'failed_count': failed,
        'stdout_bytes': stdout_bytes,
        'stderr_bytes': stderr_bytes,
    }
    if active_matches_after_failed_deletes is not None:
        result['active_matches_after_failed_deletes'] = active_matches_after_failed_deletes
    note(
        label,
        failed == 0
        or deleted > 0
        or not unique_ids
        or active_matches_after_failed_deletes == 0,
        **result,
    )
    return result


def wait_operation(operation_id: str, name: str) -> Any | None:
    rc, out, _err, parsed, _shape = run_cli(name, ['mempal', 'operation', 'wait', operation_id, '--timeout-secs', '300', '--json'], expect_json=True, timeout=330)
    if created_ids_from(parsed):
        return parsed
    if rc != 0 or not terminal_state(parsed):
        rc, out, _err, parsed, _shape = run_cli(name + '_status', ['mempal', 'operation', 'status', operation_id, '--json'], expect_json=True, timeout=30)
    return parsed


def recover_created_ids(value: Any, wait_label: str) -> tuple[list[str], dict[str, Any]]:
    ids = created_ids_from(value)
    operation_id = operation_id_from(value)
    info: dict[str, Any] = {
        'operation_id_present': bool(operation_id),
        'operation_state': operation_state_from(value),
        'recovered_via': None,
        'recovered_state': None,
    }
    if ids or operation_id is None:
        return ids, info

    waited = wait_operation(operation_id, wait_label)
    ids = created_ids_from(waited)
    info['recovered_via'] = wait_label
    info['recovered_state'] = operation_state_from(waited)
    return ids, info


def recovery_fields(info: dict[str, Any]) -> dict[str, Any]:
    return {key: value for key, value in info.items() if value not in (None, False)}


def cli_crud() -> list[str]:
    cleanup_ids: list[str] = []
    content = json.dumps({'content': f'{MARKER} reversible CLI smoke drawer; nonce {NONCE}; lexical tokens quorvax nimbledrift zettaplum; safe to delete', 'wing': 'smoke', 'room': 'cli', 'source_type': 'agent_inference', 'memory_kind': 'evidence', 'domain': 'project', 'field': 'smoke'}) + '\n'
    rc, _out, _err, parsed, _shape = run_cli(
        'cli_create',
        ['mempal', 'ingest', '--stdin', '--wing', 'smoke', '--room', 'cli', '--source-type', 'agent_inference', '--memory-kind', 'evidence', '--domain', 'project', '--field', 'smoke', '--no-gate', '--wait', '--wait-timeout-secs', '90', '--json'],
        input_text=content,
        expect_json=True,
        timeout=130,
    )
    ids, create_recovery = recover_created_ids(parsed, 'cli_create_wait')
    if ids and create_recovery.get('recovered_via'):
        note('cli_create', True, created_id_count=len(ids), **recovery_fields(create_recovery))
    if not ids:
        note('cli_crud', False, reason='create_missing_created_drawer_ids', **recovery_fields(create_recovery))
        return cleanup_ids
    cleanup_ids.extend(ids)
    created_id = ids[0]
    SUMMARY['created_counts']['cli'] = len(cleanup_ids)

    run_cli('cli_read_view', ['mempal', 'view', created_id, '--all-projects'], timeout=60)
    _rc, _out, _err, search_parsed, _shape = run_cli('cli_search_created', ['mempal', 'search', MARKER, '--top-k', '5', '--json'], expect_json=True, timeout=180)
    matches = count_marker_matches(search_parsed, 'cli')
    note('cli_search_created_match', matches > 0, active_matches=matches)
    run_cli('cli_context_created', ['mempal', 'context', MARKER, '--format', 'json', '--max-items', '3', '--no-distill-suggestions'], expect_json=True, timeout=150)
    run_cli('cli_pinned_before', ['mempal', 'pinned', '--json'], expect_json=True, timeout=60)
    run_cli('cli_pin', ['mempal', 'pin', created_id], timeout=60)
    run_cli('cli_unpin', ['mempal', 'unpin', created_id], timeout=60)
    run_cli('cli_pinned_after', ['mempal', 'pinned', '--json'], expect_json=True, timeout=60)

    update_content = json.dumps({'content': f'{MARKER} reversible CLI smoke drawer updated; nonce {NONCE[::-1]}; lexical tokens ploverquartz rivetmint yondercoil; safe to delete', 'wing': 'smoke', 'room': 'cli', 'source_type': 'agent_inference', 'memory_kind': 'evidence', 'domain': 'project', 'field': 'smoke'}) + '\n'
    rc, _out, _err, upd_parsed, _shape = run_cli(
        'cli_update',
        ['mempal', 'ingest', '--stdin', '--wing', 'smoke', '--room', 'cli', '--source-type', 'agent_inference', '--memory-kind', 'evidence', '--domain', 'project', '--field', 'smoke', '--no-gate', '--supersedes', created_id, '--wait', '--wait-timeout-secs', '90', '--json'],
        input_text=update_content,
        expect_json=True,
        timeout=130,
    )
    upd_ids, update_recovery = recover_created_ids(upd_parsed, 'cli_update_wait')
    if upd_ids and update_recovery.get('recovered_via'):
        note('cli_update', True, created_id_count=len(upd_ids), **recovery_fields(update_recovery))
    if not upd_ids:
        delete_exact_ids_cli(cleanup_ids, 'cli_cleanup_after_update_failure', room='cli')
        note('cli_crud', False, reason='update_missing_created_drawer_ids', cleanup_id_count=len(cleanup_ids), **recovery_fields(update_recovery))
        return cleanup_ids
    cleanup_ids.extend(upd_ids)
    SUMMARY['created_counts']['cli'] = len(cleanup_ids)
    run_cli('cli_read_updated', ['mempal', 'view', upd_ids[0], '--all-projects'], timeout=60)

    delete_result = delete_exact_ids_cli(cleanup_ids, 'cli_delete_batch')
    deleted = int(delete_result['deleted_count'])
    delete_failures = int(delete_result['failed_count'])
    SUMMARY['cleanup']['cli_deleted_count'] = deleted
    _rc, _out, _err, post_parsed, _shape = run_cli('cli_search_post_delete', ['mempal', 'search', MARKER, '--top-k', '5', '--json'], expect_json=True, timeout=180)
    post_matches = count_marker_matches(post_parsed, 'cli')
    if post_matches > 0:
        SUMMARY['cleanup']['failures'] += delete_failures
    note('cli_crud', post_matches == 0 and deleted > 0, created_id_count=len(cleanup_ids), deleted_count=deleted, delete_failed_attempt_count=delete_failures, post_delete_active_matches=post_matches)
    return cleanup_ids


class McpClient:
    def __init__(self) -> None:
        self.stderr_file = tempfile.TemporaryFile()
        self.proc_io_before: dict[str, int] | None = None
        self.proc = subprocess.Popen(
            ['mempal', 'serve', '--mcp'],
            cwd=str(REPO),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr_file,
            text=True,
            bufsize=1,
        )
        OWNED_MCP_CHILDREN[self.proc.pid] = self.proc
        self.proc_io_before = read_proc_io(self.proc.pid)
        self.next_id = 1

    def send(self, msg: dict[str, Any]) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(msg, separators=(',', ':')) + '\n')
        self.proc.stdin.flush()

    def call(self, method: str, params: dict[str, Any] | None = None, timeout: int = 120) -> dict[str, Any]:
        msg_id = self.next_id
        self.next_id += 1
        self.send({'jsonrpc': '2.0', 'id': msg_id, 'method': method, 'params': params or {}})
        return self.read_response(msg_id, timeout)

    def notify(self, method: str, params: dict[str, Any] | None = None) -> None:
        msg: dict[str, Any] = {'jsonrpc': '2.0', 'method': method}
        if params is not None:
            msg['params'] = params
        self.send(msg)

    def read_response(self, msg_id: int, timeout: int) -> dict[str, Any]:
        assert self.proc.stdout is not None
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            remaining = max(0.1, end - time.monotonic())
            ready, _, _ = select.select([self.proc.stdout], [], [], remaining)
            if not ready:
                continue
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError('mcp eof')
            msg = json.loads(line)
            if msg.get('id') == msg_id:
                return msg
        raise TimeoutError(f'mcp response timeout for {msg_id}')

    def tool(self, name: str, arguments: dict[str, Any], timeout: int = 120) -> tuple[dict[str, Any] | None, dict[str, Any]]:
        start = time.monotonic()
        try:
            msg = self.call('tools/call', {'name': name, 'arguments': arguments}, timeout=timeout)
            elapsed = int((time.monotonic() - start) * 1000)
            if 'error' in msg:
                return None, {'ok': False, 'latency_ms': elapsed, 'error_code': msg['error'].get('code'), 'error_message_bytes': len(json.dumps(msg['error']).encode())}
            result = msg.get('result', {})
            structured = result.get('structuredContent')
            return structured, {'ok': isinstance(structured, dict), 'latency_ms': elapsed, 'shape': json_shape(structured)}
        except Exception as exc:
            return None, {'ok': False, 'error_type': type(exc).__name__}

    def close(self) -> None:
        killed = False
        exited = False
        try:
            self.notify('notifications/exit')
        except Exception:
            pass
        try:
            if self.proc.stdin is not None and not self.proc.stdin.closed:
                self.proc.stdin.close()
        except Exception:
            pass
        try:
            waited = wait_exited_without_reap(self.proc.pid, 1)
            exited = waited is True
        except Exception:
            exited = False
        proc_io_after = read_proc_io(self.proc.pid)
        try:
            self.proc.wait(timeout=1)
        except Exception:
            try:
                killed = True
                self.proc.terminate()
            except Exception:
                pass
            try:
                self.proc.wait(timeout=1)
                exited = True
            except Exception:
                pass
        if self.proc.returncode is None:
            try:
                killed = True
                self.proc.kill()
            except Exception:
                pass
            try:
                wait_exited_without_reap(self.proc.pid, 5)
                proc_io_after = proc_io_after or read_proc_io(self.proc.pid)
                self.proc.wait(timeout=5)
                exited = True
            except Exception:
                pass
        record_proc_io_delta('mcp_stdio_child_processes', self.proc_io_before, proc_io_after)
        lifecycle = SUMMARY.setdefault('mcp_stdio_lifecycle', {'process_count': 0, 'exited_count': 0, 'killed_count': 0})
        lifecycle['process_count'] += 1
        if exited or self.proc.returncode is not None:
            lifecycle['exited_count'] += 1
        if killed:
            lifecycle['killed_count'] += 1
        try:
            self.stderr_file.close()
        except Exception:
            pass
        if self.proc.returncode is not None:
            OWNED_MCP_CHILDREN.pop(self.proc.pid, None)


def wait_owned_mcp_children_reaped(timeout: float = 5.0) -> dict[str, Any]:
    initial_pids = sorted(OWNED_MCP_CHILDREN)
    deadline = time.monotonic() + timeout
    while OWNED_MCP_CHILDREN and time.monotonic() < deadline:
        for pid, proc in list(OWNED_MCP_CHILDREN.items()):
            try:
                proc.wait(timeout=0)
            except subprocess.TimeoutExpired:
                continue
            except Exception:
                OWNED_MCP_CHILDREN.pop(pid, None)
                continue
            if proc.returncode is not None:
                OWNED_MCP_CHILDREN.pop(pid, None)
        if OWNED_MCP_CHILDREN:
            time.sleep(0.05)
    remaining_pids = sorted(OWNED_MCP_CHILDREN)
    return {
        'initial_count': len(initial_pids),
        'remaining_count': len(remaining_pids),
        'reaped_count': len(initial_pids) - len(remaining_pids),
    }


def mcp_start_initialized() -> McpClient:
    client = McpClient()
    init = client.call('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {}, 'clientInfo': {'name': 'mempal-skill-smoke', 'version': '0'}}, timeout=15)
    note('mcp_initialize', 'result' in init, result_fields=sorted(init.get('result', {}).keys()) if isinstance(init.get('result'), dict) else [])
    client.notify('notifications/initialized')
    return client


def mcp_call_isolated(tool_names: list[str], name: str, args: dict[str, Any], timeout: int) -> tuple[Any | None, dict[str, Any]]:
    label = 'mcp_read_' + name.removeprefix('mempal_')
    return mcp_call_isolated_labeled(tool_names, label, name, args, timeout)


def mcp_call_isolated_labeled(
    tool_names: list[str],
    label: str,
    name: str,
    args: dict[str, Any],
    timeout: int,
) -> tuple[Any | None, dict[str, Any]]:
    if name not in tool_names:
        note(label, True, skipped='tool_not_advertised')
        return None, {'ok': True, 'skipped': True}
    client: McpClient | None = None
    try:
        client = mcp_start_initialized()
        structured, info = client.tool(name, args, timeout=timeout)
        note(label, bool(info.get('ok')), **without_ok(info))
        return structured, info
    except Exception as exc:
        note(label, False, error_type=type(exc).__name__)
        return None, {'ok': False, 'error_type': type(exc).__name__}
    finally:
        if client is not None:
            client.close()


def mcp_crud() -> list[str]:
    cleanup_ids: list[str] = []
    # Discover tools in a throwaway server, then close it so status/read-only
    # probes cannot leave the CRUD server holding a stale read lock.
    discover: McpClient | None = None
    try:
        discover = mcp_start_initialized()
        tools_msg = discover.call('tools/list', {}, timeout=30)
        tools = tools_msg.get('result', {}).get('tools', [])
        tool_names = sorted(t.get('name') for t in tools if isinstance(t, dict) and isinstance(t.get('name'), str))
        required = {'mempal_ingest', 'mempal_operation_status', 'mempal_search', 'mempal_read_drawer', 'mempal_delete'}
        note('mcp_tools_list', required.issubset(set(tool_names)), tool_count=len(tool_names), required_missing=sorted(required - set(tool_names)))
    except Exception as exc:
        note('mcp_crud', False, error_type=type(exc).__name__, stage='tools_list')
        return cleanup_ids
    finally:
        if discover is not None:
            discover.close()

    for name, args, timeout in [
        ('mempal_pinned_facts', {'budget_chars': 512}, 30),
        ('mempal_timeline', {'top_k': 3, 'since': '1h'}, 30),
        ('mempal_doctor', {}, 30),
        ('mempal_field_taxonomy', {}, 30),
        ('mempal_taxonomy', {'action': 'list'}, 30),
        ('mempal_kg', {'action': 'stats'}, 30),
        ('mempal_skill', {'action': 'list'}, 30),
    ]:
        mcp_call_isolated(tool_names, name, args, timeout)

    client: McpClient | None = None
    try:
        client = mcp_start_initialized()
        create_args = {'content': f'{MARKER} reversible MCP smoke drawer; nonce {NONCE}; lexical tokens azurequill basaltfern cobaltlyric; safe to delete', 'wing': 'smoke', 'room': 'mcp', 'source_type': 'agent_inference', 'memory_kind': 'evidence', 'domain': 'project', 'field': 'smoke', 'smoke': True, 'wait': True, 'wait_timeout_secs': 90}
        create, info = client.tool('mempal_ingest', create_args, timeout=130)
        ids = created_ids_from(create)
        create_recovery: dict[str, Any] = {
            'operation_id_present': bool(operation_id_from(create)),
            'operation_state': operation_state_from(create),
        }
        if not ids and operation_id_from(create):
            # A non-terminal MCP ingest receipt means the daemon may still be
            # processing the write. Close this stdio server before following the
            # operation via CLI so the smoke runner never observes a result while
            # its own MCP process is still an extra SQLite holder.
            client.close()
            client = None
            waited = wait_operation(operation_id_from(create) or '', 'mcp_create_cli_wait')
            ids = created_ids_from(waited)
            create_recovery.update({'recovered_via': 'mcp_create_cli_wait', 'recovered_state': operation_state_from(waited)})
            SUMMARY['mcp_ingest_fallback_to_cli'] += 1
        note('mcp_create', bool(info.get('ok')) and bool(ids), created_id_count=len(ids), **recovery_fields(create_recovery), **without_ok(info))
        if not ids:
            note(
                'mcp_inconclusive_no_cleanup_id',
                False,
                reason='create_missing_created_drawer_ids',
                **recovery_fields(create_recovery),
                product_issue='https://github.com/RyderFreeman4Logos/mempal/issues/545',
            )
            return cleanup_ids
        cleanup_ids.extend(ids)
        created_id = ids[0]
        SUMMARY['created_counts']['mcp'] = len(cleanup_ids)

        if client is not None:
            client.close()
            client = None

        for name, args, timeout in [
            ('mempal_search', {'query': MARKER, 'top_k': 5, 'all_projects': True}, 180),
            ('mempal_read_drawer', {'drawer_id': created_id, 'all_projects': True}, 60),
            ('mempal_read_drawers', {'drawer_ids': [created_id], 'max_count': 2, 'all_projects': True}, 60),
            ('mempal_context', {'query': MARKER, 'all_projects': True, 'max_items': 3, 'include_distill_suggestions': False}, 150),
            ('mempal_brief', {'query': MARKER, 'domain': 'project', 'field': 'smoke', 'cwd': str(REPO), 'max_items': 3}, 150),
        ]:
            structured, info = mcp_call_isolated_labeled(
                tool_names,
                'mcp_' + name.removeprefix('mempal_'),
                name,
                args,
                timeout,
            )
            if name == 'mempal_search' and bool(info.get('ok')):
                matches = count_marker_matches(structured, 'mcp')
                note('mcp_search_created_match', matches > 0, active_matches=matches)

        client = mcp_start_initialized()

        update_args = {'content': f'{MARKER} reversible MCP smoke drawer updated; nonce {NONCE[::-1]}; lexical tokens deltaorchid embervault frostcairn; safe to delete', 'wing': 'smoke', 'room': 'mcp', 'source_type': 'agent_inference', 'memory_kind': 'evidence', 'domain': 'project', 'field': 'smoke', 'smoke': True, 'supersedes': created_id, 'wait': True, 'wait_timeout_secs': 90}
        update, uinfo = client.tool('mempal_ingest', update_args, timeout=130)
        upd_ids = created_ids_from(update)
        update_recovery: dict[str, Any] = {
            'operation_id_present': bool(operation_id_from(update)),
            'operation_state': operation_state_from(update),
        }
        if not upd_ids and operation_id_from(update):
            client.close()
            client = None
            waited = wait_operation(operation_id_from(update) or '', 'mcp_update_cli_wait')
            upd_ids = created_ids_from(waited)
            update_recovery.update({'recovered_via': 'mcp_update_cli_wait', 'recovered_state': operation_state_from(waited)})
            SUMMARY['mcp_ingest_fallback_to_cli'] += 1
        note('mcp_update', bool(uinfo.get('ok')) and bool(upd_ids), created_id_count=len(upd_ids), **recovery_fields(update_recovery), **without_ok(uinfo))
        if not upd_ids:
            delete_exact_ids_cli(cleanup_ids, 'mcp_cleanup_after_update_failure', room='mcp')
            note(
                'mcp_inconclusive_no_cleanup_id',
                False,
                reason='update_missing_created_drawer_ids',
                **recovery_fields(update_recovery),
                product_issue='https://github.com/RyderFreeman4Logos/mempal/issues/545',
            )
            return cleanup_ids
        cleanup_ids.extend(upd_ids)
        SUMMARY['created_counts']['mcp'] = len(cleanup_ids)
        if client is None:
            client = mcp_start_initialized()
        structured, info = client.tool('mempal_read_drawer', {'drawer_id': upd_ids[0], 'all_projects': True}, timeout=60)
        note('mcp_read_updated', bool(info.get('ok')), **without_ok(info))

        deleted = 0
        delete_false_count = 0
        for drawer_id in list(dict.fromkeys(cleanup_ids)):
            structured, dinfo = client.tool('mempal_delete', {'drawer_id': drawer_id}, timeout=60)
            ok = bool(dinfo.get('ok')) and isinstance(structured, dict) and structured.get('deleted') is True
            if ok:
                deleted += 1
            else:
                delete_false_count += 1
        SUMMARY['cleanup']['mcp_deleted_count'] = deleted
        post, pinfo = client.tool('mempal_search', {'query': MARKER, 'top_k': 5, 'all_projects': True}, timeout=180)
        post_matches = count_marker_matches(post, 'mcp')
        if post_matches > 0:
            SUMMARY['cleanup']['failures'] += delete_false_count
        note('mcp_delete_batch', post_matches == 0, attempted_count=len(set(cleanup_ids)), deleted_count=deleted, delete_false_count=delete_false_count)
        note('mcp_crud', post_matches == 0 and deleted > 0, created_id_count=len(cleanup_ids), deleted_count=deleted, delete_false_count=delete_false_count, post_delete_active_matches=post_matches)
        if 'mempal_status' in tool_names:
            structured, sinfo = client.tool('mempal_status', {}, timeout=30)
            note('mcp_status_last', bool(sinfo.get('ok')), **without_ok(sinfo))
        return cleanup_ids
    except Exception as exc:
        note('mcp_crud', False, error_type=type(exc).__name__)
        return cleanup_ids
    finally:
        if client is not None:
            client.close()
        # If MCP CRUD failed after exposing cleanup-safe IDs, clean them by exact ID.
        if SUMMARY['groups'].get('mcp_crud', {}).get('ok') is not True and cleanup_ids:
            cleaned = 0
            for drawer_id in list(dict.fromkeys(cleanup_ids)):
                proc = run_child_process(
                    ['mempal', 'delete', drawer_id],
                    timeout=60,
                    io_category='cli_child_processes',
                )
                if proc['returncode'] == 0:
                    cleaned += 1
            note('mcp_fallback_cli_cleanup', True, exact_id_count=len(set(cleanup_ids)), deleted_count=cleaned)


def holder_summary() -> dict[str, Any]:
    try:
        proc = run_child_process(
            ['mempal', 'daemon', 'status'],
            timeout=30,
            io_category='cli_child_processes',
        )
        text = (proc['stdout'] + proc['stderr']).decode('utf-8', errors='replace')
        parsed: dict[str, Any] = {
            'exit_ok': proc['returncode'] == 0,
            'bytes': len(proc['stdout']) + len(proc['stderr']),
            'status': None,
            'total_holders': None,
            'extra_holders': None,
            'stale_mcp_servers': None,
            'orphan_daemons': None,
        }
        for line in text.splitlines():
            if line.startswith('status:'):
                parsed['status'] = line.split(':', 1)[1].strip()
            for key in ('total', 'extra_holders', 'stale_mcp_servers', 'orphan_daemons'):
                if line.startswith(key + ':'):
                    try:
                        value = int(line.split(':', 1)[1].strip().split()[0])
                    except Exception:
                        value = None
                    parsed['total_holders' if key == 'total' else key] = value
        return {
            **parsed,
            'unmanaged_mcp_holder_state': 'clear'
            if (parsed.get('stale_mcp_servers') in (0, None) and parsed.get('extra_holders') in (0, None))
            else 'present',
        }
    except Exception as exc:
        return {'exit_ok': False, 'error_type': type(exc).__name__}


def classify_daemon(pid_before: int | None, pid_after: int | None) -> dict[str, Any]:
    if pid_before and pid_after and pid_before == pid_after:
        state = 'stable'
    elif pid_before and pid_after:
        state = 'restarted'
    elif pid_before and not pid_after:
        state = 'exited_or_stopped'
    elif not pid_before and pid_after:
        state = 'started'
    else:
        state = 'not_running'
    return {
        'pid_before': pid_before,
        'pid_after': pid_after,
        'state': state,
        'pid_stable': bool(pid_before and pid_after and pid_before == pid_after),
    }


def check_binary_consistency(daemon_pid: int | None) -> dict[str, Any]:
    """Verify the running daemon's executable matches the installed CLI binary.

    A common regression: ``cargo install`` or ``cp`` replaces the on-disk
    binary but the daemon service is not restarted. The daemon keeps running
    the old binary, so smoke passes against stale code while the installed CLI
    already has the new version. This check surfaces that mismatch.

    Comparison strategy (in order of preference):
    1. If both paths resolve to the same inode (hardlink/identical), pass.
    2. If file sizes differ, fail (different binaries).
    3. If sizes match, compare SHA-256 hashes — pass if identical.

    ``ok=False`` is only set when the binaries are genuinely different or
    the daemon exe carries `` (deleted)``.
    """
    import hashlib

    daemon_exe = daemon_exe_path(daemon_pid)
    installed = installed_binary_path()
    result: dict[str, Any] = {
        'ok': True,
        'daemon_pid': daemon_pid,
        'daemon_exe': daemon_exe,
        'installed_binary': installed,
    }
    if daemon_exe is None or installed is None:
        result['ok'] = True
        result['reason'] = 'exe_or_installed_path_unavailable'
        return result

    deleted = daemon_exe.endswith(' (deleted)')
    daemon_norm = daemon_exe.removesuffix(' (deleted')

    if deleted:
        result['ok'] = False
        result['reason'] = 'daemon_binary_deleted_on_disk_not_restarted'
        return result

    # Check inode equality first (hardlinks, bind mounts, etc.)
    try:
        daemon_stat = os.stat(daemon_norm)
        installed_stat = os.stat(installed)
        if daemon_stat.st_dev == installed_stat.st_dev and daemon_stat.st_ino == installed_stat.st_ino:
            result['method'] = 'inode_match'
            return result
    except Exception:
        pass

    # Different inodes — compare file size, then hash.
    try:
        daemon_size = os.path.getsize(daemon_norm)
        installed_size = os.path.getsize(installed)
    except Exception as exc:
        result['reason'] = f'size_comparison_failed: {type(exc).__name__}'
        return result

    if daemon_size != installed_size:
        result['ok'] = False
        result['reason'] = f'size_mismatch: daemon={daemon_size} installed={installed_size}'
        return result

    # Same size — compare hashes to distinguish identical copies from different builds.
    def _file_hash(path: str) -> str | None:
        try:
            h = hashlib.sha256()
            with open(path, 'rb') as f:
                for chunk in iter(lambda: f.read(1024 * 1024), b''):
                    h.update(chunk)
            return h.hexdigest()[:16]
        except Exception:
            return None

    daemon_hash = _file_hash(daemon_norm)
    installed_hash = _file_hash(installed)
    result['method'] = 'hash_compare'
    if daemon_hash is not None and installed_hash is not None and daemon_hash != installed_hash:
        result['ok'] = False
        result['reason'] = f'hash_mismatch: daemon={daemon_hash} installed={installed_hash}'
    return result


def append_io_history() -> None:
    record = {
        'schema': 'mempal_smoke_io_history_v1',
        'timestamp_unix': int(time.time()),
        'marker_hash': SUMMARY.get('marker_hash'),
        'overall_ok': SUMMARY.get('overall_ok'),
        'duration_ms': SUMMARY.get('duration_ms'),
        'failure_count': len(SUMMARY.get('failures', [])),
        'cleanup': SUMMARY.get('cleanup'),
        'created_counts': SUMMARY.get('created_counts'),
        'daemon': SUMMARY.get('daemon'),
        'holders_after': SUMMARY.get('holders_after'),
        'io': SUMMARY.get('io'),
    }
    path = REPO / 'target' / 'smoke-io-history.jsonl'
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open('a', encoding='utf-8') as handle:
            handle.write(json.dumps(record, sort_keys=True) + '\n')
        SUMMARY['io_history'] = {'appended': True, 'path': str(path.relative_to(REPO))}
    except Exception as exc:
        SUMMARY['io_history'] = {'appended': False, 'error_type': type(exc).__name__}


def main() -> int:
    import hashlib
    SUMMARY['marker_hash'] = hashlib.sha256(MARKER.encode()).hexdigest()[:16]
    daemon_pid_before = daemon_main_pid()
    daemon_io_before = read_proc_io(daemon_pid_before)
    child_io_before = child_io_blocks_snapshot()
    SUMMARY['io']['daemon_pid_before'] = daemon_pid_before
    start = time.monotonic()
    run_cli('version', ['mempal', '--version'], timeout=30)
    run_cli('daemon_status_pre', ['mempal', 'daemon', 'status'], timeout=30)
    run_cli('doctor_rest', ['mempal', 'doctor', 'rest', '--format', 'json'], expect_json=True, timeout=60)
    _doc_rc, _doc_out, _doc_err, doc_parsed, _doc_shape = run_cli('doctor_json', ['mempal', 'doctor', '--format', 'json'], expect_json=True, timeout=60)
    if isinstance(doc_parsed, dict):
        db_info = doc_parsed.get('db') or {}
        db_schema_version = db_info.get('schema_version') if isinstance(db_info, dict) else None
        supported_schema = doc_parsed.get('supported_schema_version')
        # Schema must match exactly — a compatible-but-different version means
        # a migration is pending or the binary was downgraded.
        schema_matches = db_schema_version is not None and db_schema_version == supported_schema
        warnings = doc_parsed.get('warnings') or []
        # Classify warnings: critical if they indicate data integrity issues.
        # "extra process" and "locked" are operational, not critical regressions.
        critical_keywords = ('corrupt', 'mismatch', 'missing', 'incompatible', 'migration failed')
        critical_warnings = [w for w in warnings if isinstance(w, str) and any(k in w.lower() for k in critical_keywords)]
        # Check embedding health: endpoint cooldowns or terminal queue failures.
        emb_info = doc_parsed.get('embedding') or {}
        emb_endpoints = emb_info.get('endpoints') if isinstance(emb_info, dict) else None
        emb_cooldowns = 0
        if isinstance(emb_endpoints, list):
            for ep in emb_endpoints:
                if isinstance(ep, dict) and ep.get('cooldown_remaining_secs') is not None:
                    emb_cooldowns += 1
        emb_queue = emb_info.get('queue') if isinstance(emb_info, dict) else None
        emb_terminal_failures = 0
        if isinstance(emb_queue, dict):
            emb_terminal_failures = int(emb_queue.get('failed_terminal') or 0)
        # Terminal failures are historical and don't indicate current regression
        # unless they're growing. We report them but only fail if endpoints are
        # in cooldown (active health problem).
        embedding_ok = emb_cooldowns == 0
        note(
            'doctor_json_validation',
            bool(schema_matches) and len(critical_warnings) == 0 and embedding_ok,
            db_schema_version=db_schema_version,
            supported_schema_version=supported_schema,
            schema_matches=bool(schema_matches),
            total_warning_count=len(warnings),
            critical_warning_count=len(critical_warnings),
            embedding_endpoint_cooldowns=emb_cooldowns,
            embedding_terminal_failures=emb_terminal_failures,
            embedding_ok=embedding_ok,
        )
        if not (schema_matches and len(critical_warnings) == 0 and embedding_ok):
            SUMMARY['failures'] = [f for f in SUMMARY['failures'] if f != 'doctor_json_validation']
            SUMMARY['failures'].append('doctor_json_validation')
    run_cli('status', ['mempal', 'status'], timeout=60)
    run_cli('timeline_json', ['mempal', 'timeline', '--since', '1h', '--format', 'json'], expect_json=True, timeout=60)
    run_cli('pinned_json', ['mempal', 'pinned', '--json'], expect_json=True, timeout=60)
    run_cli('field_taxonomy_json', ['mempal', 'field-taxonomy', '--format', 'json'], expect_json=True, timeout=60)
    run_cli('patterns_json', ['mempal', 'patterns', 'list', '--json'], expect_json=True, timeout=60)
    run_cli('skills_json', ['mempal', 'skills', 'list', '--json'], expect_json=True, timeout=60)
    run_cli('repair_json', ['mempal', 'repair', 'list', '--json'], expect_json=True, timeout=60)

    cli_ids = cli_crud()
    mcp_ids = mcp_crud()

    SUMMARY['mcp_owned_children_after_wait'] = wait_owned_mcp_children_reaped()
    SUMMARY['holders_after'] = holder_summary()
    daemon_pid_after = daemon_main_pid()
    daemon_io_after = read_proc_io(daemon_pid_after)
    child_io_after = child_io_blocks_snapshot()
    SUMMARY['daemon'] = classify_daemon(daemon_pid_before, daemon_pid_after)
    SUMMARY['binary_consistency'] = check_binary_consistency(daemon_pid_after)
    SUMMARY['io']['daemon_pid_after'] = daemon_pid_after
    SUMMARY['io']['daemon_proc_io_delta'] = io_delta(daemon_io_before, daemon_io_after) if daemon_pid_before == daemon_pid_after else None
    SUMMARY['io']['child_block_io_delta'] = child_io_blocks_delta(child_io_before, child_io_after)
    SUMMARY['duration_ms'] = int((time.monotonic() - start) * 1000)
    binary_ok = SUMMARY.get('binary_consistency', {}).get('ok', True)
    SUMMARY['overall_ok'] = not SUMMARY['failures'] and SUMMARY['cleanup']['failures'] == 0 and binary_ok
    append_io_history()
    print(json.dumps(SUMMARY, sort_keys=True))
    return 0 if SUMMARY['overall_ok'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
