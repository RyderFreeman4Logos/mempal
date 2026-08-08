#!/usr/bin/env python3
"""Aggregate-only mempal smoke runner for repo-local skills/smoke-test.

Exercises CLI and MCP CRUD without printing drawer content or raw command output.
"""
from __future__ import annotations

import json
import importlib.util
import ipaddress
import math
import os
import signal
import subprocess
import sys
import tempfile
import time
import resource
import urllib.parse
import urllib.request
from urllib.error import HTTPError
from pathlib import Path
from typing import Any


def _load_smoke_runtime() -> Any:
    """Load the sibling runtime module without mutating global import paths."""
    path = Path(__file__).with_name('smoke_runtime.py')
    spec = importlib.util.spec_from_file_location('_mempal_smoke_runtime', path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f'cannot load smoke runtime from {path}')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


_SMOKE_RUNTIME = _load_smoke_runtime()
CleanupManifest = _SMOKE_RUNTIME.CleanupManifest

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - Python 3.11+ has tomllib.
    try:
        import tomli as tomllib  # type: ignore[no-redef]
    except ModuleNotFoundError:  # pragma: no cover - fallback parser handles smoke fields.
        tomllib = None

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
OWNED_MCP_REGISTRY = _SMOKE_RUNTIME.OwnedSubprocessRegistry()
OWNED_MCP_CHILDREN: dict[int, subprocess.Popen[Any]] = OWNED_MCP_REGISTRY.processes
CLEANUP_MANIFEST: CleanupManifest | None = CleanupManifest()
_PROC_IO_RECEIPT_TARGETS: dict[tuple[str, str], dict[str, Any]] = {}

PROC_IO_KEYS = ('read_bytes', 'write_bytes', 'cancelled_write_bytes', 'rchar', 'wchar')
CONFORMANCE_MATRIX_PATH = 'docs/conformance-matrix.md'
DEFAULT_RERANKER_TIMEOUT_SECS = 60
DEFAULT_RERANKER_TOP_K = 50
MAX_RERANKER_SMOKE_TIMEOUT_SECS = 240
MCP_RESOURCE_NOT_FOUND_ERROR_CODE = -32002
RERANKER_PROBES = (
    'reranker_endpoint_reachable',
    'reranker_reorders_results',
    'reranker_fallback_warning',
)
CONFORMANCE_GROUPS: dict[str, dict[str, Any]] = {
    'runtime_daemon_service': {
        'features': [
            'runtime.installed_binary',
            'runtime.daemon_singleton',
            'runtime.db_holders',
            'runtime.schema_compatibility',
        ],
        'probes': [
            'version',
            'daemon_status_pre',
            'status',
            'queue_failed',
            'doctor_json',
            'doctor_json_validation',
            'binary_consistency',
        ],
        'skipped_reason': 'installed binary, daemon, or doctor command unavailable',
    },
    'core_cli_memory_lifecycle': {
        'features': [
            'cli.ingest',
            'cli.search',
            'cli.context',
            'cli.view_read',
            'cli.pinned_facts',
            'cli.pin_unpin',
            'cli.delete_soft_delete',
        ],
        'probes': [
            'cli_create',
            'cli_read_view',
            'cli_search_created',
            'cli_search_created_match',
            'cli_context_created',
            'cli_pinned_before',
            'cli_pin',
            'cli_unpin',
            'cli_pinned_after',
            'cli_update',
            'cli_read_updated',
            'cli_delete_batch',
            'cli_search_post_delete',
            'cli_crud',
        ],
        'skipped_reason': 'write smoke could not create cleanup-authorized drawer ids',
    },
    'typed_project_metadata': {
        'features': [
            'metadata.wing',
            'metadata.room',
            'metadata.project',
            'metadata.source_type',
            'metadata.memory_kind',
            'metadata.domain',
            'metadata.field',
        ],
        'probes': [
            'field_taxonomy_json',
            'mcp_read_field_taxonomy',
            'cli_create',
            'cli_search_created_match',
            'mcp_create',
            'mcp_search_created_match',
        ],
        'skipped_reason': 'metadata smoke skipped because live create/search path was unavailable',
    },
    'cli_dashboard_maintenance': {
        'features': [
            'cli.timeline',
            'cli.tail',
            'cli.stats',
            'cli.repair',
            'cli.purge',
        ],
        'probes': [
            'timeline_json',
            'tail_shape',
            'stats_shape',
            'repair_json',
        ],
        'skipped_reason': 'dashboard or maintenance commands unavailable in installed binary',
    },
    'rest_availability': {
        'features': [
            'rest.doctor',
            'rest.status',
            'rest.search',
            'rest.ingest',
            'rest.taxonomy',
            'rest.timeline',
            'rest.pinned_facts',
        ],
        'probes': [
            'doctor_rest',
        ],
        'skipped_reason': 'REST feature disabled or daemon REST endpoint unreachable',
    },
    'mcp_tools': {
        'features': [
            'mcp.status',
            'mcp.search',
            'mcp.context',
            'mcp.read',
            'mcp.pinned',
            'mcp.timeline',
            'mcp.doctor',
            'mcp.ingest',
            'mcp.delete',
            'mcp.operation_status',
        ],
        'probes': [
            'mcp_tools_list',
            'mcp_read_pinned_facts',
            'mcp_read_timeline',
            'mcp_read_doctor',
            'mcp_read_field_taxonomy',
            'mcp_read_taxonomy',
            'mcp_read_kg',
            'mcp_read_skill',
            'mcp_create',
            'mcp_operation_status',
            'mcp_search',
            'mcp_search_created_match',
            'mcp_read_drawer',
            'mcp_read_drawers',
            'mcp_context',
            'mcp_brief',
            'mcp_update',
            'mcp_read_updated',
            'mcp_delete_batch',
            'mcp_crud',
            'mcp_status_last',
        ],
        'skipped_reason': 'MCP stdio server unavailable or required tools not advertised',
    },
    'embedding_search_behavior': {
        'features': [
            'search.hybrid_vector_bm25',
            'search.bm25_fallback',
            'search.degraded_warnings',
            'search.bounded_query',
            'context.brief_bounded',
        ],
        'probes': [
            'doctor_json_validation',
            'cli_search_created_match',
            'cli_context_created',
            'mcp_search_created_match',
            'mcp_brief',
        ],
        'skipped_reason': 'embedding/search probes skipped because live create/search path was unavailable',
    },
    'search_reranker_behavior': {
        'features': [
            'search.reranker_endpoint',
            'search.reranker_reordering',
            'search.reranker_fallback_warning',
        ],
        'probes': list(RERANKER_PROBES),
        'skipped_reason': 'search reranker probes skipped because search.reranker.enabled=false',
    },
    'privacy_cleanup_safety': {
        'features': [
            'safety.aggregate_diagnostics',
            'safety.cleanup_exact_created_ids',
            'safety.no_raw_diagnostics',
            'safety.queue_payload_redaction',
        ],
        'probes': [
            'doctor_json',
            'queue_failed',
            'cli_delete_batch',
            'mcp_delete_batch',
            'cli_crud',
            'mcp_crud',
        ],
        'skipped_reason': 'cleanup safety probes skipped because no cleanup-authorized ids were created',
    },
}


def note(name: str, ok: bool, **fields: Any) -> None:
    safe = {'ok': ok}
    safe.update(fields)
    SUMMARY['groups'][name] = safe
    SUMMARY['failures'] = [failure for failure in SUMMARY['failures'] if failure != name]
    if not ok:
        SUMMARY['failures'].append(name)


def clear_probe_failures(*labels: str) -> None:
    """Drop best-effort probe/fallback labels from the failure ledger.

    REST probes (``mcp_create_rest``, ``mcp_update_rest`` and their
    ``*_fallback`` siblings) are optional: a later path may still produce IDs.
    When the final ``mcp_create`` / ``mcp_update`` note succeeds, any earlier
    probe failure must be cleared so it does not leave the smoke gate red.
    """
    SUMMARY['failures'] = [f for f in SUMMARY['failures'] if f not in labels]


def without_ok(info: dict[str, Any]) -> dict[str, Any]:
    return {key: value for key, value in info.items() if key != 'ok'}


def _probe_result(label: str) -> tuple[str, dict[str, Any]]:
    info = SUMMARY['groups'].get(label)
    if not isinstance(info, dict):
        return 'missing', {}
    if info.get('ok') is True:
        if info.get('skipped') or info.get('skipped_reason'):
            return 'skipped', info
        return 'pass', info
    return 'fail', info


def build_conformance_report(
    group_specs: dict[str, dict[str, Any]] | None = None,
) -> dict[str, Any]:
    """Build aggregate feature-group conformance from existing smoke probes.

    The report intentionally references only feature ids, probe labels, counts,
    and error classes already present in aggregate probe notes. It never copies
    command stdout, drawer content, search previews, prompts, or headers.
    """
    specs = group_specs or CONFORMANCE_GROUPS
    groups: dict[str, Any] = {}
    summary_counts = {'pass': 0, 'fail': 0, 'skipped': 0}
    for group_name, spec in specs.items():
        probes = [p for p in spec.get('probes', []) if isinstance(p, str)]
        features = [f for f in spec.get('features', []) if isinstance(f, str)]
        passed: list[str] = []
        failed: list[str] = []
        skipped: list[str] = []
        missing: list[str] = []
        failure_classes: dict[str, str] = {}

        for probe in probes:
            status, info = _probe_result(probe)
            if status == 'pass':
                passed.append(probe)
            elif status == 'fail':
                failed.append(probe)
                failure_class = info.get('stderr_class') or info.get('error_type') or info.get('reason')
                if isinstance(failure_class, str):
                    failure_classes[probe] = failure_class
            elif status == 'skipped':
                skipped.append(probe)
            else:
                missing.append(probe)

        if failed or missing:
            status = 'fail'
        elif passed:
            status = 'pass'
        else:
            status = 'skipped'
        summary_counts[status] += 1

        groups[group_name] = {
            'status': status,
            'feature_count': len(features),
            'probe_count': len(probes),
            'passed_probe_count': len(passed),
            'skipped_probe_count': len(skipped),
            'missing_probe_count': len(missing),
            'feature_ids': features,
            'failed_probes': failed or None,
            'failure_classes': failure_classes or None,
            'missing_probes': missing or None,
            'skipped_reason': spec.get('skipped_reason') if status == 'skipped' else None,
        }

    return {
        'schema': 'mempal_conformance_smoke_v1',
        'matrix': CONFORMANCE_MATRIX_PATH,
        'groups': groups,
        'summary': summary_counts,
    }


def conformance_failure_count(report: Any) -> int:
    if not isinstance(report, dict):
        return 0
    summary = report.get('summary')
    if not isinstance(summary, dict):
        return 0
    failure_count = summary.get('fail', 0)
    return failure_count if isinstance(failure_count, int) else 0


def smoke_overall_ok(summary: dict[str, Any]) -> bool:
    cleanup = summary.get('cleanup')
    cleanup_failures = cleanup.get('failures', 0) if isinstance(cleanup, dict) else 0
    binary_consistency = summary.get('binary_consistency')
    binary_ok = binary_consistency.get('ok', True) if isinstance(binary_consistency, dict) else True
    return (
        not summary.get('failures')
        and cleanup_failures == 0
        and binary_ok
        and conformance_failure_count(summary.get('conformance')) == 0
    )


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


def record_proc_io_delta(
    category: str,
    before: dict[str, int] | None,
    after: dict[str, int] | None,
    receipt_id: str | None = None,
) -> None:
    _SMOKE_RUNTIME.record_proc_io_delta(
        SUMMARY['io'], _PROC_IO_RECEIPT_TARGETS, PROC_IO_KEYS,
        category, before, after, receipt_id,
    )


def wait_exited_without_reap(pid: int, timeout: float) -> bool | None:
    """Wait for a child process to exit without reaping it (leaves it for Popen.cleanup).

    On Python 3.14+, ``os.waitid`` with ``WNOWAIT`` can deadlock when the
    child uses tempfile redirection.  We instead poll ``/proc/<pid>`` which
    reliably detects process exit without consuming the wait status.
    """
    deadline = time.monotonic() + timeout
    while True:
        if not Path(f'/proc/{pid}').exists():
            return True
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.05)


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

        try:
            proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            killed = True
            proc.kill()
            proc.wait(timeout=5)

        after = read_proc_io(proc.pid)
        return_code = proc.returncode
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


def mempal_config_path() -> Path:
    return Path(os.environ.get('HOME', '~')).expanduser() / '.mempal' / 'config.toml'


def _strip_toml_comment(line: str) -> str:
    in_single = False
    in_double = False
    escaped = False
    for index, char in enumerate(line):
        if escaped:
            escaped = False
            continue
        if in_double and char == '\\':
            escaped = True
            continue
        if char == "'" and not in_double:
            in_single = not in_single
            continue
        if char == '"' and not in_single:
            in_double = not in_double
            continue
        if char == '#' and not in_single and not in_double:
            return line[:index]
    return line


def _parse_smoke_toml_scalar(raw: str) -> Any:
    value = raw.strip()
    if value == 'true':
        return True
    if value == 'false':
        return False
    if value.startswith('"') and value.endswith('"'):
        try:
            return json.loads(value)
        except json.JSONDecodeError:
            return value[1:-1]
    if value.startswith("'") and value.endswith("'"):
        return value[1:-1]
    try:
        return int(value)
    except ValueError:
        return None


def _load_smoke_toml_fields_fallback(path: Path) -> dict[str, Any]:
    root: dict[str, Any] = {}
    current: dict[str, Any] | None = None
    for raw_line in path.read_text(encoding='utf-8', errors='replace').splitlines():
        line = _strip_toml_comment(raw_line).strip()
        if not line:
            continue
        if line.startswith('[') and line.endswith(']'):
            section = line[1:-1].strip()
            if section in {'search.reranker', 'privacy.remote_calls'}:
                current = root
                for part in section.split('.'):
                    nested = current.get(part)
                    if not isinstance(nested, dict):
                        nested = {}
                        current[part] = nested
                    current = nested
            else:
                current = None
            continue
        if current is None or '=' not in line:
            continue
        key, raw_value = line.split('=', 1)
        value = _parse_smoke_toml_scalar(raw_value)
        if value is not None:
            current[key.strip()] = value
    return root


def _load_smoke_toml_fields(path: Path) -> dict[str, Any]:
    if tomllib is not None:
        try:
            with path.open('rb') as handle:
                return tomllib.load(handle)
        except Exception:
            return _load_smoke_toml_fields_fallback(path)
    return _load_smoke_toml_fields_fallback(path)


def _config_bool(values: dict[str, Any], key: str, default: bool = False) -> bool:
    value = values.get(key, default)
    return value if isinstance(value, bool) else default


def _config_positive_int(values: dict[str, Any], key: str, default: int) -> int:
    value = values.get(key, default)
    return value if type(value) is int and value > 0 else default


def load_reranker_config(config_path: Path | None = None) -> dict[str, Any]:
    """Load only [search.reranker] fields needed by smoke.

    Missing config follows mempal's default: reranker disabled. The returned
    dict intentionally contains no raw endpoint diagnostics beyond fields
    needed to make the probe request.
    """
    path = config_path or mempal_config_path()
    if not path.exists():
        return {
            'enabled': False,
            'endpoint': None,
            'model': None,
            'timeout_secs': DEFAULT_RERANKER_TIMEOUT_SECS,
            'top_k': DEFAULT_RERANKER_TOP_K,
            'source': 'default_missing_config',
        }
    root = _load_smoke_toml_fields(path)
    search = root.get('search') if isinstance(root, dict) else None
    reranker = search.get('reranker') if isinstance(search, dict) else None
    if not isinstance(reranker, dict):
        reranker = {}
    privacy = root.get('privacy') if isinstance(root, dict) else None
    remote_calls = privacy.get('remote_calls') if isinstance(privacy, dict) else None
    if not isinstance(remote_calls, dict):
        remote_calls = {}

    return {
        'enabled': _config_bool(reranker, 'enabled'),
        'endpoint': reranker.get('endpoint') if isinstance(reranker.get('endpoint'), str) else None,
        'model': reranker.get('model') if isinstance(reranker.get('model'), str) else None,
        'timeout_secs': _config_positive_int(reranker, 'timeout_secs', DEFAULT_RERANKER_TIMEOUT_SECS),
        'top_k': _config_positive_int(reranker, 'top_k', DEFAULT_RERANKER_TOP_K),
        'remote_calls': {
            'fail_closed': _config_bool(remote_calls, 'fail_closed'),
            'allow_rerank': _config_bool(remote_calls, 'allow_rerank'),
        },
        'source': 'config',
    }


def normalize_reranker_endpoint(endpoint: str) -> str:
    endpoint = endpoint.strip()
    if not endpoint:
        raise ValueError('empty_endpoint')
    raw = endpoint if endpoint.startswith(('http://', 'https://')) else f'http://{endpoint}'
    parsed = urllib.parse.urlparse(raw)
    if not parsed.hostname:
        raise ValueError('missing_host')
    if parsed.username or parsed.password:
        raise ValueError('userinfo_not_allowed')
    if parsed.query:
        raise ValueError('query_not_allowed')
    try:
        parsed.port
    except ValueError as exc:
        raise ValueError('invalid_port') from exc
    path = parsed.path
    if not path or path == '/':
        path = '/v1/rerank'
    return urllib.parse.urlunparse((parsed.scheme, parsed.netloc, path, '', '', '')).rstrip('/')


def sanitized_reranker_endpoint_fields(endpoint: str) -> dict[str, Any]:
    parsed = urllib.parse.urlparse(endpoint)
    host = parsed.hostname or ''
    host_kind = 'hostname'
    if host in {'localhost', '127.0.0.1', '::1'}:
        host_kind = 'loopback'
    else:
        try:
            ip = ipaddress.ip_address(host)
            if ip.is_private:
                host_kind = 'private_lan'
        except ValueError:
            pass
    endpoint_port_valid = True
    try:
        endpoint_has_port = parsed.port is not None
    except ValueError:
        endpoint_has_port = True
        endpoint_port_valid = False
    return {
        'endpoint_scheme': parsed.scheme or None,
        'endpoint_host_kind': host_kind,
        'endpoint_has_port': endpoint_has_port,
        'endpoint_port_valid': endpoint_port_valid,
        'endpoint_path_kind': 'default_rerank' if parsed.path == '/v1/rerank' else 'custom',
    }


def reranker_endpoint_is_local_or_private(endpoint: str) -> bool:
    parsed = urllib.parse.urlparse(endpoint)
    host = parsed.hostname or ''
    if host.lower() == 'localhost' or '.' not in host or host.lower().endswith('.local'):
        return True
    try:
        ip = ipaddress.ip_address(host)
        return ip.is_loopback or ip.is_private
    except ValueError:
        return False


def reranker_remote_call_blocked(config: dict[str, Any], endpoint: str) -> bool:
    policy = config.get('remote_calls')
    if not isinstance(policy, dict):
        return False
    return (
        not reranker_endpoint_is_local_or_private(endpoint)
        and policy.get('fail_closed') is True
        and policy.get('allow_rerank') is not True
    )


def reranker_smoke_documents() -> tuple[str, list[str]]:
    return (
        'Rust ownership borrow checker memory safety',
        [
            'A sourdough recipe with ripe bananas, cinnamon, and a warm oven.',
            'Rust ownership, borrowing, lifetimes, and the borrow checker enforce memory safety.',
        ],
    )


def reranker_response_items(value: Any) -> list[dict[str, Any]]:
    if not isinstance(value, dict):
        return []
    raw_items = value.get('results')
    if not isinstance(raw_items, list) or not raw_items:
        raw_items = value.get('data')
    if not isinstance(raw_items, list):
        return []
    return [item for item in raw_items if isinstance(item, dict)]


def reranker_score(item: dict[str, Any]) -> float | None:
    raw = item.get('relevance_score', item.get('score'))
    if not isinstance(raw, (int, float)):
        return None
    score = float(raw)
    return score if math.isfinite(score) else None


def reranker_response_shape(value: Any, document_count: int) -> dict[str, Any]:
    items = reranker_response_items(value)
    valid_scores = 0
    has_relevance_score = False
    has_score = False
    for item in items:
        if 'relevance_score' in item:
            has_relevance_score = True
        if 'score' in item:
            has_score = True
        index = item.get('index')
        score = reranker_score(item)
        if isinstance(index, int) and 0 <= index < document_count and score is not None:
            valid_scores += 1
    return {
        'ok': valid_scores > 0,
        'result_count': len(items),
        'valid_score_count': valid_scores,
        'has_relevance_score': has_relevance_score or None,
        'has_score': has_score or None,
        'reason': None if valid_scores > 0 else 'missing_valid_scored_results',
    }


def reranker_reorder_evidence(value: Any) -> dict[str, Any]:
    scores: dict[int, float] = {}
    first_returned_index: int | None = None
    for item in reranker_response_items(value):
        index = item.get('index')
        score = reranker_score(item)
        if not isinstance(index, int) or score is None:
            continue
        if first_returned_index is None:
            first_returned_index = index
        scores[index] = score
    score_delta = scores.get(1, float('-inf')) - scores.get(0, float('inf'))
    later_document_score_higher = math.isfinite(score_delta) and score_delta > 0.01
    return {
        'ok': later_document_score_higher,
        'first_returned_index': first_returned_index,
        'later_document_score_higher': later_document_score_higher,
        'score_delta_millis': int(score_delta * 1000) if math.isfinite(score_delta) else None,
        'reason': None if later_document_score_higher else 'later_document_not_scored_higher',
    }


def note_reranker_skipped(reason: str, **fields: Any) -> None:
    for probe in RERANKER_PROBES:
        note(probe, True, skipped=reason, **fields)


def call_reranker_endpoint(config: dict[str, Any], endpoint: str) -> tuple[Any | None, dict[str, Any]]:
    query, documents = reranker_smoke_documents()
    payload = {
        'model': config['model'],
        'query': query,
        'documents': documents,
        'top_n': len(documents),
    }
    timeout = min(max(int(config.get('timeout_secs') or DEFAULT_RERANKER_TIMEOUT_SECS), 1), MAX_RERANKER_SMOKE_TIMEOUT_SECS)
    request = urllib.request.Request(
        endpoint,
        data=json.dumps(payload).encode(),
        headers={'Content-Type': 'application/json'},
        method='POST',
    )
    start = time.monotonic()
    endpoint_fields = sanitized_reranker_endpoint_fields(endpoint)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = response.read(64 * 1024 + 1)
            latency_ms = int((time.monotonic() - start) * 1000)
            if response.status != 200:
                return None, {
                    'ok': False,
                    'reason': 'unexpected_http_status',
                    'http_status': response.status,
                    'latency_ms': latency_ms,
                    **endpoint_fields,
                }
            if len(body) > 64 * 1024:
                return None, {
                    'ok': False,
                    'reason': 'response_body_too_large',
                    'latency_ms': latency_ms,
                    **endpoint_fields,
                }
            parsed = json.loads(body.decode('utf-8'))
            shape = reranker_response_shape(parsed, len(documents))
            return parsed, {
                'ok': bool(shape.get('ok')),
                'http_status': response.status,
                'latency_ms': latency_ms,
                **without_ok(shape),
                **endpoint_fields,
            }
    except HTTPError as exc:
        return None, {
            'ok': False,
            'reason': 'http_error',
            'http_status': exc.code,
            'latency_ms': int((time.monotonic() - start) * 1000),
            **endpoint_fields,
        }
    except Exception as exc:
        return None, {
            'ok': False,
            'error_type': type(exc).__name__,
            'latency_ms': int((time.monotonic() - start) * 1000),
            **endpoint_fields,
        }


def probe_search_reranker_behavior(config_path: Path | None = None) -> None:
    try:
        config = load_reranker_config(config_path)
    except Exception as exc:
        for probe in RERANKER_PROBES:
            note(probe, False, error_type=type(exc).__name__, stage='load_config')
        return

    if not config.get('enabled'):
        note_reranker_skipped('reranker_disabled')
        return

    # The fallback warning path is covered by Rust unit tests; smoke records the
    # conformance probe without forcing the live endpoint into failure mode.
    note('reranker_fallback_warning', True, skipped='covered_by_rust_unit')

    if not config.get('endpoint'):
        note('reranker_endpoint_reachable', False, reason='missing_endpoint')
        note('reranker_reorders_results', False, reason='missing_endpoint')
        return
    if not config.get('model'):
        note('reranker_endpoint_reachable', False, reason='missing_model')
        note('reranker_reorders_results', False, reason='missing_model')
        return

    try:
        endpoint = normalize_reranker_endpoint(str(config['endpoint']))
    except Exception as exc:
        reason = str(exc) or 'invalid_endpoint'
        note(
            'reranker_endpoint_reachable',
            False,
            error_type=type(exc).__name__,
            reason=reason,
            stage='normalize_endpoint',
        )
        note(
            'reranker_reorders_results',
            False,
            error_type=type(exc).__name__,
            reason=reason,
            stage='normalize_endpoint',
        )
        return
    if reranker_remote_call_blocked(config, endpoint):
        note_reranker_skipped(
            'reranker_remote_blocked_by_policy',
            remote_call_blocked=True,
            network_call=False,
            **sanitized_reranker_endpoint_fields(endpoint),
        )
        return

    parsed, info = call_reranker_endpoint(config, endpoint)
    note('reranker_endpoint_reachable', bool(info.get('ok')), **without_ok(info))
    if not info.get('ok') or parsed is None:
        note('reranker_reorders_results', False, reason='endpoint_probe_failed')
        return

    reorder = reranker_reorder_evidence(parsed)
    note('reranker_reorders_results', bool(reorder.get('ok')), **without_ok(reorder))


def receipt_dicts_from(value: Any) -> list[dict[str, Any]]:
    """Return operation-style receipt dicts without parsing raw text payloads."""
    receipts: list[dict[str, Any]] = []
    if isinstance(value, dict):
        receipts.append(value)
        # MCP JSON-RPC and REST errors wrap their documented terminal receipts
        # under ``error.data`` and ``error`` respectively. Keep traversal
        # deliberately limited to those protocol envelopes; arbitrary JSON
        # object shapes must not become cleanup evidence.
        for key in ('structuredContent', 'result', 'payload', 'response', 'error', 'data', 'terminal_receipt'):
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


_MAX_RUST_WIRE_INTEGER = (1 << 64) - 1


def _is_nonnegative_rust_wire_integer(value: Any) -> bool:
    return (
        isinstance(value, int)
        and not isinstance(value, bool)
        and 0 <= value <= _MAX_RUST_WIRE_INTEGER
    )


def _attempt_envelope_is_coherent_no_write(receipts: list[dict[str, Any]]) -> bool:
    """Reject no-write classification when any receipt can represent a write."""
    if not receipts:
        return False
    for receipt in receipts:
        for key in ('created_drawer_ids', 'cleanup_drawer_ids'):
            if key in receipt and (
                not isinstance(receipt[key], list)
                or any(not isinstance(item, str) or not item for item in receipt[key])
            ):
                return False
        if (
            'operation_id' in receipt
            or 'state' in receipt
            or 'timed_out' in receipt
            or 'status' in receipt
            or receipt.get('outcome') not in (None, 'admission_blocked')
            or receipt.get('ok') is True
            or receipt.get('success') is True
            or receipt.get('accepted') is True
        ):
            return False
        if 'returncode' in receipt and (
            not isinstance(receipt['returncode'], int)
            or isinstance(receipt['returncode'], bool)
            or receipt['returncode'] == 0
        ):
            return False
    return True


def holder_budget_no_write_receipt(value: Any) -> dict[str, Any] | None:
    """Return only a coherent full-attempt holder-budget no-write receipt."""
    if created_ids_from(value):
        return None
    receipts = receipt_dicts_from(value)
    if not _attempt_envelope_is_coherent_no_write(receipts):
        return None
    for receipt in receipts:
        capacity = receipt.get('capacity')
        headroom = receipt.get('headroom')
        admission = receipt.get('profile_admission')
        if (
            receipt.get('outcome') != 'admission_blocked'
            or receipt.get('reason') != 'holder_budget_exceeded'
            or receipt.get('action') != 'write_refused'
            or receipt.get('created_drawer_ids') != []
            or receipt.get('cleanup_drawer_ids') != []
            or not isinstance(capacity, dict)
            or not isinstance(headroom, dict)
            or not isinstance(admission, dict)
        ):
            continue
        fields = (
            capacity.get('holders'), capacity.get('cache_bytes'),
            headroom.get('holders'), headroom.get('cache_bytes'),
            admission.get('active_holders'), admission.get('configured_holder_limit'),
            admission.get('active_cache_bytes'), admission.get('configured_cache_bytes'),
            admission.get('reaped_stale_holders_this_snapshot'),
            admission.get('reserved_service_holders'), admission.get('service_holders'),
            admission.get('requested_cache_bytes'),
        )
        if not all(_is_nonnegative_rust_wire_integer(field) for field in fields):
            continue
        holders, cache_bytes, holder_headroom, cache_headroom = fields[:4]
        active_holders, configured_holders, active_cache, configured_cache = fields[4:8]
        _reaped, reserved_holders, service_holders, requested_cache = fields[8:]
        budget_reason = admission.get('budget_reason')
        if (
            holders == 0
            or cache_bytes == 0
            or requested_cache == 0
            or configured_holders != holders
            or configured_cache != cache_bytes
            or holder_headroom != max(0, holders - active_holders)
            or cache_headroom != max(0, cache_bytes - active_cache)
            or service_holders > active_holders
            or reserved_holders > holders
            or budget_reason not in {
                'holder_limit', 'cache_budget', 'reserved_service_slots',
            }
        ):
            continue
        if budget_reason == 'holder_limit' and active_holders < holders:
            continue
        if budget_reason == 'cache_budget' and (
            active_holders >= holders or active_cache + requested_cache <= cache_bytes
        ):
            continue
        if budget_reason == 'reserved_service_slots' and (
            active_holders >= holders
            or active_cache + requested_cache > cache_bytes
            or reserved_holders == 0
            or active_holders + 1 + reserved_holders <= holders
        ):
            continue
        return {
            'outcome': 'admission_blocked',
            'reason': 'holder_budget_exceeded',
            'cleanup_required': False,
        }
    return None


def create_terminal_receipt(value: Any) -> dict[str, Any] | None:
    """Classify only the documented cleanup-safe create terminal contracts."""
    created_ids = created_ids_from(value)
    if created_ids:
        return {
            'outcome': 'write_accepted',
            'created_drawer_ids': created_ids,
            'cleanup_required': True,
        }
    receipts = receipt_dicts_from(value)
    for receipt in receipts:
        if receipt.get('outcome') != 'admission_blocked':
            continue
        if receipt.get('action') != 'write_refused':
            return None
        created = receipt.get('created_drawer_ids')
        cleanup = receipt.get('cleanup_drawer_ids')
        if created != [] or cleanup != []:
            return None
        reason = receipt.get('reason')
        return {
            'outcome': 'admission_blocked',
            'reason': reason if isinstance(reason, str) and reason else 'unknown_admission_reason',
            'cleanup_required': False,
        }
    return None


def note_no_write_create(create_label: str, downstream_label: str, receipt: dict[str, Any] | None) -> bool:
    """Record a proven no-write admission without requesting cleanup."""
    if not receipt or receipt.get('outcome') != 'admission_blocked':
        return False
    fields = {
        'outcome': 'admission_blocked',
        'reason': receipt.get('reason'),
        'cleanup_required': False,
    }
    note(create_label, True, **fields)
    note(downstream_label, True, skipped='admission_blocked_no_write', **fields)
    return True


def terminal_state(value: Any) -> bool:
    return operation_state_from(value) in {'completed', 'rejected', 'failed'}


def operation_id_from(value: Any) -> str | None:
    for receipt in receipt_dicts_from(value):
        operation_id = receipt.get('operation_id')
        if isinstance(operation_id, str) and operation_id:
            return operation_id
    return None


def followable_timeout_operation_id(value: Any) -> str | None:
    """Return an accepted MCP wait-timeout receipt's operation ID, if any."""
    if holder_budget_no_write_receipt(value) is not None:
        return None
    for receipt in receipt_dicts_from(value):
        operation_id = receipt.get('operation_id')
        if receipt.get('timed_out') is True and isinstance(operation_id, str) and operation_id:
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
    stdout_bytes = 0
    stderr_bytes = 0

    def delete(drawer_id: str) -> bool:
        nonlocal stdout_bytes, stderr_bytes
        proc = run_child_process(['mempal', 'delete', drawer_id], timeout=60, io_category='cli_child_processes')
        stdout_bytes += len(proc['stdout'])
        stderr_bytes += len(proc['stderr'])
        return proc['returncode'] == 0

    def verify_absent(drawer_id: str) -> bool:
        nonlocal stdout_bytes, stderr_bytes
        proc = run_child_process(['mempal', 'view', drawer_id, '--all-projects'], timeout=60, io_category='cli_child_processes')
        stdout_bytes += len(proc['stdout'])
        stderr_bytes += len(proc['stderr'])
        expected = f'drawer {drawer_id} not found'.encode()
        return proc['returncode'] != 0 and expected in proc['stderr']

    result = _SMOKE_RUNTIME.cleanup_exact_ids(unique_ids, checkpoint=_checkpoint_manifest, delete=delete, verify_absent=verify_absent, mark_cleaned=_mark_verified_cleaned)
    active_matches_after_deletes: int | None = None
    if unique_ids and room is not None:
        rc, _out, _err, parsed, _shape = run_cli(
            label + '_post_cleanup_search',
            ['mempal', 'search', MARKER, '--top-k', '5', '--json'],
            expect_json=True,
            timeout=180,
        )
        if rc == 0:
            active_matches_after_deletes = count_marker_matches(parsed, room)
    result.update({'stdout_bytes': stdout_bytes, 'stderr_bytes': stderr_bytes})
    if active_matches_after_deletes is not None:
        result['active_matches_after_deletes'] = active_matches_after_deletes
    note(label, result['failed_count'] == 0, **result)
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
    receipt = create_terminal_receipt(value)
    info: dict[str, Any] = {
        'operation_id_present': bool(operation_id),
        'operation_state': operation_state_from(value),
        'recovered_via': None,
        'recovered_state': None,
    }
    if receipt and receipt.get('outcome') == 'admission_blocked':
        info.update(receipt)
    if ids or operation_id is None:
        return ids, info

    waited = wait_operation(operation_id, wait_label)
    ids = created_ids_from(waited)
    info['recovered_via'] = wait_label
    info['recovered_state'] = operation_state_from(waited)
    return ids, info


def recovery_fields(info: dict[str, Any]) -> dict[str, Any]:
    return {key: value for key, value in info.items() if key != 'reason' and value not in (None, False)}


def _rest_ingest_fallback(
    content: str, label: str, supersedes: str | None = None, room: str = 'cli'
) -> tuple[list[str], dict[str, Any] | None]:
    """Retry direct writes through daemon REST with safe terminal outcomes."""
    import urllib.request
    payload: dict[str, Any] = {
        'content': content,
        'wing': 'smoke',
        'room': room,
        'source_type': 'agent_inference',
        'memory_kind': 'evidence',
        'domain': 'project',
        'field': 'smoke',
    }
    if supersedes:
        payload['supersedes'] = supersedes
    _checkpoint_manifest()
    old_handler = signal.getsignal(signal.SIGALRM)
    old_timer = signal.setitimer(signal.ITIMER_REAL, 0)

    def _alarm_handler(_signum: int, _frame: Any) -> None:
        raise TimeoutError('REST ingest fallback hard timeout')

    signal.signal(signal.SIGALRM, _alarm_handler)
    signal.setitimer(signal.ITIMER_REAL, 35)
    try:
        try:
            req = urllib.request.Request(
                'http://127.0.0.1:3080/api/ingest',
                data=json.dumps(payload).encode(),
                headers={'Content-Type': 'application/json'},
                method='POST',
            )
            resp = urllib.request.urlopen(req, timeout=30)
            if resp.status in (200, 201):
                body = json.loads(resp.read().decode())
                ids = created_ids_from(body)
                receipt = create_terminal_receipt(body)
                if ids:
                    _remember_created_ids(ids)
                    note(label, True, created_id_count=len(ids), json=json_shape(body))
                    return ids, receipt
                if receipt and receipt.get('outcome') == 'admission_blocked':
                    note(label, True, http_status=resp.status, json=json_shape(body), **receipt)
                    return [], receipt
                note(label, False, error_type='MissingTerminalReceipt', http_status=resp.status, json=json_shape(body))
                return [], None
        except HTTPError as exc:
            raw_body = exc.read(8193)
            if len(raw_body) > 8192:
                note(label, False, error_type='HTTPErrorBodyTooLarge', http_status=exc.code)
                return [], None
            body, shape = parse_json_bytes(raw_body)
            receipt = create_terminal_receipt(body)
            if receipt and receipt.get('outcome') == 'admission_blocked':
                note(label, True, http_status=exc.code, json=shape, **receipt)
                return [], receipt
            note(label, False, error_type='HTTPError', http_status=exc.code, json=shape)
            return [], None
    except Exception as exc:
        note(label, False, error_type=type(exc).__name__)
        return [], None
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, old_handler)
        signal.setitimer(signal.ITIMER_REAL, *old_timer)
    note(label, False, error_type='MissingTerminalReceipt')
    return [], None


def _mcp_tool_with_hard_timeout(
    client: 'McpClient',
    tool_name: str,
    args: dict[str, Any],
    timeout: float,
) -> tuple[dict[str, Any] | None, dict[str, Any]]:
    """Interrupt a blocked MCP readline and synchronously reap its process."""
    result_box: dict[str, Any] = {}
    hard_timed_out = False
    timer = signal.ITIMER_REAL
    old_handler = signal.getsignal(signal.SIGALRM)
    old_timer = signal.setitimer(timer, 0)

    def _alarm_handler(signum: int, frame: Any) -> None:
        nonlocal hard_timed_out
        hard_timed_out = True
        raise TimeoutError(f'MCP tool {tool_name} hard timeout after {timeout}s')

    signal.signal(signal.SIGALRM, _alarm_handler)
    signal.setitimer(timer, timeout)
    try:
        structured, info = client.tool(tool_name, args, timeout=timeout + 5)
        result_box['structured'] = structured
        result_box['info'] = info
    except Exception as exc:
        result_box['info'] = {'ok': False, 'error_type': type(exc).__name__}
    finally:
        signal.setitimer(timer, 0)
        try:
            if hard_timed_out:
                client.close()
        finally:
            signal.signal(signal.SIGALRM, old_handler)
            signal.setitimer(timer, *old_timer)

    if 'info' in result_box:
        return result_box.get('structured'), result_box['info']
    return None, {'ok': False, 'error_type': 'UnknownError'}


def cli_crud() -> list[str]:
    cleanup_ids: list[str] = []
    content = json.dumps({'content': f'{MARKER} reversible CLI smoke drawer; nonce {NONCE}; lexical tokens quorvax nimbledrift zettaplum; safe to delete', 'wing': 'smoke', 'room': 'cli', 'source_type': 'agent_inference', 'memory_kind': 'evidence', 'domain': 'project', 'field': 'smoke'}) + '\n'
    _checkpoint_manifest()
    rc, _out, _err, parsed, _shape = run_cli(
        'cli_create',
        ['mempal', 'ingest', '--stdin', '--wing', 'smoke', '--room', 'cli', '--source-type', 'agent_inference', '--memory-kind', 'evidence', '--domain', 'project', '--field', 'smoke', '--no-gate', '--wait', '--wait-timeout-secs', '90', '--json'],
        input_text=content,
        expect_json=True,
        timeout=130,
    )
    create_attempt = [parsed, {'returncode': rc}]
    ids, create_recovery = recover_created_ids(parsed, 'cli_create_wait')
    direct_receipt = holder_budget_no_write_receipt(create_attempt)
    _remember_created_ids(ids)
    if not ids and direct_receipt and note_no_write_create('cli_create', 'cli_crud', direct_receipt):
        return cleanup_ids

    # Fallback: if CLI direct-write fails due to daemon writer lease, retry via REST.
    # The fork's daemon holds a long-lived sqlite-writer lease; CLI ingest that
    # writes directly to the DB will be rejected when the daemon is active.
    # REST ingest goes through the daemon and either writes or returns a cleanup-safe no-write receipt.
    if not ids:
        rest_ids, _ = _rest_ingest_fallback(
            f'{MARKER} reversible CLI smoke drawer; nonce {NONCE}; lexical tokens quorvax nimbledrift zettaplum; safe to delete',
            'cli_create_rest_fallback',
        )
        if rest_ids:
            ids = rest_ids
            create_recovery = {'recovered_via': 'rest_fallback'}
            note('cli_create', True, created_id_count=len(ids), via='rest_fallback')
    elif ids and create_recovery.get('recovered_via'):
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
    _checkpoint_manifest()
    run_cli('cli_pin', ['mempal', 'pin', created_id], timeout=60)
    _checkpoint_manifest()
    run_cli('cli_unpin', ['mempal', 'unpin', created_id], timeout=60)
    run_cli('cli_pinned_after', ['mempal', 'pinned', '--json'], expect_json=True, timeout=60)

    update_content = json.dumps({'content': f'{MARKER} reversible CLI smoke drawer updated; nonce {NONCE[::-1]}; lexical tokens ploverquartz rivetmint yondercoil; safe to delete', 'wing': 'smoke', 'room': 'cli', 'source_type': 'agent_inference', 'memory_kind': 'evidence', 'domain': 'project', 'field': 'smoke'}) + '\n'
    _checkpoint_manifest()
    rc, _out, _err, upd_parsed, _shape = run_cli(
        'cli_update',
        ['mempal', 'ingest', '--stdin', '--wing', 'smoke', '--room', 'cli', '--source-type', 'agent_inference', '--memory-kind', 'evidence', '--domain', 'project', '--field', 'smoke', '--no-gate', '--supersedes', created_id, '--wait', '--wait-timeout-secs', '90', '--json'],
        input_text=update_content,
        expect_json=True,
        timeout=130,
    )
    upd_ids, update_recovery = recover_created_ids(upd_parsed, 'cli_update_wait')
    _remember_created_ids(upd_ids)

    # Fallback: retry update via REST if direct write failed (writer lease).
    if not upd_ids:
        rest_upd_ids, _rest_update_receipt = _rest_ingest_fallback(
            f'{MARKER} reversible CLI smoke drawer updated; nonce {NONCE[::-1]}; lexical tokens ploverquartz rivetmint yondercoil; safe to delete',
            'cli_update_rest_fallback',
            supersedes=created_id,
        )
        if rest_upd_ids:
            upd_ids = rest_upd_ids
            update_recovery = {'recovered_via': 'rest_fallback'}
            note('cli_update', True, created_id_count=len(upd_ids), via='rest_fallback')
    elif upd_ids and update_recovery.get('recovered_via'):
        note('cli_update', True, created_id_count=len(upd_ids), **recovery_fields(update_recovery))
    if not upd_ids:
        delete_exact_ids_cli(cleanup_ids, 'cli_cleanup_after_update_failure', room='cli')
        note('cli_crud', False, reason='update_missing_created_drawer_ids', cleanup_id_count=len(cleanup_ids), **recovery_fields(update_recovery))
        return cleanup_ids
    cleanup_ids.extend(upd_ids)
    SUMMARY['created_counts']['cli'] = len(cleanup_ids)
    run_cli('cli_read_updated', ['mempal', 'view', upd_ids[0], '--all-projects'], timeout=60)

    delete_result = delete_exact_ids_cli(cleanup_ids, 'cli_delete_batch', room='cli')
    deleted = int(delete_result['deleted_count'])
    delete_failures = int(delete_result['failed_count'])
    SUMMARY['cleanup']['cli_deleted_count'] = deleted
    _rc, _out, _err, post_parsed, _shape = run_cli('cli_search_post_delete', ['mempal', 'search', MARKER, '--top-k', '5', '--json'], expect_json=True, timeout=180)
    post_matches = count_marker_matches(post_parsed, 'cli')
    if post_matches > 0:
        SUMMARY['cleanup']['failures'] += delete_failures
    note('cli_crud', post_matches == 0 and deleted > 0, created_id_count=len(cleanup_ids), deleted_count=deleted, delete_failed_attempt_count=delete_failures, post_delete_active_matches=post_matches)
    return cleanup_ids


McpClient = _SMOKE_RUNTIME.McpClient


def new_mcp_client(process_factory: Any = subprocess.Popen) -> McpClient:
    lifecycle = SUMMARY.setdefault(
        'mcp_stdio_lifecycle',
        {'process_count': 0, 'exited_count': 0, 'killed_count': 0, 'roles': {}},
    )
    return McpClient(
        command=['mempal', 'serve', '--mcp'],
        cwd=REPO,
        registry=OWNED_MCP_REGISTRY,
        read_proc_io=read_proc_io,
        record_proc_io_delta=record_proc_io_delta,
        json_shape=json_shape,
        lifecycle_receipt=lifecycle,
        process_factory=process_factory,
    )


def terminate_and_reap_owned_mcp_children(timeout: float = 1.0) -> dict[str, Any]:
    return OWNED_MCP_REGISTRY.terminate_and_reap(timeout)


def run_with_owned_mcp_cleanup(run: Any) -> Any:
    """Run a smoke entry point and reap owned MCP children on every exit path."""
    try:
        return run()
    except BaseException as error:
        SUMMARY['mcp_owned_children_after_failure'] = terminate_and_reap_owned_mcp_children(timeout=5.0)
        note('runner_exception', False, error_type=type(error).__name__)
        SUMMARY['overall_ok'] = False
        finalize_cleanup_manifest(SUMMARY, checkpoint=False)
        print(json.dumps(SUMMARY, sort_keys=True))
        return 1
    finally:
        terminate_and_reap_owned_mcp_children(timeout=5.0)


def _checkpoint_manifest() -> None:
    if CLEANUP_MANIFEST is not None:
        CLEANUP_MANIFEST.checkpoint()


def _remember_created_ids(drawer_ids: list[str]) -> None:
    if drawer_ids and CLEANUP_MANIFEST is not None:
        CLEANUP_MANIFEST.add_created_ids(drawer_ids)


def _mark_verified_cleaned(drawer_ids: list[str]) -> None:
    if drawer_ids and CLEANUP_MANIFEST is not None:
        CLEANUP_MANIFEST.mark_cleaned(drawer_ids)


def finalize_cleanup_manifest(summary: dict[str, Any], *, checkpoint: bool = True) -> None:
    _SMOKE_RUNTIME.finalize_cleanup_manifest(CLEANUP_MANIFEST, summary, checkpoint=checkpoint)


def run_fallback_after_mcp_reaped(
    client: McpClient | None,
    label: str,
    fallback: Any, failure_result: Any = None,
) -> Any:
    """Permit a write fallback only after every owned MCP holder is reaped."""
    if client is not None:
        client.close()
    sweep = terminate_and_reap_owned_mcp_children(timeout=5.0)
    if sweep['remaining_count'] != 0 or OWNED_MCP_CHILDREN:
        note(
            f'mcp_{label}_fallback_holder_clear',
            False,
            reason='owned_mcp_holder_not_reaped',
            remaining_count=len(OWNED_MCP_CHILDREN),
        )
        return [] if failure_result is None else failure_result
    _checkpoint_manifest()
    result = fallback()
    if isinstance(result, list):
        _remember_created_ids([value for value in result if isinstance(value, str)])
    return result


def run_exact_cli_cleanup_after_mcp(drawer_ids: list[str], label: str) -> Any:
    """Run exact CLI cleanup only after the owned MCP registry is empty."""
    return run_fallback_after_mcp_reaped(None, label, lambda: delete_exact_ids_cli(drawer_ids, label + '_delete', room='mcp'))


def delete_exact_ids_mcp(client: McpClient, drawer_ids: list[str]) -> dict[str, int]:
    def delete(drawer_id: str) -> bool:
        structured, info = client.tool('mempal_delete', {'drawer_id': drawer_id}, timeout=60)
        return bool(info.get('ok')) and isinstance(structured, dict) and structured.get('deleted') is True

    def verify_absent(drawer_id: str) -> bool:
        _structured, info = client.tool('mempal_read_drawer', {'drawer_id': drawer_id, 'all_projects': True}, timeout=60)
        return info.get('error_code') == MCP_RESOURCE_NOT_FOUND_ERROR_CODE

    return _SMOKE_RUNTIME.cleanup_exact_ids(drawer_ids, checkpoint=_checkpoint_manifest, delete=delete, verify_absent=verify_absent, mark_cleaned=_mark_verified_cleaned)


def mcp_start_initialized() -> McpClient:
    client = new_mcp_client()
    try:
        init = client.call('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {}, 'clientInfo': {'name': 'mempal-skill-smoke', 'version': '0'}}, timeout=15)
        note('mcp_initialize', 'result' in init, result_fields=sorted(init.get('result', {}).keys()) if isinstance(init.get('result'), dict) else [])
        client.notify('notifications/initialized')
        return client
    except BaseException:
        try:
            client.close()
        except Exception:
            pass
        try:
            terminate_and_reap_owned_mcp_children(timeout=5.0)
        except Exception:
            pass
        raise


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

        # Keep MCP write regressions visible before trying the guarded fallback.
        create_args = {'content': f'{MARKER} reversible MCP smoke drawer; nonce {NONCE}; lexical tokens azurequill basaltfern cobaltlyric; safe to delete', 'wing': 'smoke', 'room': 'mcp', 'source_type': 'agent_inference', 'memory_kind': 'evidence', 'domain': 'project', 'field': 'smoke', 'smoke': True, 'wait': True, 'wait_timeout_secs': 15}
        _checkpoint_manifest()
        create, info = _mcp_tool_with_hard_timeout(client, 'mempal_ingest', create_args, timeout=30)
        create_receipt = [create, info]
        ids = created_ids_from(create_receipt)
        mcp_receipt = holder_budget_no_write_receipt(create_receipt)
        _remember_created_ids(ids)
        create_operation_id = operation_id_from(create_receipt)
        create_timeout_operation_id = followable_timeout_operation_id(create_receipt)
        create_recovery: dict[str, Any] = {
            'operation_id_present': bool(create_operation_id),
            'operation_state': operation_state_from(create_receipt),
        }
        if not ids and mcp_receipt and note_no_write_create(
            'mcp_create', 'mcp_inconclusive_no_cleanup_id', mcp_receipt
        ):
            return cleanup_ids
        if create_operation_id and client is not None:
            status_structured, status_info = client.tool(
                'mempal_operation_status',
                {'operation_id': create_operation_id},
                timeout=30,
            )
            note('mcp_operation_status', bool(status_info.get('ok')), **without_ok(status_info))
            status_ids = created_ids_from(status_structured)
            if not ids and status_ids:
                ids = status_ids
                _remember_created_ids(ids)
                create_recovery.update({
                    'recovered_via': 'mcp_operation_status',
                    'recovered_state': operation_state_from(status_structured),
                })
        elif 'mempal_operation_status' in tool_names:
            note('mcp_operation_status', True, skipped='no_operation_receipt')
        else:
            note('mcp_operation_status', True, skipped='tool_not_advertised')

        if not ids and create_timeout_operation_id:
            # A non-terminal MCP ingest receipt means the daemon may still be
            # processing the write. Close this stdio server before following the
            # operation via CLI so the smoke runner never observes a result while
            # its own MCP process is still an extra SQLite holder.
            waited = run_fallback_after_mcp_reaped(
                client,
                'create_cli_wait',
                lambda: wait_operation(create_timeout_operation_id, 'mcp_create_cli_wait'),
            )
            client = None
            ids = created_ids_from(waited)
            _remember_created_ids(ids)
            create_recovery.update({'recovered_via': 'mcp_create_cli_wait', 'recovered_state': operation_state_from(waited)})
            SUMMARY['mcp_ingest_fallback_to_cli'] += 1

        # Fallback: if MCP ingest fails/hangs (writer lease), retry via REST so
        # follow-on read/delete paths still have a drawer to exercise.
        if not ids:
            rest_ids, _ = run_fallback_after_mcp_reaped(
                client,
                'create_rest',
                lambda: _rest_ingest_fallback(
                    f'{MARKER} reversible MCP smoke drawer; nonce {NONCE}; lexical tokens azurequill basaltfern cobaltlyric; safe to delete',
                    'mcp_create_rest_fallback',
                    room='mcp',
                ), failure_result=([], None))
            client = None
            if rest_ids:
                ids = rest_ids
                create_recovery = {'recovered_via': 'rest_fallback'}
                SUMMARY['mcp_ingest_fallback_to_cli'] += 1

        if ids:
            clear_probe_failures('mcp_create_rest', 'mcp_create_rest_fallback')
        # MCP create passes when the MCP tool returned IDs immediately or when a
        # queued operation was recovered via mempal_operation_status. REST
        # fallback keeps the suite going but does not mask MCP write failure.
        mcp_recovered = create_recovery.get('recovered_via') in ('mcp_create_cli_wait',)
        mcp_create_ok = bool(ids) and (bool(info.get('ok')) or mcp_recovered) and create_recovery.get('recovered_via') != 'rest_fallback'
        note('mcp_create', mcp_create_ok, created_id_count=len(ids), **recovery_fields(create_recovery), **without_ok(info))
        if not ids:
            note(
                'mcp_inconclusive_no_cleanup_id',
                False,
                reason='create_missing_created_drawer_ids',
                **recovery_fields(create_recovery),
                product_issue='https://github.com/RyderFreeman4Logos/mempal/issues/834',
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

        # Call MCP update first under the hard timeout; only fall back to REST
        # if MCP fails/hangs, so MCP write regressions are surfaced rather than
        # masked by a REST bypass.
        update_args = {'content': f'{MARKER} reversible MCP smoke drawer updated; nonce {NONCE[::-1]}; lexical tokens deltaorchid embervault frostcairn; safe to delete', 'wing': 'smoke', 'room': 'mcp', 'source_type': 'agent_inference', 'memory_kind': 'evidence', 'domain': 'project', 'field': 'smoke', 'smoke': True, 'supersedes': created_id, 'wait': True, 'wait_timeout_secs': 15}
        _checkpoint_manifest()
        update, uinfo = _mcp_tool_with_hard_timeout(client, 'mempal_ingest', update_args, timeout=30)
        update_receipt = [update, uinfo]
        upd_ids = created_ids_from(update_receipt)
        _remember_created_ids(upd_ids)
        update_timeout_operation_id = followable_timeout_operation_id(update_receipt)
        update_recovery: dict[str, Any] = {
            'operation_id_present': bool(operation_id_from(update_receipt)),
            'operation_state': operation_state_from(update_receipt),
        }
        if not upd_ids and update_timeout_operation_id:
            waited = run_fallback_after_mcp_reaped(
                client,
                'update_cli_wait',
                lambda: wait_operation(update_timeout_operation_id, 'mcp_update_cli_wait'),
            )
            client = None
            upd_ids = created_ids_from(waited)
            _remember_created_ids(upd_ids)
            update_recovery.update({'recovered_via': 'mcp_update_cli_wait', 'recovered_state': operation_state_from(waited)})
            SUMMARY['mcp_ingest_fallback_to_cli'] += 1

        # Fallback: if MCP update fails/hangs (writer lease), retry via REST so
        # follow-on read/delete paths still have an updated drawer to exercise.
        if not upd_ids:
            rest_upd_ids, _rest_update_receipt = run_fallback_after_mcp_reaped(
                client,
                'update_rest',
                lambda: _rest_ingest_fallback(
                    f'{MARKER} reversible MCP smoke drawer updated; nonce {NONCE[::-1]}; lexical tokens deltaorchid embervault frostcairn; safe to delete',
                    'mcp_update_rest_fallback',
                    supersedes=created_id,
                    room='mcp',
                ), failure_result=([], None))
            client = None
            if rest_upd_ids:
                upd_ids = rest_upd_ids
                update_recovery = {'recovered_via': 'rest_fallback'}
                SUMMARY['mcp_ingest_fallback_to_cli'] += 1

        if upd_ids:
            clear_probe_failures('mcp_update_rest', 'mcp_update_rest_fallback')
        # MCP update passes when the MCP tool returned IDs immediately or when a
        # queued operation was recovered via mempal_operation_status. REST
        # fallback keeps the suite going but does not mask MCP write failure.
        mcp_upd_recovered = update_recovery.get('recovered_via') in ('mcp_update_cli_wait',)
        mcp_update_ok = bool(upd_ids) and (bool(uinfo.get('ok')) or mcp_upd_recovered) and update_recovery.get('recovered_via') != 'rest_fallback'
        note('mcp_update', mcp_update_ok, created_id_count=len(upd_ids), **recovery_fields(update_recovery), **without_ok(uinfo))
        if not upd_ids:
            run_exact_cli_cleanup_after_mcp(cleanup_ids, 'mcp_cleanup_after_update_failure')
            note(
                'mcp_inconclusive_no_cleanup_id',
                False,
                reason='update_missing_created_drawer_ids',
                **recovery_fields(update_recovery),
                product_issue='https://github.com/RyderFreeman4Logos/mempal/issues/834',
            )
            return cleanup_ids
        cleanup_ids.extend(upd_ids)
        SUMMARY['created_counts']['mcp'] = len(cleanup_ids)
        if client is None:
            client = mcp_start_initialized()
        structured, info = client.tool('mempal_read_drawer', {'drawer_id': upd_ids[0], 'all_projects': True}, timeout=60)
        note('mcp_read_updated', bool(info.get('ok')), **without_ok(info))

        cleanup_result = delete_exact_ids_mcp(client, cleanup_ids)
        deleted = cleanup_result['deleted_count']
        delete_false_count = cleanup_result['delete_failed_attempt_count']
        SUMMARY['cleanup']['mcp_deleted_count'] = deleted
        post, pinfo = client.tool('mempal_search', {'query': MARKER, 'top_k': 5, 'all_projects': True}, timeout=180)
        post_matches = count_marker_matches(post, 'mcp')
        cleanup_verified = cleanup_result['failed_count'] == 0
        SUMMARY['cleanup']['failures'] += cleanup_result['failed_count']
        note('mcp_delete_batch', cleanup_verified, **cleanup_result)
        note('mcp_crud', cleanup_verified and bool(pinfo.get('ok')) and post_matches == 0 and deleted > 0, created_id_count=len(cleanup_ids), deleted_count=deleted, delete_false_count=delete_false_count, post_delete_active_matches=post_matches)
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
            client = None
        # If MCP CRUD failed after exposing cleanup-safe IDs, clean them by exact ID.
        if SUMMARY['groups'].get('mcp_crud', {}).get('ok') is not True and cleanup_ids and CLEANUP_MANIFEST is not None and CLEANUP_MANIFEST.pending_count > 0:
            cleanup_result = run_exact_cli_cleanup_after_mcp(cleanup_ids, 'mcp_fallback_cli_cleanup')
            if isinstance(cleanup_result, dict):
                note(
                    'mcp_fallback_cli_cleanup',
                    cleanup_result.get('failed_count') == 0,
                    exact_id_count=len(set(cleanup_ids)),
                    deleted_count=cleanup_result.get('deleted_count', 0),
                )


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


_DOCTOR_CRITICAL_KEYWORDS = ('corrupt', 'mismatch', 'missing', 'incompatible', 'migration failed')


def validate_doctor_health(doc_json: Any) -> dict[str, Any]:
    """Validate a parsed ``mempal doctor --format json`` report.

    Returns a dict with ``ok`` plus diagnostic fields. Used by both
    the smoke runner and unit tests so the validation logic is exercised
    identically in both paths.
    """
    if not isinstance(doc_json, dict):
        return {'ok': False, 'reason': 'not_a_dict'}
    db_info = doc_json.get('db') or {}
    db_schema_version = db_info.get('schema_version') if isinstance(db_info, dict) else None
    supported_schema = doc_json.get('supported_schema_version')
    schema_matches = db_schema_version is not None and db_schema_version == supported_schema
    warnings = doc_json.get('warnings') or []
    critical_warnings = [
        w for w in warnings
        if isinstance(w, str) and any(k in w.lower() for k in _DOCTOR_CRITICAL_KEYWORDS)
    ]
    emb_info = doc_json.get('embedding') or {}
    embedder_degraded = bool(emb_info.get('degraded')) if isinstance(emb_info, dict) else False
    embedding_write_refused = bool(emb_info.get('write_refused')) if isinstance(emb_info, dict) else False
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
    embedding_ok = emb_cooldowns == 0 and not embedder_degraded and not embedding_write_refused
    ok = bool(schema_matches) and len(critical_warnings) == 0 and embedding_ok
    return {
        'ok': ok,
        'db_schema_version': db_schema_version,
        'supported_schema_version': supported_schema,
        'schema_matches': bool(schema_matches),
        'total_warning_count': len(warnings),
        'critical_warning_count': len(critical_warnings),
        'embedder_degraded': embedder_degraded,
        'embedding_write_refused': embedding_write_refused,
        'embedding_endpoint_cooldowns': emb_cooldowns,
        'embedding_terminal_failures': emb_terminal_failures,
        'queue_terminal_failures': emb_terminal_failures,
        'embedding_ok': embedding_ok,
    }


def daemon_status_diagnostics(stdout: bytes) -> dict[str, Any]:
    text = stdout.decode('utf-8', errors='replace')
    embedder_status_source: str | None = None
    embedder_degraded: bool | None = None
    embedder_write_refused: bool | None = None
    queue_terminal_failures: int | None = None
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if line.startswith('rest.embedder_status_source:'):
            embedder_status_source = line.split(':', 1)[1].strip()
        elif line.startswith('rest.embedder_degraded:'):
            value = line.split(':', 1)[1].strip().lower()
            embedder_degraded = value == 'true'
        elif line.startswith('rest.embedder_write_refused:'):
            value = line.split(':', 1)[1].strip().lower()
            embedder_write_refused = value == 'true'
        elif line.startswith('queue.failed_terminal:') or line.startswith('rest.queue_terminal_failures:'):
            value = line.split(':', 1)[1].strip()
            try:
                queue_terminal_failures = int(value)
            except ValueError:
                queue_terminal_failures = None
    return {
        'embedder_status_source': embedder_status_source,
        'embedder_degraded': embedder_degraded,
        'embedder_write_refused': embedder_write_refused,
        'queue_terminal_failures': queue_terminal_failures,
    }


def main() -> int:
    import hashlib
    SUMMARY['marker_hash'] = hashlib.sha256(MARKER.encode()).hexdigest()[:16]
    daemon_pid_before = daemon_main_pid()
    daemon_io_before = read_proc_io(daemon_pid_before)
    child_io_before = child_io_blocks_snapshot()
    SUMMARY['io']['daemon_pid_before'] = daemon_pid_before
    start = time.monotonic()
    run_cli('version', ['mempal', '--version'], timeout=30)
    _daemon_rc, daemon_stdout, _daemon_stderr, _daemon_parsed, _daemon_shape = run_cli('daemon_status_pre', ['mempal', 'daemon', 'status'], timeout=30)
    SUMMARY['groups']['daemon_status_pre'].update(daemon_status_diagnostics(daemon_stdout))
    run_cli('doctor_rest', ['mempal', 'doctor', 'rest', '--format', 'json'], expect_json=True, timeout=60)
    _doc_rc, _doc_out, _doc_err, doc_parsed, _doc_shape = run_cli('doctor_json', ['mempal', 'doctor', '--format', 'json'], expect_json=True, timeout=60)
    doc_validation = validate_doctor_health(doc_parsed)
    note('doctor_json_validation', doc_validation['ok'], **without_ok(doc_validation))
    if not doc_validation['ok']:
        SUMMARY['failures'] = [f for f in SUMMARY['failures'] if f != 'doctor_json_validation']
        SUMMARY['failures'].append('doctor_json_validation')
    run_cli('status', ['mempal', 'status'], timeout=60)
    run_cli('queue_failed', ['mempal', 'queue', 'failed'], timeout=60)
    run_cli('timeline_json', ['mempal', 'timeline', '--since', '1h', '--format', 'json'], expect_json=True, timeout=60)
    run_cli('tail_shape', ['mempal', 'tail', '--limit', '3'], timeout=60)
    run_cli('stats_shape', ['mempal', 'stats'], timeout=60)
    run_cli('pinned_json', ['mempal', 'pinned', '--json'], expect_json=True, timeout=60)
    run_cli('field_taxonomy_json', ['mempal', 'field-taxonomy', '--format', 'json'], expect_json=True, timeout=60)
    run_cli('patterns_json', ['mempal', 'patterns', 'list', '--json'], expect_json=True, timeout=60)
    run_cli('skills_json', ['mempal', 'skills', 'list', '--json'], expect_json=True, timeout=60)
    run_cli('repair_json', ['mempal', 'repair', 'list', '--json'], expect_json=True, timeout=60)
    probe_search_reranker_behavior()

    cli_crud()
    mcp_crud()

    SUMMARY['mcp_owned_children_after_wait'] = terminate_and_reap_owned_mcp_children(timeout=5.0)
    if SUMMARY['mcp_owned_children_after_wait']['remaining_count'] > 0:
        note(
            'mcp_owned_children_reaped',
            False,
            reason='owned_mcp_children_remaining_before_holder_summary',
            remaining_count=SUMMARY['mcp_owned_children_after_wait']['remaining_count'],
        )
    if CLEANUP_MANIFEST is not None and CLEANUP_MANIFEST.pending_count > 0:
        note(
            'cleanup_manifest_pending',
            False,
            reason='verified_cleanup_incomplete',
            remaining_count=CLEANUP_MANIFEST.pending_count,
        )
    SUMMARY['holders_after'] = holder_summary()
    daemon_pid_after = daemon_main_pid()
    daemon_io_after = read_proc_io(daemon_pid_after)
    child_io_after = child_io_blocks_snapshot()
    SUMMARY['daemon'] = classify_daemon(daemon_pid_before, daemon_pid_after)
    binary_result = check_binary_consistency(daemon_pid_after)
    SUMMARY['binary_consistency'] = binary_result
    note('binary_consistency', bool(binary_result.get('ok')), **without_ok({k: v for k, v in binary_result.items() if k != 'daemon_exe'}))
    SUMMARY['io']['daemon_pid_after'] = daemon_pid_after
    SUMMARY['io']['daemon_proc_io_delta'] = io_delta(daemon_io_before, daemon_io_after) if daemon_pid_before == daemon_pid_after else None
    SUMMARY['io']['child_block_io_delta'] = child_io_blocks_delta(child_io_before, child_io_after)
    SUMMARY['duration_ms'] = int((time.monotonic() - start) * 1000)
    SUMMARY['conformance'] = build_conformance_report()
    SUMMARY['overall_ok'] = smoke_overall_ok(SUMMARY)
    finalize_cleanup_manifest(SUMMARY)
    append_io_history()
    print(json.dumps(SUMMARY, sort_keys=True))
    return 0 if SUMMARY['overall_ok'] else 1


if __name__ == '__main__':
    if any(arg in ('-h', '--help') for arg in sys.argv[1:]):
        print(
            "usage: full_smoke.py\n\n"
            "Runs the aggregate mempal CLI/MCP smoke suite and prints a JSON summary."
        )
        raise SystemExit(0)
    raise SystemExit(run_with_owned_mcp_cleanup(main))
