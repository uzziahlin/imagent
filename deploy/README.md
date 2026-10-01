# 部署 imagent 为系统服务

`imagent start` 默认前台运行（Ctrl-C 退出）。常驻建议用系统服务管理：开机启动 + 崩溃自动重启 + 日志收集。

> ⚠️ **macOS 用户注意**：`imagent` 与系统输入法进程 `imagent`（Input Method Agent）撞名。`pkill imagent` 会杀系统输入法。务必用**全路径**（`/usr/local/bin/imagent`）操作本程序，不要用进程名 `pkill`。

> 🤖 **本目录模板由命令生成，勿手改**：`launchd/com.imagent.plist` 与
> `systemd/imagent.service` 均为 `imagent service print` 的输出——与
> `service install` 写盘走**同一份生成代码**（src/service.rs），含 `--platform`
> 显式入参。历史债（B2 合流前）：静态模板缺 `--platform`，复制装出的守护进程
> 会误走缺省平台解析。改动生成器后刷新模板（在仓库根）：
>
> ```bash
> cargo run -q -- service print --format launchd --exe /usr/local/bin/imagent \
>   --platform feishu --log-path /usr/local/var/log/imagent.log \
>   > deploy/launchd/com.imagent.plist
> cargo run -q -- service print --format systemd --exe /usr/local/bin/imagent \
>   --platform feishu > deploy/systemd/imagent.service
> ```
>
> 模板以 `--platform feishu` / `/usr/local/bin/imagent` 为示例参数生成；复制使用
> 前按实际平台（feishu / wecom / ilink）与路径修改。`--log-path` 沿用旧模板的
> `/usr/local/var/log`（见下文轮转注意事项）；`--with-env` 未用——模板不含凭据
> 环境快照（`service install` 才会把你 shell 里的 secret 快照进服务定义）。

## Linux（systemd，用户单元）

生成器产出的是**用户单元**（`WantedBy=default.target`，服务以你的用户身份运行，
日志走 journal）：

```bash
cp target/release/imagent /usr/local/bin/
mkdir -p ~/.config/systemd/user
cp deploy/systemd/imagent.service ~/.config/systemd/user/
# 按需编辑：ExecStart 的二进制路径与 --platform
systemctl --user daemon-reload
systemctl --user enable --now imagent
journalctl --user -u imagent -f       # 看日志
# 无人登录也常驻（服务器场景，一次即可）：
loginctl enable-linger $USER
```

**改造成系统单元（可选进阶）**：若确实要装到 `/etc/systemd/system/`（开机即启、
不依赖用户会话），复制后需手改两处——这是模板之外的人工步骤，改动义务在你：

- `WantedBy=default.target` 改回 `multi-user.target`；
- 取消注释并填写 `User=你的用户`。⚠️ 不要写 `User=%i`——`%i` 仅对模板单元
  （`imagent@.service`）有效，非模板单元展开为空会触发
  "User may not be empty" 启动失败（B5 记过此坑）。
- 可选加固（生成器用户单元未内置）：`NoNewPrivileges=true`、
  `ProtectSystem=strict`。⚠️ 开 `ProtectSystem=strict` 必须把 config.toml 的
  `default_workdir` 一并加进 `ReadWritePaths=%h/.imagent /srv/agent-ws`，否则
  agent 写 workdir 会被静默拒绝（首装必踩）。

## macOS（launchd）

```bash
cp target/release/imagent /usr/local/bin/
cp deploy/launchd/com.imagent.plist ~/Library/LaunchAgents/
# 按需编辑：ProgramArguments 的二进制路径与 --platform、日志路径
launchctl load ~/Library/LaunchAgents/com.imagent.plist
tail -f /usr/local/var/log/imagent.log
# 卸载：launchctl unload ~/Library/LaunchAgents/com.imagent.plist
```

> 优先用 `imagent service install`（自动注册二进制 / config 平台 / 凭据环境
> 快照，装完即生效，无上述手工编辑步骤）。内置日志轮转（50MB 触发、保留 5 份，
> `IMAGENT_LOG_MAX_MB` 可调）只覆盖它的日志路径 `~/.imagent/logs/daemon.log`
> ——本模板自选的 `/usr/local/var/log` 不在轮转范围内，需自行配 newsyslog /
> logrotate（或刷新模板时改传 `--log-path ~/.imagent/logs/daemon.log`——注意
> launchd 不展开 `~`，须写绝对路径）。

## 指标（Prometheus）

`config.toml` 默认**不开启** metrics（`metrics_addr` 留空 / 不设）；设为 `"127.0.0.1:9100"` 即开启。

```bash
curl http://127.0.0.1:9100/metrics     # prometheus 文本格式
curl http://127.0.0.1:9100/health       # JSON 状态
```

prometheus scrape 示例：
```yaml
scrape_configs:
  - job_name: imagent
    static_configs:
      - targets: ["localhost:9100"]
```

## 配置热重载

```bash
kill -HUP $(cat /run/imagent.pid 2>/dev/null || pgrep -f /usr/local/bin/imagent)
# SIGHUP → 重读 config.toml（白名单 / 工具 / permission_mode），无需重启
```
