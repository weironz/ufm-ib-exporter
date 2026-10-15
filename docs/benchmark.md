# 合成数据压测

2026-10-16，在 Codex Cloud 中使用 Linux x86_64、Rust 1.99.0 release 构建测试。数据在本机 HTTP 服务器动态生成，不访问 UFM、BMC、客户 OS 或 Fabric。

512 MiB 输入包含 4,096 个合成端口标签组合，各 17 个光学字段，共 69,632 条保留业务系列。其余是被白名单丢弃的文本；长标签模拟 CableInfo 的体积。Rust 还输出真实观测时间指标，故其导出体积略大。旧 Python 使用同一来源、同一白名单，依赖由已有开发 venv 提供。

| 实现 | 原始输入 | 保留业务系列 | 输出体积 | 端到端时间 | 进程 CPU 时间 | 峰值 RSS |
| --- | --- | --- | --- | --- | --- | --- |
| Rust | 536,870,912 B | 69,632 | 50,518,285 B | 0.646 s | 0.53 s | 61.68 MiB |
| 旧 Python | 536,870,912 B | 69,632 | 47,501,717 B | 5.082 s | 5.02 s | 218.49 MiB |
| Rust，增加丢弃数据 | 1,073,741,824 B | 69,632 | 50,518,287 B | 0.687 s | 0.62 s | 61.29 MiB |

每次测试只有一次上游读取。端到端时间包括进程启动、合成 HTTP 数据生成、过滤、缓存准备及一次完整抓取；不是纯解析耗时。RSS 来自 Linux `VmHWM`，CPU 来自进程累计时间。输入加倍且保留集合不变时，RSS 在这次测量中基本不变，符合流式处理目标。

这是一组单次、可复现的合成结果，不是统计置信区间，也不是 UFM 实机吞吐保证。实际标签数、保留比例、原生导出耗时、TLS、并发消费者和缓存代际重叠均会改变结果。本次没有测量 UFM 原生序列化负载，不能用这些数字声称对 SM 没有影响。

复现：

```sh
cargo build --locked --release
python3 tools/benchmark.py --binary target/release/ufm-ib-exporter --input-mib 512
python3 tools/benchmark.py --binary target/release/ufm-ib-exporter --input-mib 1024
# 需要与旧 Python 对照时，提供已安装 requirements.txt 的解释器。
python3 tools/benchmark.py --binary target/release/ufm-ib-exporter --input-mib 512 --legacy-python /path/to/python
```

输入大小、端口数和标签填充量可调整。不得把输入扩大到超过显式预算后仍视为成功；预算失败应按软件保护机制验收。生产需另外覆盖至少两个自然采集周期、双大源、较大保留集合以及慢消费者。
