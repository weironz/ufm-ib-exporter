#!/usr/bin/env python3
"""Synthetic streaming HTTP benchmark; no production endpoints or data files."""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', default='target/release/ufm-ib-exporter')
    parser.add_argument('--input-mib', type=int, default=512)
    parser.add_argument('--ports', type=int, default=4096)
    parser.add_argument('--padding', type=int, default=600)
    parser.add_argument('--legacy-python', help='optional Python interpreter with legacy requirements installed')
    args = parser.parse_args()
    assert args.input_mib > 0 and 1 <= args.ports <= 30000 and 0 <= args.padding <= 10000
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    metrics = [f'{direction}_power_lane{i}' for direction in ['rx', 'tx'] for i in range(8)] + ['cable_temperature']
    discarded = b'unused_counter{padding="' + b'x' * 950 + b'"} 0\n'
    unit = args.input_mib * 1024 * 1024 // args.ports
    padding = 'x' * args.padding
    counts = [0]

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *unused):
            pass

        def do_GET(self):
            counts[0] += 1
            stamp = int(time.time() * 1000)
            self.send_response(200)
            self.send_header('Content-Length', str(unit * args.ports))
            self.end_headers()
            try:
                for index in range(args.ports):
                    selected = ''.join(f'{metric}{{Port_Name="B300-{index:06}",PN="synthetic",SN="{index:06}",padding="{padding}"}} 1.0 {stamp}\n' for metric in metrics).encode()
                    assert len(selected) + 1 < unit, 'Increase input size or decrease retained data'
                    self.wfile.write(selected)
                    remaining = unit - len(selected)
                    # Repeated rejected lines are intentionally not retained in memory.
                    batch = discarded * min(64, remaining // len(discarded))
                    while batch and remaining >= len(batch):
                        self.wfile.write(batch)
                        remaining -= len(batch)
                    while remaining >= len(discarded):
                        self.wfile.write(discarded)
                        remaining -= len(discarded)
                    if remaining:
                        self.wfile.write(b'#' + b'x' * (remaining - 2) + b'\n' if remaining > 1 else b'\n')
            except (BrokenPipeError, ConnectionResetError):
                pass

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    upstream = f'http://127.0.0.1:{server.server_port}/metrics/CableInfo'
    results = []
    try:
        implementations = [('rust', [str(Path(args.binary).resolve())])]
        if args.legacy_python:
            implementations.append(('python-legacy', [args.legacy_python, str(Path('ufm_exporter.py').resolve())]))
        for name, command in implementations:
            with tempfile.TemporaryDirectory(prefix='ufm-bench-') as temporary:
                root = Path(temporary)
                with socket.socket() as sock:
                    sock.bind(('127.0.0.1', 0)); listen_port = sock.getsockname()[1]
                config = {'listen': f'127.0.0.1:{listen_port}', 'rest': {'enabled': False},
                          'telemetry_9001': {'enabled': False},
                          'telemetry_9002': {'enabled': True, 'url': upstream, 'interval_seconds': 3600,
                                             'timeout_seconds': 300, 'max_input_bytes': args.input_mib * 1024 * 1024 + 1}}
                (root / 'config.json').write_text(json.dumps(config))
                env = {k: v for k, v in os.environ.items() if not k.startswith('UFM_') and k not in ['LISTEN_ADDRESS', 'LISTEN_PORT']}
                env.update(NO_PROXY='127.0.0.1,localhost', no_proxy='127.0.0.1,localhost')
                if name == 'rust':
                    command += ['--config', str(root / 'config.json')]
                else:
                    env.update(UFM_COLLECT_REST='false', UFM_COLLECT_9001='false', UFM_COLLECT_9002='true',
                               UFM_9002_URL=upstream, UFM_9002_INTERVAL='3600',
                               UFM_9002_KEEP='^(rx_power_lane[0-7]|tx_power_lane[0-7]|cable_temperature)$', LISTEN_PORT=str(listen_port))
                before = counts[0]
                with (root / 'stderr.log').open('w+') as log:
                    start = time.monotonic()
                    process = subprocess.Popen(command, env=env, stdout=subprocess.DEVNULL, stderr=log)
                    try:
                        deadline = start + 310
                        while time.monotonic() < deadline:
                            if process.poll() is not None:
                                raise RuntimeError(name + ' terminated')
                            try:
                                with opener.open(f'http://127.0.0.1:{listen_port}/metrics', timeout=5) as response:
                                    data = response.read()
                                if b'ufm_telemetry_up{source="9002"} 1' in data:
                                    break
                            except (urllib.error.URLError, TimeoutError):
                                pass
                            time.sleep(0.05)
                        else:
                            raise RuntimeError(name + ' benchmark timed out')
                        elapsed = time.monotonic() - start
                        status = Path(f'/proc/{process.pid}/status').read_text()
                        peak_kib = int(re.search(r'VmHWM:\s+(\d+)', status)[1])
                        stat = Path(f'/proc/{process.pid}/stat').read_text().split()
                        cpu = (int(stat[13]) + int(stat[14])) / os.sysconf('SC_CLK_TCK')
                        count = int(re.search(rb'ufm_telemetry_series\{source="9002"\} ([0-9.]+)', data)[1].split(b'.')[0])
                        assert count == args.ports * 17, (name, count)
                        assert counts[0] - before == 1
                        results.append(dict(implementation=name, input_bytes=unit * args.ports,
                                            series=count, exported_bytes=len(data), end_to_end_seconds=round(elapsed, 3),
                                            process_cpu_seconds=cpu, peak_rss_mib=round(peak_kib / 1024, 2), upstream_requests=1))
                    finally:
                        process.terminate()
                        try: process.wait(timeout=5)
                        except subprocess.TimeoutExpired: process.kill(); process.wait(timeout=5)
        print(json.dumps({'synthetic_only': True, 'includes_http_generation_and_one_scrape': True, 'results': results}, indent=2))
    finally:
        server.shutdown()
        server.server_close()

if __name__ == '__main__':
    main()
