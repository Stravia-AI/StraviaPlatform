from __future__ import annotations

import base64
import json
import os
import re
import socket
import subprocess
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from http.client import HTTPConnection, HTTPException
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from tests.common.helpers import (
    SERVER_STARTUP_TIMEOUT, find_free_port, http_request, initialize_server,
    stop_stravia_server, wait_for_setup_token, wait_until_ready,
)
from tests.common.measure_protocol_http import Catalog
from tests.e2e.admin.test_observations import (
    _create_route, _detail, _finished_detail, _forest, _route_interactions, _wait_for,
)

pytestmark = [pytest.mark.e2e, pytest.mark.storage]


class HostLog:
    """Wait on real host output, never poll a runtime barrier or sleep."""

    MARKERS = re.compile(
        r'STRAVIA_TEST_OBSERVATION_QUEUE_(?:HELD|FULL|RELEASED|DRAINED)(?![A-Z_])'
        r'|STRAVIA_TEST_TRACE_WIRE_CAPTURE_LIMIT(?![A-Z_])'
        r'|STRAVIA_DESKTOP_TEST_READY(?![A-Z_])'
    )

    def __init__(self, process):
        self.lines = []
        self.markers = set()
        self.condition = threading.Condition()
        self.ended = False
        self.process = process
        self.reader = threading.Thread(target=self._drain, daemon=True)
        self.reader.start()

    def _drain(self):
        for line in self.process.stdout:
            with self.condition:
                self.lines.append(line.rstrip('\n'))
                parsed = set(self.MARKERS.findall(line))
                self.markers.update(parsed)
                self.condition.notify_all()
        with self.condition:
            self.ended = True
            self.condition.notify_all()

    def contains(self, marker):
        with self.condition:
            return marker in self.markers

    def safe_state(self):
        with self.condition:
            return {'returncode': self.process.poll(), 'output_ended': self.ended,
                    'markers': sorted(self.markers)}

    def wait(self, marker, timeout=20):
        with self.condition:
            reached = self.condition.wait_for(lambda: self.contains(marker) or self.ended, timeout)
            if not reached or not self.contains(marker):
                pytest.fail(f'host did not reach {marker}: {self.safe_state()}')


@contextmanager
def isolated_host(host, backend, stravia_binary, storage_runtime, tmp_path, controls):
    __tracebackhide__ = True
    schema = None
    database = {'backend': 'sqlite'}
    if backend == 'postgres':
        schema = storage_runtime['make_isolated_schema']('stravia_capacity_scenarios')
        storage_runtime['run_schema_action']('create', pg_url=storage_runtime['pg_url'], schema=schema)
        database = {'backend': 'postgres', 'url': storage_runtime['postgres_dsn_for_schema'](
            storage_runtime['pg_url'], schema)}
    process = None
    log = None
    native_root = None
    catalog = None
    catalog_thread = None
    try:
        if host == 'desktop':
            if os.environ.get('STRAVIA_RUN_NATIVE_STORAGE_TESTS') != '1':
                pytest.skip('native host contracts run serially with STRAVIA_RUN_NATIVE_STORAGE_TESTS=1')
            binary = os.environ.get('STRAVIA_DESKTOP_E2E_BINARY')
            assert binary and Path(binary).is_file(), 'desktop-e2e binary is required'
            command = [binary]
            native_root = tempfile.TemporaryDirectory(prefix='stravia-desktop-e2e-storage-')
            run_root = Path(native_root.name)
            # Native readiness uses the existing private storage-test bootstrap.
            # Negative controls never arm this default fault; storage failure cases
            # override these values with their explicit isolated arm file.
            controls = {'STRAVIA_TEST_STORAGE_FAULT': 'observation',
                        'STRAVIA_TEST_STORAGE_FAULT_KIND': 'io-full',
                        'STRAVIA_TEST_STORAGE_FAULT_ARM_FILE': str(run_root / 'never-arm-storage-fault'),
                        **controls, 'STRAVIA_DESKTOP_E2E_RUN_ROOT': str(run_root)}
        else:
            port = find_free_port()
            base = f'http://127.0.0.1:{port}'
            catalog = ThreadingHTTPServer(('127.0.0.1', 0), Catalog)
            catalog.arrivals = []
            catalog_thread = threading.Thread(target=catalog.serve_forever, daemon=True)
            catalog_thread.start()
            command = [str(stravia_binary), '--data-dir', str(tmp_path / 'data'),
                       '--host', '127.0.0.1', '--port', str(port),
                       '--test-catalog-base-url', f'http://127.0.0.1:{catalog.server_port}',
                       ] + storage_runtime['server_args'](backend)
        process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                   text=True, encoding='utf-8', env={**os.environ, **controls})
        log = HostLog(process)
        if host == 'desktop':
            log.wait('STRAVIA_DESKTOP_TEST_READY', SERVER_STARTUP_TIMEOUT)
            state = json.loads((run_root / 'native-storage-ready.json').read_text(encoding='utf-8'))
            base = f"http://127.0.0.1:{state['port']}"
            auth = {'Authorization': f"Bearer {state['session']['access_token']}"}
        else:
            wait_until_ready(base + '/api/v1/auth/state')
            session = initialize_server(base, wait_for_setup_token(log.lines, process), database)
            auth = session.auth_headers()
        yield {'admin': base, 'proxy': base, 'auth': auth}, process, log
    finally:
        if process is not None:
            stop_stravia_server(process, log.lines if log else [], print_tail=0)
        if catalog is not None:
            catalog.shutdown()
            catalog.server_close()
            catalog_thread.join()
        if native_root is not None:
            native_root.cleanup()
        if schema is not None:
            storage_runtime['run_schema_action']('drop', pg_url=storage_runtime['pg_url'], schema=schema)


HOST_DATABASES = [('server', 'sqlite'), ('server', 'postgres'), ('desktop', 'sqlite')]


@contextmanager
def streaming_upstream():
    entered = threading.Event()
    release = threading.Event()
    attempts = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            attempts.append(body)
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Connection', 'close')
            self.end_headers()
            frame = {'id': 'capacity-live', 'object': 'chat.completion.chunk',
                     'model': body['model'], 'choices': [{'index': 0,
                     'delta': {'content': 'first'}, 'finish_reason': None}]}
            self.wfile.write(('data: ' + json.dumps(frame) + '\n\n').encode())
            self.wfile.flush()
            entered.set()
            if not release.wait(20):
                return
            try:
                frame['choices'][0]['delta']['content'] = 'after-arm'
                self.wfile.write(('data: ' + json.dumps(frame) + '\n\n').encode())
                frame['choices'][0]['delta'] = {}
                frame['choices'][0]['finish_reason'] = 'stop'
                self.wfile.write(('data: ' + json.dumps(frame) + '\n\ndata: [DONE]\n\n').encode())
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                pass

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server, entered, release, attempts
    finally:
        release.set()
        server.shutdown()
        server.server_close()
        thread.join()


@pytest.mark.parametrize('backend', ['sqlite', 'postgres'])
def test_observation_queue_preserves_model_event_times(
    backend, stravia_binary, storage_runtime, tmp_path,
):
    hold_port = find_free_port()
    controls = {'STRAVIA_TEST_OBSERVATION_QUEUE_HOLD_ADDR': f'127.0.0.1:{hold_port}'}
    with isolated_host('server', backend, stravia_binary, storage_runtime, tmp_path, controls) as (env, _, log):
        with streaming_upstream() as (upstream, entered, release, _):
            env['mock'] = f'http://127.0.0.1:{upstream.server_port}'
            model = 'queued-observation-event-times'
            route, key = _create_route(env, model, retry_budget=0)
            hold = socket.create_connection(('127.0.0.1', hold_port), timeout=20)
            try:
                log.wait('STRAVIA_TEST_OBSERVATION_QUEUE_HELD')
                with ThreadPoolExecutor(max_workers=1) as clients:
                    response = clients.submit(
                        http_request, 'POST', env['proxy'] + '/v1/chat/completions',
                        {'model': model, 'stream': True,
                         'messages': [{'role': 'user', 'content': 'Report the synthetic result.'}]},
                        {'Authorization': f'Bearer {key}'},
                    )
                    try:
                        assert entered.wait(20), 'model request did not reach the local upstream'
                        started_by = time.time_ns() // 1_000_000
                        release.set()
                        status, body = response.result(timeout=20)
                        delivered_by = time.time_ns() // 1_000_000
                        assert status == 200 and '[DONE]' in body, body
                    finally:
                        release.set()
            finally:
                hold.close()
                log.wait('STRAVIA_TEST_OBSERVATION_QUEUE_RELEASED')
                log.wait('STRAVIA_TEST_OBSERVATION_QUEUE_DRAINED')

            interaction = _wait_for(
                'completed request observed after releasing the queue',
                lambda: next(iter(_route_interactions(env, route)), None),
            )
            detail = _wait_for(
                'queued lifecycle events persisted',
                lambda: _finished_detail(env, interaction['id']),
            )
            assert len(detail['runs']) == 1, detail
            run = detail['runs'][0]
            assert run['status'] == 'completed', run
            assert not detail['interaction']['observation_gap'], detail
            assert run['delivery_completed_at'] <= delivered_by
            for kind, upper_bound in [
                ('model_turn_started', started_by),
                ('target_attempt_started', started_by),
                ('target_attempt_finished', delivered_by),
                ('model_turn_finished', delivered_by),
            ]:
                matching = [event for event in run['events'] if event['kind'] == kind]
                assert len(matching) == 1, (kind, matching)
                # 模型与交付已在 writer 阻塞期间完成，出队不能重新定义发生时间。
                assert run['started_at'] <= matching[0]['occurred_at'] <= upper_bound, (
                    kind, matching[0]['occurred_at'], upper_bound,
                )


@pytest.mark.parametrize('host,backend', HOST_DATABASES)
@pytest.mark.parametrize('control', ['queue-full', 'trace-limit'])
def test_diagnostic_limits_preserve_real_host_inference(
    host, backend, control, request, storage_runtime, tmp_path,
):
    stravia_binary = request.getfixturevalue('stravia_binary') if host == 'server' else None
    hold_port = find_free_port()
    controls = ({'STRAVIA_TEST_OBSERVATION_QUEUE_CAPACITY': '4',
                 'STRAVIA_TEST_OBSERVATION_QUEUE_HOLD_ADDR': f'127.0.0.1:{hold_port}'}
                if control == 'queue-full' else {'STRAVIA_TEST_TRACE_WIRE_CAPTURE_LIMIT_BYTES': '32'})
    with isolated_host(host, backend, stravia_binary, storage_runtime, tmp_path, controls) as (env, process, log):
        with streaming_upstream() as upstream:
            server, _, release, attempts = upstream
            release.set()
            env['mock'] = f'http://127.0.0.1:{server.server_port}'
            _, key = _create_route(env, 'diagnostic-limit-survival')
            if control == 'trace-limit':
                status, body = http_request('PUT', env['admin'] + '/api/v1/observations/debug',
                                           {'enabled': True, 'confirmed': True}, env['auth'])
                assert status == 200, body
            hold = None
            try:
                if control == 'queue-full':
                    hold = socket.create_connection(('127.0.0.1', hold_port), timeout=20)
                    log.wait('STRAVIA_TEST_OBSERVATION_QUEUE_HELD')
                status, body = http_request('POST', env['proxy'] + '/v1/chat/completions',
                                           {'model': 'diagnostic-limit-survival', 'stream': True,
                                            'messages': [{'role': 'user', 'content': 'ordinary inference'}]},
                                           {'Authorization': f'Bearer {key}'})
                assert status == 200 and '[DONE]' in body, body
                log.wait('STRAVIA_TEST_OBSERVATION_QUEUE_FULL' if control == 'queue-full'
                         else 'STRAVIA_TEST_TRACE_WIRE_CAPTURE_LIMIT')
            finally:
                if hold is not None:
                    hold.close()
                    log.wait('STRAVIA_TEST_OBSERVATION_QUEUE_RELEASED')
                    log.wait('STRAVIA_TEST_OBSERVATION_QUEUE_DRAINED')
            assert process.poll() is None, 'diagnostic loss must not fail the owning host'
            if control == 'queue-full':
                # 丢失 Turn 开始后没有模型执行投影，不能用模型筛选排除这条缺口记录。
                interaction = _wait_for(
                    'queue loss is visible without losing the completed Run',
                    lambda: next(
                        (item for root in _forest(env)['roots']
                         for item in root['interactions']
                         if item['observation_gap']),
                        None,
                    ),
                )
                run = _finished_detail(env, interaction['id'])['runs'][0]
                assert run['status'] == 'completed'
                assert run['client_output_committed'] is True
                assert run['finished_at'] is not None
                assert run['generation_node_id'] is not None
            # Fresh inference must cross admission and reach the actual upstream:
            # a responding status endpoint alone is not proof of business survival.
            status, body = http_request('POST', env['proxy'] + '/v1/chat/completions',
                                       {'model': 'diagnostic-limit-survival', 'stream': True,
                                        'messages': [{'role': 'user', 'content': 'fresh inference'}]},
                                       {'Authorization': f'Bearer {key}'})
            assert status == 200 and '[DONE]' in body, body
            assert len(attempts) == 2
            assert process.poll() is None




LOCAL_FAILURES = [
    (host, backend, source, kind)
    for host, backend in HOST_DATABASES
    for source in ('observation', 'debug', 'artifact')
    for kind in ('io-full', 'ordinary-io')
] + [('server', 'postgres', 'observation', 'postgres-full'),
     ('server', 'sqlite', 'observation', 'sqlite-full'),
     ('desktop', 'sqlite', 'observation', 'sqlite-full')]


@pytest.mark.parametrize('host,backend,source,kind', LOCAL_FAILURES)
def test_storage_failure_preserves_active_and_following_inference(
    host, backend, source, kind, request, storage_runtime, tmp_path,
):
    stravia_binary = request.getfixturevalue('stravia_binary') if host == 'server' else None
    arm = tmp_path / 'arm-storage-fault'
    controls = {'STRAVIA_TEST_STORAGE_FAULT': source,
                'STRAVIA_TEST_STORAGE_FAULT_KIND': kind,
                'STRAVIA_TEST_STORAGE_FAULT_ARM_FILE': str(arm)}
    with isolated_host(host, backend, stravia_binary, storage_runtime, tmp_path, controls) as (env, process, _log):
        with streaming_upstream() as upstream:
            server, entered, release, attempts = upstream
            env['mock'] = f'http://127.0.0.1:{server.server_port}'
            route_id, key = _create_route(env, 'local-storage-survival', retry_budget=3)
            auth = {'Authorization': f'Bearer {key}'}
            upload = None
            if source == 'artifact':
                status, upload = http_request('POST', env['proxy'] + '/v1/artifacts/uploads',
                                              {'mime_type': 'text/plain', 'size': 3}, auth)
                assert status == 201, f'upload creation returned HTTP {status}'
            if source == 'debug':
                status, body = http_request('PUT', env['admin'] + '/api/v1/observations/debug',
                                           {'enabled': True, 'confirmed': True}, env['auth'])
                assert status == 200, body
            connection = HTTPConnection('127.0.0.1', int(env['proxy'].rsplit(':', 1)[1]), timeout=20)
            try:
                connection.request('POST', '/v1/chat/completions', json.dumps({
                    'model': 'local-storage-survival', 'stream': True,
                    'messages': [{'role': 'user', 'content': 'active inference'}]}),
                    {**auth, 'Content-Type': 'application/json'})
                response = connection.getresponse()
                assert response.status == 200
                assert entered.wait(10), 'upstream never entered active inference'
                prefix = []
                while True:
                    line = response.readline()
                    assert line, 'active stream ended before fault'
                    prefix.append(line)
                    if b'first' in line:
                        break
                assert process.poll() is None
                arm.touch()
                if source == 'observation':
                    # Fail a Run event write, not the separate best-effort
                    # rejection recorder, so public Interaction gap is evidence.
                    release.set()
                    # 模型投影本身也可能丢失；隔离实例只有本次推理，不能用模型筛掉缺口。
                    _wait_for('failed observation task leaves a durable gap', lambda: any(
                        item['observation_gap'] for root in _forest(env)['roots']
                        for item in root['interactions']
                    ))
                trigger = HTTPConnection('127.0.0.1', int(env['proxy'].rsplit(':', 1)[1]), timeout=20)
                try:
                    if source == 'artifact':
                        trigger.request('PUT', f"/v1/artifacts/uploads/{upload['upload_id']}/parts/1", b'abc',
                                        {**auth, 'x-upload-token': upload['upload_token']})
                    else:
                        trigger.request('POST', '/v1/chat/completions', '{',
                                        {**auth, 'Content-Type': 'application/json'})
                    rejected = trigger.getresponse()
                    rejected.read()
                    if source == 'artifact':
                        assert rejected.status == 500, 'artifact storage failure must return HTTP 500'
                    else:
                        # Malformed JSON is Axum JsonRejection before the
                        # gateway's JSON protocol-error renderer is reached.
                        assert rejected.status == 400, 'diagnostic failure changed the rejection result'
                finally:
                    trigger.close()
                assert process.poll() is None, 'local failure stopped the host'
                release.set()
                completed = b''.join(prefix) + response.read()
                assert b'first' in completed and b'after-arm' in completed and b'[DONE]' in completed
                assert len(attempts) == 1, 'storage failure retried or replayed active inference'
                if source == 'debug':
                    def partial_capture():
                        status, body = http_request('GET', env['admin'] + '/api/v1/observations/debug',
                                                    headers=env['auth'])
                        assert status == 200, body
                        return body['data']['partial_trace_count'] >= 1
                    _wait_for('real debug storage failure recorded as partial', partial_capture)
                _wait_for('configured storage fault consumed', lambda: not arm.exists())
                status, body = http_request('POST', env['proxy'] + '/v1/chat/completions',
                                           {'model': 'local-storage-survival', 'stream': True,
                                            'messages': [{'role': 'user', 'content': 'following inference'}]}, auth)
                assert status == 200 and 'after-arm' in body and '[DONE]' in body, body
                assert len(attempts) == 2, 'fresh inference must reach upstream once without replay'
                assert process.poll() is None
                # The upstream never reports usage; later successful persistence
                # must not convert that unknown consumption into fabricated zero.
                interaction = _wait_for('following inference observation', lambda: next((
                    item for item in _route_interactions(env, route_id)
                    if item['input_preview'] == 'following inference' and item['status'] == 'completed'
                ), None))
                detail = _wait_for('following inference terminal detail',
                                   lambda: _finished_detail(env, interaction['id']))
                assert detail['interaction']['usage']['input_tokens'] is None
                assert detail['interaction']['usage']['output_tokens'] is None
            finally:
                release.set()
                connection.close()


@contextmanager
def media_background_upstream():
    """Gate the actual vision provider, never a product execution barrier."""
    entered = threading.Event()
    release = threading.Event()
    parent_requests = []
    media_requests = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            assert self.path == '/v1/chat/completions'
            if body['model'] == 'vision':
                media_requests.append(body)
                entered.set()
                # 保持真实上游挂起，跨过公开工具默认的 60 秒同步等待窗口。
                assert release.wait(150), 'background vision provider was not released'
                message = {'role': 'assistant', 'content': json.dumps({
                    'answer': f'A blue square from the real media provider. [{self.server.artifact_path}]',
                    'artifacts': [{'path': self.server.artifact_path}], 'limitations': []})}
                finish = 'stop'
            else:
                parent_requests.append(body)
                if len(parent_requests) == 1:
                    media_tool = next(tool['function']['name'] for tool in body['tools']
                        if 'path' in tool['function']['parameters'].get('properties', {}))
                    message = {'role': 'assistant', 'content': 'background published', 'tool_calls': [
                        {'id': 'read-media', 'type': 'function', 'function': {'name': media_tool,
                         'arguments': json.dumps({'path': self.server.artifact_path})}},
                        {'id': 'client-probe', 'type': 'function',
                         'function': {'name': 'local_probe', 'arguments': '{}'}}]}
                    finish = 'tool_calls'
                else:
                    message = {'role': 'assistant', 'content': 'background continuation completed'}
                    finish = 'stop'
            payload = {'id': 'real-media-background', 'object': 'chat.completion',
                       'model': body['model'], 'choices': [{'index': 0, 'message': message,
                                                          'finish_reason': finish}]}
            encoded = json.dumps(payload).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server, entered, release, parent_requests, media_requests
    finally:
        release.set()
        server.shutdown()
        server.server_close()
        thread.join()


def configure_background_media(env, parent_route):
    status, body = http_request('POST', env['admin'] + '/api/v1/providers', {
        'name': 'background-vision-provider', 'source': {'type': 'custom', 'vendor': 'custom',
        'channel': 'default', 'protocol': 'openai-compatible', 'base_url': env['mock']},
        'credential': {'type': 'api_key', 'value': 'local-vision-fixture'}, 'vendor_options': {}}, env['auth'])
    assert status == 200, body
    provider = body['data']['id']
    status, body = http_request('POST', env['admin'] + f'/api/v1/providers/{provider}/models', {
        'model_id': 'vision', 'metadata': {'id': 'vision', 'reasoning_efforts': ['medium'],
        'modalities': {'input': ['text', 'image'], 'output': ['text']}}}, env['auth'])
    assert status == 201, body
    status, body = http_request('POST', env['admin'] + '/api/v1/models', {
        'model_id': 'background-vision', 'output_modality': 'text', 'targets': [{'provider_id': provider, 'model': 'vision',
        'first_token_timeout_ms': 120_000, 'target_retry_budget': 0, 'target_cooldown_ms': 0}]}, env['auth'])
    assert status == 200, body
    vision = body['data']['id']
    status, body = http_request('PUT', env['admin'] + '/api/v1/media-understanding/image', {
        'enabled': True, 'model_id': vision, 'thinking_level': 'medium'}, env['auth'])
    assert status == 200, body
    # 每个模态独立保存：图片绑定不改变音频，也不能把仅支持图片输入的 Model 绑定为音频理解模型。
    assert body['data']['image']['model_id'] == vision, body
    assert body['data']['audio']['enabled'] is False and body['data']['audio']['model_id'] is None, body
    status, body = http_request('PUT', env['admin'] + '/api/v1/media-understanding/audio', {
        'enabled': True, 'model_id': vision, 'thinking_level': 'medium'}, env['auth'])
    assert status == 400 and body['code'] == 'MEDIA_UNDERSTANDING_MODEL_UNAVAILABLE', body
    status, body = http_request('GET', env['admin'] + '/api/v1/media-understanding', headers=env['auth'])
    assert status == 200 and body['data']['image']['enabled'] is True, body
    assert body['data']['audio']['eligible_models'] == [], body
    status, body = http_request('POST', env['admin'] + '/api/v1/api-keys', {
        'name': 'background-media-key', 'model_ids': [parent_route],
        'transparent_injection_enabled': True, 'inject_media_understanding': True,
        'mcp_access_enabled': True}, env['auth'])
    assert status == 200, body
    return body['data']['key']


def upload_background_image(env, key):
    image = base64.b64decode('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=')
    auth = {'Authorization': f'Bearer {key}'}
    status, upload = http_request('POST', env['proxy'] + '/v1/artifacts/uploads',
                                  {'mime_type': 'image/png', 'size': len(image)}, auth)
    assert status == 201, f'image upload creation returned HTTP {status}'
    connection = HTTPConnection('127.0.0.1', int(env['proxy'].rsplit(':', 1)[1]), timeout=20)
    try:
        connection.request('PUT', f"/v1/artifacts/uploads/{upload['upload_id']}/parts/1", image,
                           {**auth, 'x-upload-token': upload['upload_token']})
        response = connection.getresponse()
        assert response.status == 200
        part = json.loads(response.read())
    finally:
        connection.close()
    status, artifact = http_request('POST', env['proxy'] + f"/v1/artifacts/uploads/{upload['upload_id']}/complete",
                                    {'upload_token': upload['upload_token'], 'parts': [part]}, auth)
    assert status == 200 and artifact['mime_type'] == 'image/png', artifact
    return artifact['path']


@pytest.mark.parametrize('host,backend', HOST_DATABASES)
def test_storage_failure_preserves_published_background_media_task(
    host, backend, request, storage_runtime, tmp_path,
):
    stravia_binary = request.getfixturevalue('stravia_binary') if host == 'server' else None
    arm = tmp_path / 'arm-storage-fault'
    controls = {'STRAVIA_TEST_STORAGE_FAULT': 'artifact',
                'STRAVIA_TEST_STORAGE_FAULT_KIND': 'io-full',
                'STRAVIA_TEST_STORAGE_FAULT_ARM_FILE': str(arm)}
    with isolated_host(host, backend, stravia_binary, storage_runtime, tmp_path, controls) as (env, process, _log):
        with media_background_upstream() as (upstream, entered, release, parent_requests, media_requests):
            env['mock'] = f'http://127.0.0.1:{upstream.server_port}'
            route_id, _ = _create_route(env, 'background-media-parent', retry_budget=0)
            key = configure_background_media(env, route_id)
            auth = {'Authorization': f'Bearer {key}'}
            upstream.artifact_path = upload_background_image(env, key)
            status, upload = http_request('POST', env['proxy'] + '/v1/artifacts/uploads',
                                          {'mime_type': 'text/plain', 'size': 3}, auth)
            assert status == 201, f'fault upload creation returned HTTP {status}'
            # Public StraviaRead is the path facade. Its PNG artifact branch
            # delegates to the real configured Media Understanding provider.
            # HTTP 等待必须覆盖真实同步窗口，不能把产品交接期限缩短成测试专用值。
            status, published = http_request('POST', env['proxy'] + '/v1/responses', {
                'model': 'background-media-parent', 'input': [{'role': 'user', 'content': 'read the owned image'}],
                'tools': [{'type': 'function', 'name': 'StraviaRead', 'parameters': {
                    'type': 'object', 'properties': {'path': {'type': 'string', 'minLength': 1}},
                    'required': ['path'], 'additionalProperties': False}},
                    {'type': 'function', 'name': 'local_probe', 'parameters': {'type': 'object'}}]}, auth, timeout=90)
            assert status == 200 and 'background published' in json.dumps(published), published
            client_call = next(item for item in published['output']
                               if item.get('type') == 'function_call' and item.get('name') == 'local_probe')
            assert entered.wait(10), 'real background media task did not enter its provider'
            assert not release.is_set(), 'background task must still be pending after initial HTTP completion'
            arm.touch()
            trigger = HTTPConnection('127.0.0.1', int(env['proxy'].rsplit(':', 1)[1]), timeout=20)
            try:
                trigger.request('PUT', f"/v1/artifacts/uploads/{upload['upload_id']}/parts/1", b'abc',
                                {**auth, 'x-upload-token': upload['upload_token']})
                response = trigger.getresponse()
                response.read()
                assert response.status == 500
            finally:
                trigger.close()
            assert not arm.exists(), 'configured artifact fault was not consumed'
            assert process.poll() is None, 'unrelated upload failure terminated background host'
            # Marker 恢复本次实际交付的执行引用，不等后台完成，也不替换成最终报告。
            status, continued = http_request('POST', env['proxy'] + '/v1/responses', {
                'model': 'background-media-parent', 'previous_response_id': published['id'],
                'input': [{'type': 'function_call_output', 'call_id': client_call['call_id'], 'output': 'probe done'}]}, auth)
            assert status == 200 and 'background continuation completed' in json.dumps(continued), continued
            assert len(parent_requests) == 2
            tool_results = [message for message in parent_requests[-1]['messages']
                            if message['role'] == 'tool' and message.get('tool_call_id') == 'read-media']
            assert len(tool_results) == 1
            delivered = json.loads(tool_results[0]['content'])
            assert delivered['path'].startswith('stravia://tool-executions/'), delivered
            assert delivered['status'] == 'running', delivered
            assert not release.is_set(), 'Marker restoration waited for background completion'
            release.set()
            status, queried = http_request('POST', env['proxy'] + '/mcp', {
                'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call', 'params': {
                    'name': 'StraviaRead', 'arguments': {'path': delivered['path'] + '?wait=30'},
                    '_meta': {'io.modelcontextprotocol/protocolVersion': '2026-07-28',
                              'io.modelcontextprotocol/clientCapabilities': {}}}},
                {**auth, 'Accept': 'application/json, text/event-stream',
                 'MCP-Protocol-Version': '2026-07-28', 'Mcp-Method': 'tools/call',
                 'Mcp-Name': 'StraviaRead'}, timeout=45)
            assert status == 200, queried
            assert not queried['result']['isError'], queried
            completed = queried['result']['structuredContent']
            assert completed['status'] == 'completed', completed
            assert completed['result']['report']['answer'] == (
                f'A blue square from the real media provider. [{upstream.artifact_path}]')
            assert completed['result']['report']['artifacts'] == [{'path': upstream.artifact_path}]
            assert len(media_requests) == 1, 'storage failure retried or replayed background media'
            assert process.poll() is None
