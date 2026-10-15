# UFM 同机部署

两种方案择一，都在 UFM 所在 Linux 主机运行，读取 `127.0.0.1` 上的 REST、9001 和 9002。它们使用相同的 [配置模板](../config.example.yaml)，不需要登录客户服务器或修改 UFM/SM 监听。

`v0.1` Release 对应程序版本 `0.1.0`，提供 Linux amd64 / arm64 的静态二进制和 SHA-256 校验清单。Docker 镜像为 `willdockerhub/ufm-ib-exporter:v0.1`，同时发布 `v0.1.0` 和 `latest`。生产建议记录二进制校验值或使用镜像摘要。发布产物不等于生产已切换。

## 获取发布包

不要求目标机器安装 Rust。需要 GitHub CLI（`gh`）、`curl`、`tar`、`sha256sum` 和系统 CA 证书；原生运行必须安装发行版的 `ca-certificates` 包。

仓库目前为私有仓库。先在可信终端配置具有仓库读取权限的 GitHub 身份，并用 `gh auth status` 确认；私有 Release 的匿名下载 URL 会返回 404。以下使用 GitHub CLI 下载，凭据不放进部署文件，也不提供给 exporter 服务。

```sh
version=v0.1
case "$(uname -m)" in
  x86_64) arch=amd64 ;;
  aarch64|arm64) arch=arm64 ;;
  *) echo 'Unsupported architecture' >&2; exit 1 ;;
esac
asset="ufm-ib-exporter-${version}-linux-${arch}.tar.gz"
mkdir -p "ufm-ib-exporter-${version}"
cd "ufm-ib-exporter-${version}"
gh release download "$version" --repo weironz/ufm-ib-exporter --pattern "$asset" --pattern SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf "$asset"
./ufm-ib-exporter --version
```

发布包包含二进制、配置模板、`deploy/`、面板、规则和文档。以下命令从解压后的目录开始；从 Git 克隆目录部署 Compose 也适用。

## Docker Compose

前提：Linux Docker Engine 和 Compose 插件。使用 host 网络访问宿主机的回环接口；镜像自带 CA 证书，以 UID/GID 65532 运行。

```sh
cd deploy/docker-compose
cp .env.example .env
chmod 600 .env
sudo install -d -o root -g 65532 -m 0750 config
sudo install -o root -g 65532 -m 0640 ../../config.example.yaml config/exporter.local.yaml
sudoedit config/exporter.local.yaml
# 编辑 .env，填写 UFM_PASSWORD，并按需修改 IMAGE 和监听地址。
${EDITOR:-vi} .env
docker compose --env-file .env config --quiet
docker compose --env-file .env pull
docker compose --env-file .env up -d
docker compose ps
docker compose logs --tail=100 ufm-ib-exporter
```

`.env` 密码建议使用单引号，避免 Compose 将 `$` 当作变量插值。只运行 `config --quiet` 校验，不输出包含密码的完整展开配置。

默认监听 `127.0.0.1:9627`。让监控节点抓取时，在 `.env` 设置 `LISTEN_ADDRESS` 为 UFM 管理地址，限制网络访问来源，并配置 Prometheus/vmagent 抓取该地址。REST URL 的主机名必须匹配 TLS 证书；自签 CA 可放入只读配置目录，例如 `config/ufm-ca.pem`，在 YAML 设置 `ca_file: /etc/ufm-ib-exporter/ufm-ca.pem`，并保证容器用户可读。

配置文件支持原子替换后的 SIGHUP 重载：

```sh
docker compose kill -s SIGHUP ufm-ib-exporter
docker compose logs --tail=30 ufm-ib-exporter
```

监听地址、CA、线程数和并发上限变化需要重建/重启；修改 `.env` 后执行 `docker compose up -d --force-recreate`。

只需要 telemetry 时，在 YAML 关闭 REST，或使用 `docker-compose.telemetry.yml`。该文件使用独立服务名和默认端口 9628；同时将 `.env` 的 `LISTEN_PORT` 改为 9628：

```sh
docker compose --env-file .env -f docker-compose.telemetry.yml config --quiet
docker compose --env-file .env -f docker-compose.telemetry.yml up -d
```

不要同时启动两份采集相同来源的 exporter。Compose 的 1 CPU / 1 GiB 上限需结合实际规模调整。

## systemd 二进制

适用于使用 systemd 的 Linux。下面首次安装命令会创建专用系统用户，不授予管理员权限：

```sh
id ufm-ib-exporter >/dev/null 2>&1 || sudo useradd --system --no-create-home --shell /usr/sbin/nologin ufm-ib-exporter
sudo install -m 0755 ufm-ib-exporter /usr/local/bin/ufm-ib-exporter
sudo install -d -o root -g ufm-ib-exporter -m 0750 /etc/ufm-ib-exporter
sudo install -o root -g ufm-ib-exporter -m 0640 config.example.yaml /etc/ufm-ib-exporter/config.yaml
sudo install -o root -g root -m 0600 deploy/systemd/ufm-ib-exporter.env.example /etc/ufm-ib-exporter/exporter.env
sudo install -m 0644 deploy/systemd/ufm-ib-exporter.service /etc/systemd/system/ufm-ib-exporter.service
sudoedit /etc/ufm-ib-exporter/config.yaml
sudoedit /etc/ufm-ib-exporter/exporter.env
sudo systemd-analyze verify /etc/systemd/system/ufm-ib-exporter.service
sudo systemctl daemon-reload
sudo systemctl enable --now ufm-ib-exporter
sudo systemctl status ufm-ib-exporter --no-pager
sudo journalctl -u ufm-ib-exporter -n 100 --no-pager
```

在 `exporter.env` 填写 `UFM_PASSWORD`；此文件由 systemd 读取，不要 `source`。监听地址在 `config.yaml` 中设置。自签 CA 放入 `/etc/ufm-ib-exporter/`，赋予服务用户只读权限，并配置 `ca_file`。启动前自动执行 `--check-config`；它只验证配置，不连接 UFM。

单元使用无权限用户、只读系统目录、空 capability 集合、1 CPU / 1 GiB 上限；它保留宿主机网络访问，以读取 UFM 回环接口。日志进入 journal。根据现场规模调整资源限制时，使用 `systemctl edit ufm-ib-exporter` 添加 drop-in。

```sh
# 修改来源配置后重载；通过日志确认已接受。
sudo systemctl reload ufm-ib-exporter
sudo journalctl -u ufm-ib-exporter -n 30 --no-pager
# 修改环境变量、监听、CA 或运行时上限后重启。
sudo systemctl restart ufm-ib-exporter
```

`systemctl reload` 仅发送信号；无效配置会在程序日志中显示拒绝并保留当前配置，不能只凭命令退出码判断新配置生效。

## 验收、升级与回退

```sh
curl -fsS http://127.0.0.1:9627/healthz
curl -fsS http://127.0.0.1:9627/readyz
curl -fsS http://127.0.0.1:9627/metrics
```

若改成管理 IP 监听，上述地址也需相应修改。`healthz` 只代表进程可响应；`readyz` 要求所有启用来源有未过期数据。首次光学采集需要等待完整一轮；用来源状态、实际观测时间和目标覆盖验收，不能只看 HTTP 200。

升级前保存配置及镜像摘要/二进制校验值。Compose 修改 `IMAGE` 为已发布版本或摘要后 `pull`、`up -d`；systemd 停止服务后替换已验证的二进制，再启动。已有配置文件不应在升级时被模板覆盖。回退时恢复保存的镜像摘要或二进制及匹配配置，保留监控历史。

## CI 发布

推送版本标签（本次为 `v0.1`）后，CI 先执行格式、Clippy、单元/集成测试、真实 Prometheus 规则和容器验证，再构建两个架构的 musl 静态二进制。镜像直接使用这两个发布二进制，支持 `linux/amd64`、`linux/arm64`，不在 Docker Hub 构建另一份应用。

Docker Hub 凭据使用仓库 Actions Secret：`DOCKERHUB_USERNAME`（可省略，默认 `willdockerhub`）和 `DOCKERHUB_TOKEN`；兼容 `DOCKERHUB_PASSWORD` 作为 token 的替代名称。Token 需允许写入 `willdockerhub/ufm-ib-exporter`。GitHub Release 使用工作流自带的 `GITHUB_TOKEN`，仅发布 job 获得 `contents: write`。

Release 附带两个压缩包、`SHA256SUMS` 和 `docker-image.txt`（镜像 digest）。分支推送/PR 只运行检查，版本标签才发布。重复运行会核对已有 Release 附件，拒绝覆盖不同内容；已发布版本的修订应使用新标签。
