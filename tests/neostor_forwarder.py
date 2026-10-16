"""Real vmagent -> Prometheus receipt: 9001/9002 identity survives source scoping."""
import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import unittest
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('forwarder', ROOT / 'deploy/neostor/render.py')
renderer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(renderer)


def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


class ForwarderTest(unittest.TestCase):
    def test_source_diagnostics_are_distinct_after_real_remote_write(self):
        prom = os.environ['PROMETHEUS_BINARY']
        vmagent = os.environ['VMAGENT_BINARY']
        class Source(BaseHTTPRequestHandler):
            def do_GET(self):
                # Larger than vmagent's default 16 MiB: small fixtures would hide
                # the production cache-size regression even with healthy write ACKs.
                rich = ''.join('ufm_forward_fixture{index="'+str(i)+'",'+','.join('p%02d="native-metadata-value"'%n for n in range(24))+'} 1\n' for i in range(4000)).encode()
                body = rich + b'ufm_telemetry_up{source="9001"} 1\nufm_telemetry_up{source="9002"} 0\n' + (b'#' + b'x' * 1022 + b'\n') * 17408
                self.send_response(200)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            def log_message(self, *args):
                pass
        source = ThreadingHTTPServer(('127.0.0.1', 0), Source)
        threading.Thread(target=source.serve_forever, daemon=True).start()
        children = []
        with tempfile.TemporaryDirectory(prefix='ufm-forwarder-') as tmp:
            root = Path(tmp)
            prom_port, agent_port = port(), port()
            config = renderer.render('127.0.0.1:9627', '127.0.0.1:9628', 'fixture-idc')
            # Only synthetic inputs use fast polling and ephemeral ports.
            config['scrape_configs'] = [config['scrape_configs'][1]]
            job = config['scrape_configs'][0]
            job.update(scrape_interval='1s', scrape_timeout='1s')
            job['static_configs'][0]['targets'] = [f'127.0.0.1:{source.server_port}']
            (root / 'scrape.json').write_text(json.dumps(config))
            (root / 'prom.json').write_text('{"scrape_configs":[]}')
            try:
                with (root / 'process.log').open('w') as log:
                    # The queue must also recover after initially unavailable receipt.
                    children.append(subprocess.Popen([vmagent,
                        f'-httpListenAddr=127.0.0.1:{agent_port}', f'-promscrape.config={root}/scrape.json',
                        f'-remoteWrite.url=http://127.0.0.1:{prom_port}/api/v1/write',
                        '-remoteWrite.forcePromProto=true', '-remoteWrite.queues=1',
                        '-remoteWrite.maxRowsPerBlock=15000', '-remoteWrite.maxBlockSize=8388608', '-remoteWrite.flushInterval=5s',
                        f'-remoteWrite.tmpDataPath={root}/queue', '-remoteWrite.maxDiskUsagePerURL=64MB'], stdout=log, stderr=log))
                    time.sleep(2)
                    children.append(subprocess.Popen([prom,
                        f'--web.listen-address=127.0.0.1:{prom_port}', '--web.enable-remote-write-receiver',
                        f'--config.file={root}/prom.json', f'--storage.tsdb.path={root}/tsdb'], stdout=log, stderr=log))
                    http = urllib.request.build_opener(urllib.request.ProxyHandler({}))
                    deadline = time.monotonic() + 35
                    result = []
                    while time.monotonic() < deadline:
                        self.assertTrue(all(p.poll() is None for p in children), (root / 'process.log').read_text()[-4000:])
                        try:
                            with http.open(f'http://127.0.0.1:{prom_port}/api/v1/query?' + urllib.parse.urlencode({'query': 'ufm_telemetry_up'}), timeout=2) as r:
                                result = json.load(r)['data']['result']
                            if len(result) == 2:
                                break
                        except OSError:
                            pass
                        time.sleep(0.2)
                    self.assertEqual({r['metric']['ufm_source']: r['value'][1] for r in result}, {'9001': '1', '9002': '0'})
                    self.assertTrue(all('source' not in r['metric'] and 'device_id' not in r['metric'] for r in result))
                    with http.open(f'http://127.0.0.1:{agent_port}/metrics', timeout=3) as r:
                        metrics = r.read().decode()
                    import re
                    buckets = re.findall(r'vmagent_remotewrite_block_size_rows_bucket\{vmrange="([^.]*(?:\.[0-9]+)?e[+-][0-9]+)\.\.\.[^"]+"\} ([0-9]+)', metrics)
                    self.assertTrue(any(float(low) >= 3000 and int(count) > 0 for low, count in buckets),
                                    'Rich native labels must not force tiny WAN requests')

            finally:
                for process in reversed(children):
                    process.terminate()
                    process.wait(timeout=10)
                source.shutdown()
                source.server_close()


if __name__ == '__main__':
    unittest.main()
