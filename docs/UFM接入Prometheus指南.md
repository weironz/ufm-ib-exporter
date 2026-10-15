# UFM 对接 Prometheus 指南

当前主实现已迁移为 Rust。请从[项目 README](../README.md)进入，依次查看：

- [三源配置及资源预算](../config.example.yaml)
- [架构、官方与社区方法借鉴](rust-design.md)
- [合成数据压测](benchmark.md)
- [采集状态与原生时间告警](../examples/telemetry-alerts.yml)

当前部署前提是 exporter 与 UFM 同机，读取原生回环接口；不需要为此调整 UFM/SM 的监听或重启 UFM。现有生产部署须单独核对，代码更新不等于生产切换。

旧版 Python 指南和源代码可在 Git 历史提交 `7ff11f363826a95699be61ded5e0b62856b778c8` 查阅。旧文档中的无校验证书示例、SSH 上传镜像/脚本、固定密码及“必须重启 Grafana”等操作方式不作为新版本的部署指引。
