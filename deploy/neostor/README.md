# NeoStor 指标上行

该可选 vmagent 读取已经部署的 Rust exporter 缓存，经本机 NeoStor Agent 的
`127.0.0.1:19091/api/v1/write` 上传。它不连接 UFM 原生接口、BMC 或租户 OS，
不修改 Agent 管理的配置。现有 Prometheus 可独立读取同一缓存，不触发重复 UFM 轮询。

前提：生产变更已获当次授权；两个 exporter 就绪；本机 Agent 已正确绑定 IDC，
指标 relay 已启用并使用 HTTPS 连接中心。不要将测试 Agent 或测试中心当成生产。
本目录无 token；中心从 Agent 鉴权身份强制设置 `source/site_code/idc/agent_id`。

按固定 Git 提交从 GitHub 下载本目录后，在目标机准备独立运行目录及配置：

```sh
mkdir -p /srv/ufm-neostor/config
python3 deploy/neostor/render.py --rest-target 192.168.1.10:9627 \
  --telemetry-target 192.168.1.10:9628 --site-code example-idc \
  --output /srv/ufm-neostor/config/scrape.json
cp deploy/neostor/.env.example /srv/ufm-neostor/.env
# 按现场填写根目录、真实运行 UID/GID、专用队列卷名；不放密码。
```

首次创建专用 Docker volume，使用同一固定 vmagent 镜像、只挂载新空卷，
将根目录所有权设为 `.env` 中的 UID/GID，再启动。不得操作既有指标队列的权限或内容。
使用固定 project 名和 `.env` 执行 `docker compose config --quiet`；
先以镜像的 `-dryRun -promscrape.config=/config/scrape.json` 验证暂存配置，再 `up -d`。
新增 overlay 到现有 NeoStor 项目时还需更新该项目编排清单；本方案使用独立 project，避免 orphan 清理。

队列独立持久化，上限 5 GB；1 个写入 worker，块最多 5000 样本 / 1 MiB，
适配中心的请求限制及 Agent 共享 relay 的并发限制。满队列仍可能丢弃，需观察积压/丢弃指标。
telemetry 启用流式解析，响应上限为 512 MiB；REST 上限为 8 MiB。默认 16 MiB
抓取上限不足以容纳 UFM 过滤缓存，不能只增加 remote-write 块大小来处理这个问题。
不要换 URL、清卷或反复创建新队列来隐藏失败。

`ufm_source` 保存 exporter 的原始 `source` 标签，避免中心强制的
`source="neostor"` 合并 9001/9002 状态。Fabric 指标没有虚构 `device_id/monitoring_id`；
它们是 IDC 范围数据，不能当作某台服务器的已发布监控配置。
现有 NeoStor 的服务器/SNMP 详情页不会自动成为 UFM Fabric 大盘，需要相应展示支持。

验收：`127.0.0.1:19096/api/v1/targets` 两目标 up，实际新抓取后有成功发送请求，
积压回落为零、无丢弃，中心接收返回 2xx；中心查询须确认真实站点、Agent 身份和
`ufm_source` 区分。relay healthz 只表示本地监听，不证明中心接收。
`up=1` 或链路达到基线不表示设备无硬件问题。

回退仅停止此独立 forwarder，保留配置/队列/历史；现有 exporter、Prometheus、
Redfish/SNMP、Agent 和业务服务保持运行。
