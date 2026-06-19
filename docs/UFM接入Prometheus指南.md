# UFM 对接 Prometheus 完整指南

把 NVIDIA **UFM**(InfiniBand Subnet Manager)的 IB 链路状态接入 Prometheus + Grafana。
涵盖:为什么、可选方案对比、**当前选定方案**、详细配置步骤、踩坑经验。

---

## 1. 背景:为什么必须用 UFM

监控 ConnectX-8(XDR 800G)计算网链路 up/down,有三个可能的数据源,但只有 UFM 权威:

- **BMC 带外(Redfish)读 IB 链路 ❌ 不可信**:实测 BMC 经 NC-SI/MCTP sideband 会把主机侧
  **ACTIVE 800G** 的口误报成 `Disabled/LinkDown`(假阴性),且各节点端口数不一致。仅可用于
  "CX8 卡在不在 + Status.Health",**不能**判 up/down。
- **带内 `ibstat`** ✅ 准,但❌ 客户重装系统就没了,做不了常态监控底座。
- **UFM** ✅ 它是这套 fabric 的 SM,链路状态由它说了算;带外、不随重装消失。**→ 选 UFM。**

UFM REST 访问:`https://172.16.30.33/ufmRest/...`,Basic Auth **`admin / 123456`**(注意不是 BMC 那套)。

---

## 2. 可选方案对比

### 2.1 数据源层面

| 方案 | 数据源 | 给什么 | 体量 | 是否需 exporter | 跑在哪 |
|---|---|---|---|---|---|
| **A. REST(选定)** | `/ufmRest/resources/ports` | 每口 up/down | 小 JSON | ✅ 需(JSON→Prom) | 任意位置 |
| B. Telemetry 原生 | `:9001`/`:9002` `/metrics` | up/down + 计数器/BER | **火管 158MB / 1.26GB** | ❌ 本身就是 Prom 格式 | 几乎必须 UFM 本机 |
| C. UFM 自带告警 | 事件/SNMP trap/syslog | 故障事件推送 | 小 | ❌ | UFM 自身 |

**关键认知**:
- **Prometheus 只会读 Prometheus 文本格式,不会解析 JSON** → REST(JSON)**必须**有 exporter 转换;
  Telemetry 本身是 Prom 格式所以"不用 exporter",但它是火管。
- **掉线口在 UFM 里会从列表"消失"**(而非显示 `Down`),整节点也可能消失 → 逐口 `==0` 抓不到,
  得靠"每节点 up 数 < 基线"兜底。

### 2.2 如果走 Telemetry 火管(方案 B),怎么驯服

`:9001`=158MB(计数器),`:9002`=1.26GB(富数据:BER/FEC/光功率/link_partner)。**直接让 Prometheus 抓不可行**:

- **慢拉解决不了**:单次 scrape 是原子的,必须在 `scrape_timeout` 内把整 158MB 下完+解析完;
  且基数(series 数)只跟数据有关,跟频率无关。
- **`metric_relabel_configs` 白名单**:能省**存储**(被 drop 的 series 不入库),但**不省下载/解析**
  (Prometheus 先全量下载+解析再 drop)。
- 可行做法:
  1. **慢抓 `:9001` + 白名单**(仅 :9001,158MB,间隔 300s/timeout 120s):零 exporter,但每 5min 一次带宽+解析尖峰。
  2. **UFM 本机裁剪 exporter**:本机后台慢读 loopback `:9001`,裁剪后只暴露几 KB,Prometheus 抓小端点(最干净,`:9002` 富数据走这个)。
  3. 改 UFM telemetry 采集计数器集让端点本身变小(动 UFM 配置,风险高)。

---

## 3. 当前选定方案:轻量 REST exporter

**只要 up/down → REST 是对的工具**:小 JSON、一次调用拿全 Fabric、exporter 能跑任意位置、
不依赖开 telemetry 端口、延迟低(60s)。Telemetry 火管只在将来要 BER/计数器趋势时才上(见附录 B)。

```
                    bastion(监控机)
 UFM ──REST/HTTPS──► ufm-ib-exporter(:9627) ──► Prometheus(job ufm-ib)──► Grafana / 告警
 172.16.30.33          一次取全 Fabric              既有栈                  IB 大盘
```

代码:私有仓库 `github.com/weironz/ufm-ib-exporter`;镜像 `willdockerhub/ufm-ib-exporter:latest`。

---

## 4. 详细配置步骤(当前方案)

### 4.1 前置:确认 UFM REST 可用

```bash
curl -sk -u admin:123456 https://172.16.30.33/ufmRest/app/ufm_version   # 200 + 版本 JSON 即可
curl -sk -u admin:123456 https://172.16.30.33/ufmRest/resources/ports | head -c 200
```

> ✅ **REST 走 UFM GUI 的 443 端口(开箱即暴露),当前方案无需改 UFM 监听地址。**
> 只有走 Telemetry(方案 B)才需要在 UFM 上放开 `:9001/:9002` 的监听地址 —— 详见 **附录 A**。

### 4.2 Exporter(`ufm_exporter.py`)

轮询 `/ufmRest/resources/ports`,导出 Prometheus 指标。环境变量:

| 变量 | 默认 | 说明 |
|---|---|---|
| `UFM_HOST` | 172.16.30.33 | UFM 地址 |
| `UFM_USERNAME`/`UFM_PASSWORD` | admin / — | UFM REST 凭据(`.env` 注入) |
| `LISTEN_PORT` | 9627 | 监听端口 |
| `UFM_NODE_PREFIX` | B300 | 只采该前缀主机口 |
| `UFM_EXPECTED_NODES` | 32 | 应在册节点数(整节点消失检测,缺席补 0) |
| `UFM_PORTS_PER_NODE` | 8 | 每节点物理口基线 |

**⚠️ 解析必须处理 UFM 数据模型两个坑**(否则每节点漏一半):

1. **一块 800G CX8 = 4 个 plane**(4×200G),`/resources/ports` 把 plane 子口和物理聚合口都列出来
   → **只取 `has_plane_ports == True`**(物理聚合口),否则 plane 让计数翻 4 倍。
2. **node_description 两种格式**:`B300-10:ibB…D0F0HCA0p1s3`(已纳管,带 peer/cage)和
   `B300-10 mlx5_8`(对端调试中,peer=N/A)→ 用正则 `B300-\d+` 取节点名,**两种都认**。

```python
_NODE_RE = re.compile(r"B300-\d+")
def parse_host_ports(ports):
    out=[]
    for p in ports:
        if p.get("has_plane_ports") is not True: continue     # 只要物理聚合口
        nd=p.get("node_description") or ""
        m=_NODE_RE.match(nd)
        if not m: continue
        node=m.group(0); card=nd[len(node):].lstrip(": ").strip() or "?"
        up=1 if p.get("logical_state")=="Active" else 0
        out.append((node, card, str(p.get("peer_node_name") or ""),
                    str(p.get("peer_port_dname") or ""), str(p.get("active_speed") or ""), up))
    return out
```

导出的指标:

| 指标 | 标签 | 含义 |
|---|---|---|
| `ufm_ib_link_up` | node, card, peer, cage, speed | 单口 up=1/down=0 |
| `ufm_ib_node_links_up` / `_total` | node | 每节点 up 数 / UFM 可见数(基线 8) |
| `ufm_ib_links_expected` / `_up` / `_missing` | — | 全网应在线 / up / 未在线 |
| `ufm_up` / `ufm_scrape_error` / `ufm_data_age_seconds` | — | 采集健康(失败回退 last-good) |

### 4.3 Docker 部署(bastion)

```bash
cd ~/data/ufm-ib-exporter
cp .env.example .env && vi .env        # 填 UFM_HOST / UFM_USERNAME / UFM_PASSWORD
docker compose up -d --build           # 或 docker compose pull（镜像已发布 DockerHub）
curl -s http://localhost:9627/metrics | grep -E '^ufm_up |^ufm_ib_links_'
```

### 4.4 Prometheus 接入

在 `prometheus/prometheus.yml` 的 `scrape_configs` 加(指标体量小,60s 足够):

```yaml
  - job_name: 'ufm-ib'
    scrape_interval: 60s
    scrape_timeout: 30s
    static_configs:
      - targets: ['ufm-ib-exporter:9627']    # 同 docker 网络用容器名;独立运行用 <宿主IP>:9627
        labels: { fleet: b300 }
```

热加载(勿重启,避免重抓风暴):`curl -X POST http://localhost:9090/-/reload`

### 4.5 告警规则

`prometheus/rules/ufm_ib_alerts.yml`:

```yaml
groups:
  - name: ufm-ib
    rules:
      - alert: UFMDown
        expr: ufm_up == 0
        for: 5m
        labels: { severity: critical }
      - alert: IBCx8LinkDown                  # 真 down(UFM 仍可见但非 Active)
        expr: ufm_ib_link_up == 0 and on() ufm_up == 1
        for: 2m
        labels: { severity: critical }
      - alert: IBCx8NodeDegraded              # <8(含端口 down / 消失 / 整节点消失)
        expr: ufm_ib_node_links_up < 8 and on() ufm_up == 1
        for: 5m
        labels: { severity: warning }
```

### 4.6 Grafana 大盘

`grafana/ufm_ib.json`(uid `ufm-ib`):全网在线率 / 每节点 up(8绿/1-7黄/0红)/ down 明细 / 趋势。
provisioning 部署:拷进 `provisioning/dashboards/ib/` + `dashboards.yml` 加 file provider →
**`docker compose restart grafana`**(provisioned 大盘改文件不会热加载,必须重启 grafana)。

### 4.7 (相关)主机指标 node_exporter

B300 主机带内已跑 `node_exporter:9100`。Prometheus 加 `node` job(static_configs 列各主机
`<IP>:9100` + host 标签),Grafana 导入社区 **Node Exporter Full(ID 1860)**(数据源改 uid `prometheus`)。

---

## 5. 语义说明:down vs 消失

| 情况 | UFM 表现 | 看板 | 告警 |
|---|---|---|---|
| **真 down** | 端口在列表,`logical_state≠Active` | 进 down 明细表(精确到 card/cage) | `IBCx8LinkDown` |
| **消失/未发现** | 端口整个从列表消失(如 CL-02 调试中) | 只体现为 `missing` 数 | `IBCx8NodeDegraded`(node<8) |

实测 UFM 可见的物理口**都是 Active**(up==total,无真 down);某节点 <8 多半是对端交换机调试中、
UFM 尚未完全发现,**不是断链**,稳定后会补到 8。

> 进阶:要精确列出"每节点缺的是哪个 cage/卡",需把 ibdiagnet 实测的 cage↔B300/mlx5 连线对照表
> 喂进 exporter 当应有清单,与 UFM 实报对比 → 输出"缺失端口明细"。

---

## 6. 关键踩坑速查

- UFM REST 凭据是 `admin/123456`(不是 BMC 的);Grafana admin = `admin/3dstor888.`。
- Telemetry `:9001/:9002` 是 158MB/1.26GB 火管,**别让 Prometheus 直接抓**;`metric_relabel` 只省存储不省传输/解析。
- `/resources/ports`:`has_plane_ports==True` 才是物理口;`node_description` 有冒号和空格两种格式;
  `node_guid`/`aggregated_port`/`plane` 字段为空,`name`=`<port_guid>_<plane>`。
- 掉线口从 UFM 消失(非 Down)→ 必须对所有应在册节点强制补 0,靠 `node_links_up<8` 兜底。
- 改 provisioned Grafana 大盘文件后要 `restart grafana`,不是 reload。

---

## 附录 A:启用 UFM Telemetry 监听地址(**仅方案 B 需要**)

> ⚠️ 当前选定的 **REST 方案走 443,不需要本节**。本节仅当将来要走 Telemetry(方案 B,取计数器/BER)时才做 —— 我们已在 2026-06-17 实操过一次。

UFM Telemetry 的两个 Prometheus 端点(`ibdiagnet` 进程)默认**只绑 `127.0.0.1`**,外部连不上,必须在 `gv.cfg` 放开监听地址。

**1. 现状确认**(在 UFM 主机上,本案为裸装 Ubuntu 24.04、无 Docker):
```bash
sudo ss -tlnp | grep -E ':900[12]'      # 看到 127.0.0.1:9001 / 127.0.0.1:9002 = 只绑本机
```

**2. 改绑定地址** —— 编辑生效配置 `/opt/ufm/files/conf/gv.cfg`(不是 `.org` 模板)的 `[Telemetry]` 段:

| 配置项 | 默认 | 改成 | 控制 |
|---|---|---|---|
| `primary_ip_bind_addr` | `127.0.0.1` | `172.16.30.33`(或 `0.0.0.0`) | `:9001`(计数器) |
| `secondary_ip_bind_addr` | `127.0.0.1` | `172.16.30.33`(或 `0.0.0.0`) | `:9002`(富数据) |

> 绑管理口 IP `172.16.30.33` 比 `0.0.0.0` 更紧(只暴露在管理网;**端点无鉴权**)。

一键 sed(先备份):
```bash
sudo cp -a /opt/ufm/files/conf/gv.cfg /opt/ufm/files/conf/gv.cfg.bak
sudo sed -i -E 's/^(primary_ip_bind_addr[[:space:]]*=[[:space:]]*)127\.0\.0\.1/\1172.16.30.33/; \
                s/^(secondary_ip_bind_addr[[:space:]]*=[[:space:]]*)127\.0\.0\.1/\1172.16.30.33/' \
  /opt/ufm/files/conf/gv.cfg
sudo grep -nE 'primary_ip_bind_addr|secondary_ip_bind_addr' /opt/ufm/files/conf/gv.cfg   # 确认改对
```

**3. 生效**(先试最轻的):
```bash
# gv.cfg 有 ~120s 改动监听器,先等它自动重绑:
sleep 120; sudo ss -tlnp | grep -E ':900[12]'        # 期望 172.16.30.33:9001 / :9002
# 若仍是 127.0.0.1,再重启 UFM(生产操作:SM 短暂重启,运行中 IB 流量不受影响,建议维护窗口):
sudo systemctl restart ufm-enterprise                 # = /etc/init.d/ufmd start
```

**4. 验证可达**(从将来跑 Prometheus 的主机):
```bash
curl -s http://172.16.30.33:9001/metrics | head -3    # 看到 # TYPE 即通
```

> ⚠️ 即便开了端口:`:9001`=158MB、`:9002`=1.26GB 是火管,**别让 Prometheus 直接抓**,
> 驯服办法见 §2.2(慢抓 :9001 + 白名单 / UFM 本机裁剪 exporter)。
> 另:UFM Telemetry 服务在跑(`telemetry 1.25.x`),`/metrics`=Prometheus 格式、`/csv/metrics`=CSV。
