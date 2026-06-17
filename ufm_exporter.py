#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
UFM IB Exporter —— 通过 UFM REST 监控 InfiniBand 计算网 ConnectX-8(XDR 800G)链路 up/down。

设计要点:
  * 数据源 = UFM(InfiniBand Subnet Manager)REST `/ufmRest/resources/ports`,一次调用拿全
    Fabric 端口状态。不走 BMC——BMC sideband 读 IB 链路是假阴性(主机 ibstat 显示 ACTIVE,
    BMC 却报 LinkDown),UFM 才权威。
  * 不直接抓 UFM telemetry 原生 /metrics:那端点单次 158MB(:9001)/1.26GB(:9002)的火管,
    Prometheus 抓它会被打爆(metric_relabel 是先全下载+解析再 drop,省存储但不省传输/解析)。
    REST /resources/ports 是小 JSON,正好够 up/down。
  * 掉线口在 UFM 里会从列表「消失」(非显示 Down),整节点也可能消失 → 对所有应在册节点
    强制发指标、缺席补 0,用 ufm_ib_node_links_up<基线 一条阈值覆盖三种异常。

凭据: 环境变量 UFM_HOST / UFM_USERNAME / UFM_PASSWORD。详见 README。
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
from prometheus_client import CollectorRegistry, generate_latest, CONTENT_TYPE_LATEST
from prometheus_client.core import GaugeMetricFamily

urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)  # UFM 自签证书

# ----------------------------- 配置 -----------------------------
LISTEN_PORT = int(os.getenv("LISTEN_PORT", "9627"))
UFM_HOST    = os.getenv("UFM_HOST", "172.16.30.33")
UFM_USER    = os.getenv("UFM_USERNAME", "admin")
UFM_PASS    = os.getenv("UFM_PASSWORD", "")
TIMEOUT     = float(os.getenv("TIMEOUT", "30"))
# 只采该前缀的主机 CA 端口(B300 计算节点);交换机端口 node_description 是 Q3400-*,自动排除。
NODE_PREFIX = os.getenv("UFM_NODE_PREFIX", "B300")
# 应在册节点数(B300-1..N)与每节点 CX8 基线 —— 用于「整节点从 Fabric 消失」检测:
# down/掉线的口在 UFM 里是直接「消失」而非显示 Down,故必须对所有应在册节点强制发指标(缺席补0)。
EXPECTED_NODES = int(os.getenv("UFM_EXPECTED_NODES", "32"))
PORTS_PER_NODE = int(os.getenv("UFM_PORTS_PER_NODE", "8"))
PORTS_PATH  = "/ufmRest/resources/ports"
_NODE_RE = re.compile(re.escape(NODE_PREFIX) + r"-\d+")

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("ufm-exporter")

# last-good:抓取失败时回退上次链路快照,保证大盘/告警不因 UFM 偶发抖动而空白或误报。
LAST_GOOD = None           # (rows, epoch)
LG_LOCK = threading.Lock()


def fetch_ports():
    """GET UFM 全 Fabric 端口列表(一次调用)。"""
    url = f"https://{UFM_HOST}{PORTS_PATH}"
    r = requests.get(url, auth=HTTPBasicAuth(UFM_USER, UFM_PASS), verify=False, timeout=TIMEOUT)
    r.raise_for_status()
    return r.json()


def parse_host_ports(ports):
    """挑出 B300 主机的物理 CX8 端口(每节点 8 块,800G OSFP),返回 [(node, card, peer, cage, speed, up)]。

    UFM /resources/ports 的两个坑(都踩过):
      1) 一块 800G CX8 = 4 个 plane(4×200G XDR),该接口把 plane 子口和物理聚合口都列出来 →
         只数物理聚合口 `has_plane_ports == True`,否则 plane 让每节点翻 4 倍。
      2) node_description 有两种写法:'B300-10:ibB…'(已纳管,带 peer/cage)和
         'B300-10 mlx5_8'(对端交换机调试中,peer=N/A)—— 用正则取节点名,两种都认。
         (旧实现只认带冒号的,把 mlx5_X 那种全漏了,导致每节点只数到一半。)

    注:UFM 能看到的物理口实测都是 Active(up==总数);某节点 <8 多半是该口对端(如 CL-02)
    还在调试、UFM 尚未完全发现,并非真 down。
    """
    out = []
    for p in ports:
        if p.get("has_plane_ports") is not True:      # 只要物理聚合口,排除 plane 子口
            continue
        nd = p.get("node_description") or ""
        m = _NODE_RE.match(nd)
        if not m:
            continue
        node = m.group(0)
        card = nd[len(node):].lstrip(": ").strip() or "?"
        peer  = str(p.get("peer_node_name") or "")
        cage  = str(p.get("peer_port_dname") or "")
        speed = str(p.get("active_speed") or "")
        up = 1 if (p.get("logical_state") == "Active") else 0
        out.append((node, card, peer, cage, speed, up))
    return out


class UFMCollector:
    def collect(self):
        t0 = time.time()
        ok = True
        try:
            rows = parse_host_ports(fetch_ports())
            with LG_LOCK:
                global LAST_GOOD
                LAST_GOOD = (rows, time.time())
        except Exception as e:
            ok = False
            log.warning("UFM 抓取失败: %s", e)
            with LG_LOCK:
                rows = LAST_GOOD[0] if LAST_GOOD else []

        # ---- 元指标(始终反映本次真实状态)----
        with LG_LOCK:
            last_ts = LAST_GOOD[1] if LAST_GOOD else t0
        age = 0.0 if ok else round(time.time() - last_ts, 1)
        yield self._g("ufm_up", "UFM REST 可达且解析成功=1,失败=0", 1 if ok else 0)
        yield self._g("ufm_scrape_error", "本次抓取失败=1", 0 if ok else 1)
        yield self._g("ufm_scrape_duration_seconds", "本次抓取耗时(秒)", round(time.time() - t0, 3))
        yield self._g("ufm_data_age_seconds", "所展示链路数据年龄(秒,0=本次新鲜)", age)

        # ---- 逐口链路 up/down(权威来自 UFM SM)----
        link = GaugeMetricFamily(
            "ufm_ib_link_up", "CX8 IB 链路 up=1/down=0(UFM 权威)",
            labels=["node", "card", "peer", "cage", "speed"])
        node_up, node_tot = {}, {}
        for (node, card, peer, cage, speed, up) in rows:
            link.add_metric([node, card, peer, cage, speed], up)
            node_up[node]  = node_up.get(node, 0) + up
            node_tot[node] = node_tot.get(node, 0) + 1
        yield link

        # ---- per-node 聚合 ----
        # 对「所有应在册节点」(B300-1..N)都强制发一条,缺席的节点补 0;否则整节点从 Fabric
        # 消失时连 series 都没有,node_links_up<8 会漏掉它。这样一条阈值同时覆盖三种:
        # 端口 down、端口消失、整节点消失。
        expected = [f"{NODE_PREFIX}-{i}" for i in range(1, EXPECTED_NODES + 1)]
        nu = GaugeMetricFamily("ufm_ib_node_links_up", f"每节点 CX8 链路 up 数(基线{PORTS_PER_NODE})", labels=["node"])
        nt = GaugeMetricFamily("ufm_ib_node_links_total", "每节点 UFM 可见 CX8 端口数", labels=["node"])
        for node in expected:
            nu.add_metric([node], node_up.get(node, 0))
            nt.add_metric([node], node_tot.get(node, 0))
        yield nu
        yield nt

        # ---- 全网聚合(以「应在册」为基线;missing 才能反映消失的口/整节点)----
        expected_total = EXPECTED_NODES * PORTS_PER_NODE
        upc = sum(node_up.values())
        yield self._g("ufm_ib_links_expected", "全网应在线 CX8 链路数(节点数×每节点基线)", expected_total)
        yield self._g("ufm_ib_links_up", "全网 CX8 链路 up 总数", upc)
        yield self._g("ufm_ib_links_missing", "全网未 up 的 CX8 链路数(down/消失/整节点缺失)", expected_total - upc)

    def _g(self, name, doc, value):
        g = GaugeMetricFamily(name, doc, labels=[])
        g.add_metric([], value)
        return g


# ----------------------- HTTP 服务 -----------------------
class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        u = urlparse(self.path)
        if u.path == "/metrics":
            try:
                reg = CollectorRegistry()
                reg.register(UFMCollector())
                output = generate_latest(reg)
            except Exception as e:
                log.exception("collect failed")
                self.send_error(500, f"collect error: {e}")
                return
            self.send_response(200)
            self.send_header("Content-Type", CONTENT_TYPE_LATEST)
            self.end_headers()
            try:
                self.wfile.write(output)
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
    if not UFM_PASS:
        log.warning("UFM_PASSWORD 未设置")
    log.info("UFM IB Exporter :%d (ufm=%s user=%s prefix=%s)",
             LISTEN_PORT, UFM_HOST, UFM_USER, NODE_PREFIX)
    ThreadingHTTPServer(("0.0.0.0", LISTEN_PORT), Handler).serve_forever()


if __name__ == "__main__":
    main()
