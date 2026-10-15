#!/usr/bin/env python3
"""Real HTTP/process regression tests. Only synthetic loopback endpoints are used."""
import collections
import contextlib
import gzip
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import signal
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.parse
import urllib.request

BINARY = str(Path(sys.argv.pop(1)).resolve()) if len(sys.argv) > 1 and not sys.argv[1].startswith('-') else str(Path('target/debug/ufm-ib-exporter').resolve())
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


def get(url):
    try:
        with OPENER.open(url, timeout=3) as response:
            return response.status, response.read().decode()
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode()


def eventually(check, timeout=8):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = check()
            if last:
                return last
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.04)
    raise AssertionError('condition did not become true; last result: ' + repr(last)[:300])


class Fixture:
    def __init__(self, tls=None):
        self.requests = collections.Counter()
        self.modes = {}
        self.active = 0
        self.peak = 0
        self.latency = 0
        self.lock = threading.Lock()
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                parsed = urllib.parse.urlsplit(self.path)
                path = parsed.path
                with fixture.lock:
                    fixture.requests[self.path] += 1
                    mode = fixture.modes.get(path, 'ok')
                    if path.startswith('/900'):
                        fixture.active += 1
                        fixture.peak = max(fixture.peak, fixture.active)
                try:
                    if path.startswith('/900') and fixture.latency:
                        time.sleep(fixture.latency)
                    if mode == 'error':
                        self.send_error(503)
                        return
                    if mode == 'slow':
                        time.sleep(2.5)
                    stamp = int((time.time() - (10000 if mode == 'stale' else 1)) * 1000)
                    if path == '/rest':
                        body = json.dumps([
                            {'has_plane_ports': True, 'node_description': 'b300-1:card0', 'logical_state': 'Active', 'active_speed': '800G'},
                            {'has_plane_ports': False, 'node_description': 'B300-1:plane0', 'logical_state': 'Active'},
                        ]).encode()
                    elif path.startswith('/9001'):
                        shard = urllib.parse.parse_qs(parsed.query).get('shard', ['0'])[0]
                        body = f'# TYPE LinkDownedCounterExtended counter\nLinkDownedCounterExtended{{node_description="b300-01",port_guid="p{shard}"}} 4 {stamp}\n'.encode()
                    else:
                        body = ''.join(f'{name}{{Port_Name="b300-01",PN="fixture",SN="synthetic",Temperature="42"}} {value} {stamp}\n' for name, value in [('rx_power_lane0', 1.2), ('tx_power_lane0', 1.4), ('cable_temperature', 42)]).encode()
                    if mode == 'empty':
                        body = b''
                    elif mode == 'longline':
                        body = b'x' * 2048
                    elif mode == 'duplicate':
                        body += body
                    if mode == 'gzip':
                        body = gzip.compress(body)
                    self.send_response(200)
                    self.send_header('Content-Length', str(len(body)))
                    if mode == 'gzip':
                        self.send_header('Content-Encoding', 'gzip')
                    self.end_headers()
                    try:
                        # Deliberately cut UTF-8/text lines at arbitrary HTTP write boundaries.
                        for start in range(0, len(body), 23):
                            self.wfile.write(body[start:start + 23])
                    except (BrokenPipeError, ConnectionResetError):
                        pass
                finally:
                    if path.startswith('/900'):
                        with fixture.lock:
                            fixture.active -= 1

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        if tls:
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(*tls)
            self.server.socket = context.wrap_socket(self.server.socket, server_side=True)
        self.base = ('https://localhost:' if tls else 'http://127.0.0.1:') + str(self.server.server_port)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class Exporter:
    def __init__(self, fixture, modify=None):
        self.directory = tempfile.TemporaryDirectory(prefix='ufm-rust-test-')
        self.root = Path(self.directory.name)
        self.path = self.root / 'config.json'
        self.base = 'http://127.0.0.1:' + str(port())
        self.config = {
            'listen': self.base.removeprefix('http://'),
            'rest': {'url': fixture.base + '/rest', 'interval_seconds': 60, 'expected_nodes': 2},
            'telemetry_9001': {'enabled': True, 'url': fixture.base + '/9001', 'interval_seconds': 60},
            'telemetry_9002': {'enabled': True, 'url': fixture.base + '/9002', 'interval_seconds': 60},
        }
        if modify:
            modify(self.config)
        self.path.write_text(json.dumps(self.config))
        self.log = (self.root / 'stderr.log').open('w+')
        env = {k: v for k, v in os.environ.items() if not k.startswith('UFM_') and k not in ['LISTEN_PORT', 'LISTEN_ADDRESS']}
        env.update(UFM_PASSWORD='synthetic-test-password', NO_PROXY='127.0.0.1,localhost', no_proxy='127.0.0.1,localhost')
        self.env = env
        self.container = None
        command = [BINARY, '--config', str(self.path)]
        if image := os.environ.get('UFM_TEST_DOCKER_IMAGE'):
            self.container = f'ufm-rust-fixture-{os.getpid()}-{self.base.rsplit(":", 1)[-1]}'
            self.root.chmod(0o755)  # Synthetic fixture only; readable by container UID 65532.
            self.path.chmod(0o644)
            if source_ca := self.config.get('ca_file'):
                (self.root / 'ca.pem').write_bytes(Path(source_ca).read_bytes())
                (self.root / 'ca.pem').chmod(0o644)
                self.config['ca_file'] = '/config/ca.pem'
                self.path.write_text(json.dumps(self.config))
            command = ['docker', 'run', '--rm', '--name', self.container, '--network', 'host',
                       '--read-only', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
                       '--memory', '256m', '--pids-limit', '64',
                       '-e', 'UFM_CONFIG=/config/config.json', '-e', 'UFM_PASSWORD=synthetic-test-password',
                       '-e', 'NO_PROXY=127.0.0.1,localhost',
                       '-v', str(self.root) + ':/config:ro', image]
        self.process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=self.log, env=env)
        try:
            def started():
                if self.process.poll() is not None:
                    raise RuntimeError('exporter exited before liveness check')
                return get(self.base + '/healthz')[0] == 200
            eventually(started, timeout=30 if self.container else 8)
        except Exception as error:
            self.log.flush()
            self.log.seek(0)
            detail = self.log.read()[-1500:]
            self.close()
            raise RuntimeError('fixture startup failed: ' + detail) from error

    def metrics(self):
        status, text = get(self.base + '/metrics')
        assert status == 200
        return text

    def ready(self):
        eventually(lambda: get(self.base + '/readyz')[0] == 200)

    def reload(self):
        self.path.write_text(json.dumps(self.config))
        if self.container:
            subprocess.run(['docker', 'kill', '--signal', 'HUP', self.container], check=True, capture_output=True)
        else:
            self.process.send_signal(signal.SIGHUP)

    def check_liveness(self):
        command = ['docker', 'exec', self.container, '/usr/local/bin/ufm-ib-exporter', '--healthcheck'] if self.container else [BINARY, '--config', str(self.path), '--healthcheck']
        subprocess.run(command, env=self.env, check=True, capture_output=True, timeout=5)

    def close(self):
        if self.container:
            subprocess.run(['docker', 'stop', '--time', '3', self.container], capture_output=True, timeout=8)
        else:
            self.process.terminate()
        try:
            self.process.wait(timeout=4)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=4)
        self.log.close()
        self.directory.cleanup()


@contextlib.contextmanager
def running(modify=None, fixture_setup=None):
    fixture = Fixture()
    exporter = None
    try:
        if fixture_setup:
            fixture_setup(fixture)
        exporter = Exporter(fixture, modify)
        yield fixture, exporter
    finally:
        if exporter:
            exporter.close()
        fixture.close()


class EndToEnd(unittest.TestCase):
    @unittest.skipUnless(shutil.which('openssl'), 'openssl required for local synthetic TLS fixture')
    def test_tls_rejects_unknown_ca_and_wrong_hostname(self):
        with tempfile.TemporaryDirectory(prefix='ufm-synthetic-ca-') as temporary:
            root = Path(temporary)
            certificate, ca_key = root / 'ca.pem', root / 'ca-key.pem'
            server_certificate, key, csr = root / 'server.pem', root / 'server-key.pem', root / 'server.csr'
            subprocess.run(['openssl', 'req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1',
                            '-nodes', '-days', '1', '-subj', '/CN=ufm-synthetic-root',
                            '-addext', 'basicConstraints=critical,CA:TRUE', '-addext', 'keyUsage=critical,keyCertSign,cRLSign',
                            '-keyout', str(ca_key), '-out', str(certificate)], check=True, capture_output=True)
            subprocess.run(['openssl', 'req', '-new', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1',
                            '-nodes', '-subj', '/CN=localhost', '-keyout', str(key), '-out', str(csr)], check=True, capture_output=True)
            extensions = root / 'server-ext.cnf'
            extensions.write_text('basicConstraints=critical,CA:FALSE\nsubjectAltName=DNS:localhost\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n')
            subprocess.run(['openssl', 'x509', '-req', '-in', str(csr), '-CA', str(certificate), '-CAkey', str(ca_key),
                            '-CAcreateserial', '-days', '1', '-sha256', '-extfile', str(extensions),
                            '-out', str(server_certificate)], check=True, capture_output=True)
            fixture = Fixture(tls=(server_certificate, key))
            try:
                for trust, correct_hostname in [(False, True), (True, True), (True, False)]:
                    def modify(c):
                        c['telemetry_9001']['enabled'] = c['telemetry_9002']['enabled'] = False
                        if trust: c['ca_file'] = str(certificate)
                        if not correct_hostname: c['rest']['url'] = c['rest']['url'].replace('localhost', '127.0.0.1')
                    exporter = Exporter(fixture, modify)
                    try:
                        if trust and correct_hostname:
                            exporter.ready()
                            self.assertIn('ufm_ib_links_up 1', exporter.metrics())
                        else:
                            text = eventually(lambda: (s if 'source="rest",reason="network"' in (s := exporter.metrics()) else None))
                            self.assertNotIn('ufm_ib_links_up ', text)
                    finally:
                        exporter.close()
            finally:
                fixture.close()

    def test_three_sources_and_many_scrapes_do_not_repoll(self):
        with running() as (fixture, exporter):
            exporter.ready()
            text = exporter.metrics()
            self.assertIn('ufm_ib_links_up 1', text)
            self.assertIn('ufm_ib_node_links_up{node="B300-02"} 0', text)
            self.assertIn('LinkDownedCounterExtended{', text)
            self.assertIn('cable_temperature{', text)
            self.assertIn('telemetry_source="9002"', text)
            self.assertNotIn('Temperature="', text)
            before = dict(fixture.requests)
            exporter.check_liveness()
            for _ in range(20):
                exporter.metrics()
            self.assertEqual(before, dict(fixture.requests))

    def test_failed_empty_or_frozen_source_suppresses_cache_only_for_that_source(self):
        def modify(c):
            c['telemetry_9002']['interval_seconds'] = 1
        with running(modify) as (fixture, exporter):
            exporter.ready()
            for mode in ['error', 'empty', 'stale']:
                fixture.modes['/9002'] = mode
                text = eventually(lambda: (s if 'ufm_telemetry_up{source="9002"} 0' in (s := exporter.metrics()) else None))
                self.assertNotIn('cable_temperature{', text)
                self.assertIn('LinkDownedCounterExtended{', text)
                self.assertIn('ufm_ib_links_up 1', text)
                fixture.modes['/9002'] = 'ok'
                exporter.ready()

    def test_reload_is_atomic_and_bad_configuration_is_rejected(self):
        with running() as (_, exporter):
            exporter.ready()
            exporter.config['telemetry_9002']['keep'] = ['^cable_temperature$']
            exporter.reload()
            exporter.ready()
            text = eventually(lambda: (s if 'ufm_config_reload_successes_total 1' in (s := exporter.metrics()) else None))
            self.assertIn('cable_temperature{', text)
            self.assertNotIn('rx_power_lane0{', text)
            exporter.config['telemetry_9002']['keep'] = ['[bad']
            exporter.reload()
            text = eventually(lambda: (s if 'ufm_config_reload_failures_total 1' in (s := exporter.metrics()) else None))
            self.assertIn('cable_temperature{', text)

    def test_gzip_and_complete_sequential_shards(self):
        for permits in [1, 2]:
            with self.subTest(heavy_poll_permits=permits):
                def modify(c):
                    c['max_heavy_polls'] = permits
                    c['telemetry_9001']['urls'] = [c['telemetry_9001']['url'] + '?shard=0', c['telemetry_9001']['url'] + '?shard=1']
                def setup(fixture):
                    fixture.modes['/9002'] = 'gzip'
                    fixture.latency = 0.1
                # Configure before startup: cancellation during a reload does not
                # promise that the remote server has finished an abandoned request.
                with running(modify, setup) as (fixture, exporter):
                    exporter.ready()
                    text = exporter.metrics()
                    self.assertIn('port_guid="p0"', text)
                    self.assertIn('port_guid="p1"', text)
                    self.assertIn('cable_temperature{', text)
                    # Negative control: with two permits the fixture really sees
                    # two outstanding upstream requests, so this measures the guard.
                    self.assertEqual(fixture.peak, permits)

    def test_deadline_and_input_cache_line_budgets(self):
        with running() as (fixture, exporter):
            exporter.ready()
            cases = [
                ('slow', {'timeout_seconds': 1}, 'timeout'),
                ('ok', {'max_input_bytes': 16}, 'input_limit'),
                ('ok', {'max_cache_bytes': 16}, 'cache_limit'),
                ('longline', {'max_line_bytes': 128}, 'line_limit'),
            ]
            original = dict(exporter.config['telemetry_9002'])
            for mode, limits, reason in cases:
                fixture.modes['/9002'] = mode
                exporter.config['telemetry_9002'] = dict(original, **limits)
                exporter.reload()
                text = eventually(lambda: (s if f'source="9002",reason="{reason}"' in (s := exporter.metrics()) and 'ufm_ib_links_up 1' in s else None))
                self.assertNotIn('cable_temperature{', text)
                self.assertIn('ufm_ib_links_up 1', text)
            fixture.modes['/9002'] = 'ok'

    def test_old_native_observations_expire_between_polls(self):
        def modify(c):
            c['telemetry_9002']['max_observation_age_seconds'] = 2
        with running(modify) as (_, exporter):
            exporter.ready()
            text = eventually(lambda: (s if 'ufm_telemetry_up{source="9002"} 0' in (s := exporter.metrics()) else None), timeout=4)
            self.assertNotIn('cable_temperature{', text)
            self.assertIn('LinkDownedCounterExtended{', text)

    def test_overlapping_sources_cannot_publish_duplicate_metric_families(self):
        def modify(c):
            c['telemetry_9002']['url'] = c['telemetry_9001']['url']
            c['telemetry_9002']['keep'] = ['^LinkDownedCounterExtended$']
        with running(modify) as (_, exporter):
            text = eventually(lambda: (s if 'reason="source_overlap"' in (s := exporter.metrics()) else None))
            self.assertEqual(sum(line.startswith('LinkDownedCounterExtended{') for line in text.splitlines()), 1)
            self.assertIn('ufm_ib_links_up 1', text)

    @unittest.skipUnless(os.environ.get('PROMETHEUS_BINARY'), 'set PROMETHEUS_BINARY for real engine validation')
    def test_real_prometheus_accepts_exported_metrics(self):
        with running() as (_, exporter), tempfile.TemporaryDirectory(prefix='ufm-prom-test-') as temporary:
            exporter.ready()
            root = Path(temporary)
            pport = port()
            config = {'global': {'scrape_interval': '1s'}, 'scrape_configs': [{'job_name': 'ufm-rust-fixture', 'static_configs': [{'targets': [exporter.base.removeprefix('http://')]}]}]}
            (root / 'prometheus.json').write_text(json.dumps(config))
            with (root / 'prometheus.log').open('w+') as log:
                process = subprocess.Popen([os.environ['PROMETHEUS_BINARY'], '--config.file=' + str(root / 'prometheus.json'), '--storage.tsdb.path=' + str(root / 'data'), f'--web.listen-address=127.0.0.1:{pport}'], stdout=log, stderr=log)
                try:
                    def query(q):
                        return json.loads(get(f'http://127.0.0.1:{pport}/api/v1/query?' + urllib.parse.urlencode({'query': q}))[1])['data']['result']
                    eventually(lambda: query('up{job="ufm-rust-fixture"}') and query('up{job="ufm-rust-fixture"}')[0]['value'][1] == '1')
                    self.assertEqual(query('ufm_ib_links_up')[0]['value'][1], '1')
                    self.assertEqual(query('cable_temperature')[0]['value'][1], '42')
                    self.assertEqual(query('LinkDownedCounterExtended')[0]['value'][1], '4')
                finally:
                    process.terminate()
                    process.wait(timeout=5)


if __name__ == '__main__':
    unittest.main(verbosity=2)
