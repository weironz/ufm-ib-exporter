# UFM IB Exporter · Rust

读取 NVIDIA UFM **已有数据**，输出供 Prometheus / vmagent 使用的过滤结果。推荐部署在 **UFM 所在机器**：原始大响应走本机回环，过滤后的结果再发给监控节点。不登录客户服务器，不额外执行 `perfquery`、`mlxlink` 或清计数器操作。

Exporter 的实现、构建和运行均使用 Rust。代码准备、镜像发布和生产切换分别进行；本仓库更新不代表生产已部署。

## 三个数据源

| 数据源 | 默认配置文件中的范围 | 周期 |
| --- | --- | --- |
| REST `/ufmRest/resources/ports` | 物理链路 up/down、节点基线、peer/cage、速率 | 60 秒 |
| `127.0.0.1:9001/metrics` | 必要的错误、闪断、PHY/FEC 和 PortXmitWait 计数 | 180 秒 |
| `127.0.0.1:9002/metrics/CableInfo` | Rx/Tx 每 lane 光功率、`cable_temperature` | 600 秒 |

`config.example.yaml` 显式启用三源；不加载配置文件时，程序默认只启用 REST。所有来源都可独立关闭。字段名、计数单位、显式 BER 和 native 子路径是否存在，以 UFM 版本及现场为准。

## 大响应的处理

- 以最多 64 KiB 的片段扫描，跨片段只保留有大小上限的未完成行；先按指标名过滤，再解析需要的标签和时间戳。
- 三源后台更新；HTTP 抓取只读完整、不可变缓存，使用共享字节缓冲，不为每次抓取复制整份数据。
- 默认两个运行线程、一个大源采集许可：9001/9002 错开，REST 独立。周期是每轮完成后的等待时间；等待其他大源也会延长实际更新间隔。
- 对输入、单行、缓存、指标数量、标签组合、总处理时长和抓取并发设置上限；缓存全部成功后才发布。多个原生分片顺序读取，任一失败则拒绝整轮更新。
- 失败、空响应、重复指标、过期或超预算时停止输出该源业务值；保留诊断状态，不无限回放 last-good。
- 需要的来源标签可配置过滤；丢弃标签后若产生重复序列，整轮失败，不悄悄合并不同端口。

本机部署减少原始数据跨网传输；Rust 减少 exporter 的处理开销。两者都不消除 UFM 生成原始响应的成本。[设计依据与性能边界](docs/rust-design.md)。

## 原生运行

需要 Rust 1.88+。构建不连接 UFM：

```sh
cargo build --locked --release
mkdir -p config
cp config.example.yaml config/exporter.local.yaml
```

私下设置环境变量 `UFM_PASSWORD`；配置文件只放非秘密参数。REST 的 HTTPS 证书必须受信任：可用 `ca_file` / `UFM_CA_FILE` 指定 CA bundle，并让 URL 主机名与证书匹配。默认验证 TLS 证书及主机名。

```sh
# 先在当前 shell 安全注入 UFM_PASSWORD；程序不自动读取 .env。
target/release/ufm-ib-exporter --config config/exporter.local.yaml --check-config
target/release/ufm-ib-exporter --config config/exporter.local.yaml
```

`--check-config` 不请求上游。`--print-default-config` 只打印程序默认配置，省略密码字段；不打印当前环境或实际配置中的秘密。

## 部署与发布

[deploy/README.md](deploy/README.md) 提供两种完整方案：

| 方式 | 部署文件 | 适用情况 |
| --- | --- | --- |
| Docker Compose | [deploy/docker-compose/](deploy/docker-compose/) | 在 UFM 宿主机拉取多架构镜像运行 |
| systemd 二进制 | [deploy/systemd/](deploy/systemd/) | 下载静态二进制，以专用用户交给 systemd 管理 |

[GitHub Release v0.1.1](https://github.com/weironz/ufm-ib-exporter/releases/tag/v0.1.1) 提供 Linux amd64 / arm64 压缩包、`SHA256SUMS` 和镜像 digest。当前程序版本为 `0.1.1`；Docker 镜像为 `willdockerhub/ufm-ib-exporter:v0.1.1`，同时提供 `latest`。镜像使用同一批发布二进制构建，生产建议固定摘要。

两种方式均默认监听 `127.0.0.1:9627`，以无权限用户运行。让远端监控节点抓取时，显式绑定 UFM 管理地址并限制访问来源；REST TLS 主机名与 CA 必须匹配。来源开关、白名单和资源预算使用同一份 YAML 配置。

CI 在每次分支推送和 PR 执行检查；推送版本标签后，检查通过才构建并发布二进制及 Docker 镜像。Docker Hub 使用 Actions Secret `DOCKERHUB_TOKEN`（兼容 `DOCKERHUB_PASSWORD`），账号默认 `willdockerhub`，可由 `DOCKERHUB_USERNAME` 指定。

开发者仍可使用根目录 Dockerfile 从源码构建；Cloud 构建需要代理 CA 时通过临时 secret 提供：

```sh
docker build --secret id=proxy_ca,src=/etc/ssl/certs/ca-certificates.crt -t ufm-ib-exporter:local .
```

## 自定义与重载

完整参数见 [config.example.yaml](config.example.yaml)。支持指标正则 `keep`、节点 `node_filter`、任意标签 `label_filters`、`drop_labels`、来源开关、周期和各类预算。例如只保留指定端口的模块温度：

```yaml
telemetry_9002:
  enabled: true
  keep: ['^cable_temperature$']
  label_filters:
    Port: '^(1|2)$'
```

标签必须实际存在；`node_filter` 兼容 `node_description` 和 `Port_Name`。小写节点名也可使用 `(?i)` 忽略大小写。9002 省略的字段继承光学默认值，不会误用 9001 地址。

原生计数集、分片 URL 可以配置在 `urls` 列表中替代 `url`，但必须是本程序接受的 Prometheus 文本格式；官方 TFS 文档的 CSV 路由不能直接使用。所有需要的分片均应列出，分片不会自动发现或减少合计数据量。现网未知的路由不会被程序自动探测。

Linux 下用 SIGHUP 验证并加载新来源配置：

```sh
# 完整写好/原子替换配置文件后，再发送信号。
docker compose --env-file deploy/docker-compose/.env -f deploy/docker-compose/docker-compose.yml kill -s SIGHUP ufm-ib-exporter
# systemd 部署使用：sudo systemctl reload ufm-ib-exporter
```

来源配置在同一代切换，旧请求取消、旧缓存清除；错误配置保留原配置。监听地址、线程数、并发上限和 CA 变化需重启，SIGHUP 会拒绝这些变化。环境变量优先于 YAML；运行中无法通过修改宿主机 `.env` 改变进程环境，修改环境变量需重建容器。

## Prometheus 与数据有效期

对外统一 `/metrics`。`/healthz` 只代表进程可响应；`/readyz` 要求所有启用来源都有可用缓存。

```yaml
scrape_configs:
  - job_name: ufm-ib
    scrape_interval: 60s
    scrape_timeout: 30s
    static_configs:
      - targets: ['UFM_MANAGEMENT_IP:9627']
```

REST 的 `ufm_ib_*` 及 telemetry 原生业务指标名保持兼容；状态含 `ufm_up`、`ufm_telemetry_up{source}`、成功轮次、读取/缓存字节数、原生观测年龄和失败原因。下载失败不会把缺失节点补成当前硬件异常；失败来源不输出业务指标。

默认要求 telemetry 的显式**毫秒时间戳**。缓存样本供每次 scrape 使用，同时输出 `ufm_telemetry_observed_at_seconds{原有标签,telemetry_source}` 记录原始观测时间。业务值仅在缓存年龄、最旧接受观测年龄均未过期时输出；任何冻结数据不会因 HTTP 200 或 scrape 而续期。`require_timestamps: false` 只用于确认无原生时间戳的特殊来源，此时无法证明设备观测新鲜度。

可选 `preserve_timestamps: true` 保留原始业务时间戳；慢采集可能超过 Prometheus 默认 lookback，出现图表空档，需按原生时间语义设计查询。不要全局扩大 lookback 或补零掩盖缺数。

改进后的面板在 [grafana/ufm_ib_quality.json](grafana/ufm_ib_quality.json)，示例规则在 [examples/telemetry-alerts.yml](examples/telemetry-alerts.yml)。`rate` / `increase` 的窗口需覆盖足够的源更新轮次；缓存重采样不是新的硬件测量。REST 缺口也不等于硬件故障台数，光功率阈值必须结合模块规格。

## 验证与性能

运行时只需 Rust 二进制。以下集成测试与合成数据工具使用 Python 3 标准库，不需要 pip 依赖，也不参与生产采集。

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
# 可选变量须指向真实 Prometheus 二进制，CI 会实际执行该项。
PROMETHEUS_BINARY=/path/to/prometheus python3 tests/integration.py target/debug/ufm-ib-exporter
cargo build --locked --release
python3 tools/benchmark.py --binary target/release/ufm-ib-exporter --input-mib 512
```

[压测方法与结果](docs/benchmark.md)记录 Rust exporter 在不同输入量下的耗时与内存；不是生产 UFM 负载或 XDR 设备兼容性验收。生产切换前须观察自然周期、原生观测时间、完整目标覆盖和 UFM 本机资源，保留切换前的镜像摘要与部署配置。
