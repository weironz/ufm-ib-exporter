# UFM IB Exporter · Rust

读取 NVIDIA UFM **已有数据**，输出供 Prometheus / vmagent 使用的过滤结果。推荐部署在 **UFM 所在机器**：原始大响应走本机回环，过滤后的结果再发给监控节点。不登录客户服务器，不额外执行 `perfquery`、`mlxlink` 或清计数器操作。

Rust 实现是主入口；根目录 `ufm_exporter.py` 保留为旧版对照与回退依据，新镜像不包含 Python。代码准备、镜像发布和生产切换分别进行；本仓库更新不代表生产已部署。

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
cp .env.example .env
```

私下设置环境变量 `UFM_PASSWORD`；配置文件只放非秘密参数。REST 的 HTTPS 证书必须受信任：可用 `ca_file` / `UFM_CA_FILE` 指定 CA bundle，并让 URL 主机名与证书匹配。新实现默认验证 TLS，不继承旧 Python 的 `verify=False`。

```sh
# 先在当前 shell 安全注入 UFM_PASSWORD；程序不自动读取 .env。
target/release/ufm-ib-exporter --config config/exporter.local.yaml --check-config
target/release/ufm-ib-exporter --config config/exporter.local.yaml
```

`--check-config` 不请求上游。`--print-default-config` 只打印程序默认配置，省略密码字段；不打印当前环境或实际配置中的秘密。

## UFM 同机 Docker Compose

编辑 `.env` 的密码和监听地址，并编辑 `config/exporter.local.yaml` 的 REST URL、CA、白名单与范围。默认仅监听 `127.0.0.1`；让远端监控节点访问时，显式绑定目标管理地址并限制访问来源。

```sh
docker compose --env-file .env config --quiet
docker compose --env-file .env build
docker compose --env-file .env up -d
```

Compose 使用 host 网络读取 UFM 宿主机回环接口，非 root、只读文件系统、无额外 capabilities，资源起点为 1 CPU / 1 GiB；这是待实机压测的限制，不是任何规模的容量保证。私有配置目录必须对容器 UID 65532 可读；只赋予必要的读取权限。自签证书要额外挂载 CA 并设置 `ca_file`。

Cloud 构建需要受信任代理 CA 时，用临时 BuildKit secret，不把 Cloud CA 写入镜像：

```sh
docker build --secret id=proxy_ca,src=/etc/ssl/certs/ca-certificates.crt -t ufm-ib-exporter:local .
```

默认镜像标签 `willdockerhub/ufm-ib-exporter:rust-0.1.0` 是新版本标签，不能假定已发布；生产应从固定 Git 提交构建/获取已发布镜像并核对摘要，保留旧镜像、配置和历史。

`docker-compose.ufm.yml` 是兼容模式：只运行 9001/9002，REST 继续由已有实例处理。使用时把 `.env` 的 `LISTEN_PORT` 改为 `9628`。两种模式择一，避免重复采集；不要把旧 Python 脚本挂载进 Rust 镜像。

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
docker compose kill -s SIGHUP ufm-ib-exporter
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

[压测方法与结果](docs/benchmark.md)提供合成数据下的 Rust/Python 对照；不是生产 UFM 负载或 XDR 设备兼容性验收。生产切换前须观察自然周期、原生观测时间、完整目标覆盖和 UFM 本机资源，保留旧实例的回退入口。
