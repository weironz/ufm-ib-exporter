# UFM IB Exporter

一个轻量级 Prometheus exporter,通过 **NVIDIA UFM**(InfiniBand Subnet Manager)的 REST API
采集 InfiniBand 计算网每口的**链路 up/down**,面向 **HGX B300 + ConnectX-8(XDR 800G)+ Quantum-X800(Q3400)** 机群。

> 从 `b300-redfish-exporter`(BMC 带外硬件监控)中抽离独立成项目:IB 链路是另一套数据源(UFM,而非 BMC),职责单一、可独立部署。

## 为什么走 UFM,而不是 BMC

- **BMC 带外读 IB 链路不可靠(假阴性)**:实测 BMC 经 NC-SI/MCTP sideband 会把主机侧 **ACTIVE 800G** 的口误报成 `Disabled/LinkDown`,且各节点端口数不一致。UFM 是该 fabric 的 SM,链路状态由它说了算。
- **为什么用 REST `/resources/ports`,而不是 UFM telemetry `/metrics`**:UFM telemetry 端点单次是 **158MB(:9001)/ 1.26GB(:9002)** 的"火管",直接喂 Prometheus 会打爆(且 `metric_relabel` 是先全量下载+解析再 drop,省存储但不省传输/解析)。REST `/resources/ports` 是**小 JSON**,正好够 up/down。富计数器/BER 趋势是另一回事(可在 UFM 本机做裁剪采集),不在本 exporter 范围。

## 关键设计:掉线口会"消失"

down/掉线的 CX8 口在 UFM 里是**直接从端口列表消失**(而非显示 `Down`),整节点也可能整个消失。
所以:

- 逐口 `ufm_ib_link_up == 0` **抓不到**已消失的口;
- 主力信号是 **`ufm_ib_node_links_up < 8`** —— exporter 对**所有应在册节点(`B300-1..N`)强制发指标、缺席补 0**,一条阈值同时覆盖「端口 down / 端口消失 / 整节点消失」三种。

## 指标

| 指标 | 标签 | 含义 |
|---|---|---|
| `ufm_up` | — | UFM REST 可达且解析成功=1,失败=0(失败回退上次快照) |
| `ufm_scrape_error` | — | 本次抓取失败=1 |
| `ufm_scrape_duration_seconds` | — | 本次抓取耗时(秒) |
| `ufm_data_age_seconds` | — | 所展示数据年龄(秒;0=本次新鲜,>0=回退了缓存) |
| `ufm_ib_link_up` | node, card, peer, cage, speed | 单口 CX8 链路 up=1/down=0;`peer`=Q3400 名,`cage`=交换机 cage |
| `ufm_ib_node_links_up` | node | 每节点 up 链路数(基线 8;`<8` 即异常,含整节点缺失=0) |
| `ufm_ib_node_links_total` | node | 每节点 UFM 可见 CX8 端口数 |
| `ufm_ib_links_expected` | — | 全网应在线链路数(节点数×每节点基线) |
| `ufm_ib_links_up` | — | 全网 up 链路总数 |
| `ufm_ib_links_missing` | — | 全网未 up 链路数(down/消失/整节点缺失) |

```promql
ufm_ib_node_links_up < 8 and on() ufm_up == 1   # 不足 8 条 up 的节点
ufm_ib_links_up / ufm_ib_links_expected          # 全网在线率
ufm_ib_link_up == 0                              # 具体哪口 down(带 peer/cage 定位)
```

## 快速开始

```bash
cp .env.example .env
vi .env                         # 填 UFM_HOST / UFM_USERNAME / UFM_PASSWORD

docker compose up -d --build
curl -s http://localhost:9627/metrics | grep -E 'ufm_up|ufm_ib_links_'
```

不想用 Docker 也可直接跑:

```bash
pip install -r requirements.txt
UFM_HOST=172.16.30.33 UFM_USERNAME=admin UFM_PASSWORD=*** python ufm_exporter.py
```

## 配置(环境变量)

| 变量 | 默认 | 说明 |
|---|---|---|
| `UFM_HOST` | 172.16.30.33 | UFM 地址 |
| `UFM_USERNAME` / `UFM_PASSWORD` | admin / — | UFM REST 凭据(`.env` 注入,不入库) |
| `LISTEN_PORT` | 9627 | exporter 监听端口 |
| `TIMEOUT` | 30 | UFM 请求超时(秒) |
| `UFM_NODE_PREFIX` | B300 | 只采该前缀的主机 CA 端口(交换机口自动排除) |
| `UFM_EXPECTED_NODES` | 32 | 应在册节点数(`B300-1..N`),用于整节点消失检测 |
| `UFM_PORTS_PER_NODE` | 8 | 每节点 CX8 端口基线 |

## 接入 Prometheus

把 [`examples/prometheus-job.yml`](examples/prometheus-job.yml) 的 job 加进你的 `prometheus.yml`,
把 [`examples/alerts.yml`](examples/alerts.yml)(`UFMDown` / `IBCx8LinkDown` / `IBCx8NodeDegraded`)放进 rule_files 目录。

- 与既有监控栈**同一 docker 网络**:target 用容器名 `ufm-ib-exporter:9627`(见 `docker-compose.yml` 里 networks 注释)。
- **独立运行**:target 用 `<宿主IP>:9627`。

## License

内部使用。
