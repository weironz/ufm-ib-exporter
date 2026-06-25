#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
UFM IB Exporter —— 三源可独立启停的 InfiniBand 计算网监控 exporter。

三个数据源(各自 env 开关,源地址可传参 = 本地回环或指定 IP):
  1. REST  /ufmRest/resources/ports   → 链路 up/down + per-node 基线(权威、轻量、JSON)
  2. Telemetry :9001 (counters)        → 误码/错误计数(SymbolError/LinkDowned/PortRcvErrors/FEC… → 链路劣化)
  3. Telemetry :9002 (rich)            → 富指标(光功率/温度/cable…),体量大(1.26GB),默认关

设计:
  * REST 走「scrape 时现抓」(小 JSON,快);telemetry 走「后台线程拉 + 缓存」
    —— telemetry 端点单次 158MB(:9001)/1.26GB(:9002),绝不能放在 scrape 路径上。
  * telemetry 用「流式按行过滤透传」:iter_lines 边流边按【指标名白名单 + 可选 node 过滤】留行,
    保留原始 Prometheus 文本(含 # TYPE/时间戳),不解析成对象 → 省内存、保真。
  * 部署位置无关:放 UFM 上则 telemetry URL 用 127.0.0.1(loopback,UFM 默认就绑这);
    放 bastion 上则 telemetry 默认关(loopback 够不着)或填已暴露的 mgmt IP。
    REST 永远走 mgmt IP(https)。
  * 每个源独立 last-good / up / data_age / scrape_error,互不影响。

凭据/配置全走环境变量,详见 README / .env.example。
"""

import os
import re
import time
import logging
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

import requests
import urllib3
from requests.auth import HTTPBasicAuth
from prometheus_client import CollectorRegistry, generate_latest
from prometheus_client.core import GaugeMetricFamily

urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)  # UFM 自签证书

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("ufm-exporter")


def env_bool(k, default):
    return os.getenv(k, str(default)).strip().lower() in ("1", "true", "yes", "on")


# ============================ 配置 ============================
LISTEN_PORT = int(os.getenv("LISTEN_PORT", "9627"))

# ---- 源1:REST(up/down)----
COLLECT_REST   = env_bool("UFM_COLLECT_REST", True)
UFM_HOST       = os.getenv("UFM_HOST", "172.16.30.33")        # REST 走 https，这里填 mgmt IP
UFM_USER       = os.getenv("UFM_USERNAME", "admin")
UFM_PASS       = os.getenv("UFM_PASSWORD", "")
REST_TIMEOUT   = float(os.getenv("UFM_REST_TIMEOUT", "30"))
NODE_PREFIX    = os.getenv("UFM_NODE_PREFIX", "B300")
EXPECTED_NODES = int(os.getenv("UFM_EXPECTED_NODES", "32"))
PORTS_PER_NODE = int(os.getenv("UFM_PORTS_PER_NODE", "8"))
PORTS_PATH     = "/ufmRest/resources/ports"
_NODE_RE       = re.compile(re.escape(NODE_PREFIX) + r"-\d+")

# ---- 源2:Telemetry :9001(counters / 误码)----
# 默认保留 IB PM 错误计数 + PHY 误码/FEC(链路劣化所需);node_filter 默认空=全收(交换机两端都要)。
DEF_9001_KEEP = (r"^(SymbolErrorCounter|LinkErrorRecoveryCounter|LinkDownedCounter|PortRcvErrors|"
                 r"PortRcvRemotePhysicalErrors|PortRcvSwitchRelayErrors|PortXmitDiscards|"
                 r"LocalLinkIntegrityErrors|ExcessiveBufferOverrunErrors|PortXmitWait|"
                 r"phy_c_symbol_errors|phy_c_sync_headers_errors|phy_c_rs_fec_corrected_symbols_total)")

# ---- 源3:Telemetry :9002(rich / 光学)----  默认关(1.26GB)
DEF_9002_KEEP = DEF_9001_KEEP  # 真要 :9002 的光功率/温度时,自行扩白名单(grep 端点定名)


# ====================== 源1:REST collector ======================
def fetch_ports():
    url = f"https://{UFM_HOST}{PORTS_PATH}"
    r = requests.get(url, auth=HTTPBasicAuth(UFM_USER, UFM_PASS), verify=False, timeout=REST_TIMEOUT)
    r.raise_for_status()
    return r.json()


def parse_host_ports(ports):
    """挑 B300 主机物理 CX8 口(每节点 8 块)。坑见 README:
    1) 一块 800G CX8 = 4 plane,只数物理聚合口 has_plane_ports==True(否则 ×4)。
    2) node_description 两种写法(带冒号已纳管 / 'B300-N mlx5_X' 调试中),正则取节点名都认。"""
    out = []
    for p in ports:
        if p.get("has_plane_ports") is not True:
            continue
        nd = p.get("node_description") or ""
        m = _NODE_RE.match(nd)
        if not m:
            continue
        node = m.group(0)
        card = nd[len(node):].lstrip(": ").strip() or "?"
        out.append((node, card, str(p.get("peer_node_name") or ""),
                    str(p.get("peer_port_dname") or ""), str(p.get("active_speed") or ""),
                    1 if (p.get("logical_state") == "Active") else 0))
    return out


_REST_LG = None
_REST_LOCK = threading.Lock()


class UFMRestCollector:
    """scrape 时现抓 REST(小 JSON)。失败回退 last-good。"""
    def collect(self):
        global _REST_LG
        t0 = time.time(); ok = True
        try:
            rows = parse_host_ports(fetch_ports())
            with _REST_LOCK:
                _REST_LG = (rows, time.time())
        except Exception as e:
            ok = False; log.warning("REST 抓取失败: %s", e)
            with _REST_LOCK:
                rows = _REST_LG[0] if _REST_LG else []
        with _REST_LOCK:
            last_ts = _REST_LG[1] if _REST_LG else t0
        yield self._g("ufm_up", "UFM REST 可达且解析成功=1", 1 if ok else 0)
        yield self._g("ufm_scrape_error", "本次 REST 抓取失败=1", 0 if ok else 1)
        yield self._g("ufm_scrape_duration_seconds", "REST 抓取耗时(秒)", round(time.time() - t0, 3))
        yield self._g("ufm_data_age_seconds", "链路数据年龄(秒,0=新鲜)", 0.0 if ok else round(time.time() - last_ts, 1))

        link = GaugeMetricFamily("ufm_ib_link_up", "CX8 IB 链路 up=1/down=0(UFM 权威)",
                                 labels=["node", "card", "peer", "cage", "speed"])
        node_up, node_tot = {}, {}
        for (node, card, peer, cage, speed, up) in rows:
            link.add_metric([node, card, peer, cage, speed], up)
            node_up[node] = node_up.get(node, 0) + up
            node_tot[node] = node_tot.get(node, 0) + 1
        yield link

        expected = [f"{NODE_PREFIX}-{i}" for i in range(1, EXPECTED_NODES + 1)]
        nu = GaugeMetricFamily("ufm_ib_node_links_up", f"每节点 CX8 up 数(基线{PORTS_PER_NODE})", labels=["node"])
        nt = GaugeMetricFamily("ufm_ib_node_links_total", "每节点 UFM 可见 CX8 端口数", labels=["node"])
        for node in expected:
            nu.add_metric([node], node_up.get(node, 0))
            nt.add_metric([node], node_tot.get(node, 0))
        yield nu; yield nt

        upc = sum(node_up.values()); exp_total = EXPECTED_NODES * PORTS_PER_NODE
        yield self._g("ufm_ib_links_expected", "全网应在线 CX8 链路数", exp_total)
        yield self._g("ufm_ib_links_up", "全网 CX8 up 总数", upc)
        yield self._g("ufm_ib_links_missing", "全网未 up 链路数(down/消失/整节点缺失)", exp_total - upc)

    def _g(self, name, doc, value):
        g = GaugeMetricFamily(name, doc, labels=[]); g.add_metric([], value); return g


# ================= 源2/3:Telemetry 流式过滤透传 =================
class TelemetrySource:
    """一个 telemetry 源(:9001/:9002):后台线程拉文本 → 按【指标名白名单 + 可选 node 过滤】
    流式留行 → 缓存过滤后的原始字节。scrape 时只读缓存,从不触发拉取。"""

    def __init__(self, name, url, interval, keep_regex, node_filter):
        self.name = name
        self.url = url
        self.interval = max(30, int(interval))
        self.keep = re.compile(keep_regex.encode("ascii")) if keep_regex else None
        self.nodef = re.compile(node_filter.encode("utf-8")) if node_filter else None
        self.lock = threading.Lock()
        self.cached = b""      # 过滤后的原始指标行(last-good)
        self.up = 0
        self.err = 0
        self.ts = 0.0          # 上次成功时刻
        self.series = 0
        self.dur = 0.0
        threading.Thread(target=self._loop, name=f"tele-{name}", daemon=True).start()
        log.info("telemetry[%s] 启用: url=%s interval=%ss", name, url, self.interval)

    def _loop(self):
        while True:
            self._poll()
            time.sleep(self.interval)

    def _name_ok(self, name_b):
        return self.keep is None or self.keep.match(name_b) is not None

    def _node_ok(self, line):
        if self.nodef is None:
            return True
        m = re.search(rb'node_description="([^"]*)"', line)
        return bool(m and self.nodef.search(m.group(1)))

    @staticmethod
    def _strip_ts(line):
        # 去掉行尾显式毫秒时间戳(`name{labels} value [ts]` → `name{labels} value`),
        # 让 Prometheus 用 scrape 时间,避免缓存数据带旧时间戳触发 too-old / 重复点。
        # 在 `}` 之后切分(label 值可能含空格,不能直接 split 整行)。
        b = line.rfind(b"}")
        if b >= 0:
            tail = line[b + 1:].split()
            return line[:b + 1] + b" " + tail[0] if len(tail) >= 2 else line
        parts = line.split()
        return parts[0] + b" " + parts[1] if len(parts) >= 3 else line

    def _poll(self):
        t0 = time.time()
        try:
            # 流式拉取:iter_lines 边读边过滤,不把 158MB/1.26GB 整块进内存。
            r = requests.get(self.url, stream=True, timeout=(10, max(self.interval, 120)))
            r.raise_for_status()
            kept = []
            n = 0
            for line in r.iter_lines(decode_unicode=False):
                if not line:
                    continue
                if line[:1] == b"#":                       # # HELP X / # TYPE X
                    parts = line.split(b" ", 3)
                    if len(parts) >= 3 and self._name_ok(parts[2]):
                        kept.append(line)
                    continue
                name = line.split(b"{", 1)[0].split(b" ", 1)[0]   # 指标名
                if not self._name_ok(name):
                    continue
                if not self._node_ok(line):
                    continue
                kept.append(self._strip_ts(line))
                n += 1
            r.close()
            blob = b"\n".join(kept) + (b"\n" if kept else b"")
            with self.lock:
                self.cached = blob; self.up = 1; self.err = 0
                self.ts = time.time(); self.series = n; self.dur = round(time.time() - t0, 2)
            log.info("telemetry[%s] 拉取成功: 留 %d series, %.1fs", self.name, n, self.dur)
        except Exception as e:
            log.warning("telemetry[%s] 拉取失败(保留 last-good): %s", self.name, e)
            with self.lock:
                self.up = 0; self.err = 1; self.dur = round(time.time() - t0, 2)
            # cached 保留上次成功值(last-good),不清空

    def output(self):
        """返回 元指标行 + 过滤后的 telemetry 行(均为原始字节,无 # TYPE,由 handler 统一加)。"""
        with self.lock:
            age = round(time.time() - self.ts, 1) if self.ts else -1
            meta = (
                f'ufm_telemetry_up{{source="{self.name}"}} {self.up}\n'
                f'ufm_telemetry_scrape_error{{source="{self.name}"}} {self.err}\n'
                f'ufm_telemetry_data_age_seconds{{source="{self.name}"}} {age}\n'
                f'ufm_telemetry_series{{source="{self.name}"}} {self.series}\n'
                f'ufm_telemetry_poll_seconds{{source="{self.name}"}} {self.dur}\n'
            ).encode("ascii")
            return meta + self.cached


def build_telemetry_sources():
    srcs = []
    if env_bool("UFM_COLLECT_9001", False):
        srcs.append(TelemetrySource(
            "9001",
            os.getenv("UFM_9001_URL", "http://127.0.0.1:9001/metrics"),
            os.getenv("UFM_9001_INTERVAL", "180"),
            os.getenv("UFM_9001_KEEP", DEF_9001_KEEP),
            os.getenv("UFM_9001_NODE_FILTER", "")))
    if env_bool("UFM_COLLECT_9002", False):
        srcs.append(TelemetrySource(
            "9002",
            os.getenv("UFM_9002_URL", "http://127.0.0.1:9002/metrics"),
            os.getenv("UFM_9002_INTERVAL", "300"),
            os.getenv("UFM_9002_KEEP", DEF_9002_KEEP),
            os.getenv("UFM_9002_NODE_FILTER", "")))
    return srcs


TELES = []
_META_TYPES = (
    b"# TYPE ufm_telemetry_up gauge\n"
    b"# TYPE ufm_telemetry_scrape_error gauge\n"
    b"# TYPE ufm_telemetry_data_age_seconds gauge\n"
    b"# TYPE ufm_telemetry_series gauge\n"
    b"# TYPE ufm_telemetry_poll_seconds gauge\n"
)


# ============================ HTTP ============================
class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        u = urlparse(self.path)
        if u.path == "/metrics":
            try:
                out = bytearray()
                if COLLECT_REST:
                    reg = CollectorRegistry(); reg.register(UFMRestCollector())
                    out += generate_latest(reg)
                if TELES:
                    out += _META_TYPES                 # 元指标 # TYPE 只出一次
                    for t in TELES:
                        out += t.output()
            except Exception as e:
                log.exception("collect failed")
                self.send_error(500, f"collect error: {e}")
                return
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
            self.end_headers()
            try:
                self.wfile.write(bytes(out))
            except (BrokenPipeError, ConnectionResetError):
                pass
        elif u.path in ("/healthz", "/health"):
            self.send_response(200); self.end_headers(); self.wfile.write(b"ok\n")
        elif u.path == "/":
            self.send_response(200); self.end_headers()
            self.wfile.write(b"UFM IB Exporter\nGET /metrics\n")
        else:
            self.send_error(404)


def main():
    global TELES
    if COLLECT_REST and not UFM_PASS:
        log.warning("UFM_PASSWORD 未设置(REST 启用中)")
    TELES = build_telemetry_sources()
    log.info("UFM IB Exporter :%d  REST=%s  telemetry=%s",
             LISTEN_PORT, COLLECT_REST, [t.name for t in TELES] or "(none)")
    ThreadingHTTPServer(("0.0.0.0", LISTEN_PORT), Handler).serve_forever()


if __name__ == "__main__":
    main()
