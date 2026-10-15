# UFM 对接 Prometheus 指南

本项目使用 Rust 实现。请从[项目 README](../README.md)进入，依次查看：

- [Docker Compose 与 systemd 部署](../deploy/README.md)
- [三源配置及资源预算](../config.example.yaml)
- [架构、官方与社区方法借鉴](rust-design.md)
- [合成数据压测](benchmark.md)
- [采集状态与原生时间告警](../examples/telemetry-alerts.yml)

当前部署前提是 exporter 与 UFM 同机，读取原生回环接口；不需要为此调整 UFM/SM 的监听或重启 UFM。现有生产部署须单独核对，代码更新不等于生产切换。

telemetry 默认每 180 秒抓取，面板中的 FEC `rate` 使用 12 分钟窗口，覆盖至少四个抓取间隔。
5 分钟窗口在部分时刻只包含一个样本，会间歇返回空结果。调整抓取周期时应同步检查速率窗口。
`v0.1.1` 发布包附带的面板仍使用旧窗口；使用该包部署时请同步采用仓库更新后的面板。
