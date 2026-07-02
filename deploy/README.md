# 部署 imagent 为系统服务

`imagent start` 默认前台运行（Ctrl-C 退出）。常驻建议用系统服务管理：开机启动 + 崩溃自动重启 + 日志收集。

> ⚠️ **macOS 用户注意**：`imagent` 与系统输入法进程 `imagent`（Input Method Agent）撞名。`pkill imagent` 会杀系统输入法。务必用**全路径**（`/usr/local/bin/imagent`）操作本程序，不要用进程名 `pkill`。

## Linux（systemd）

```bash
sudo cp target/release/imagent /usr/local/bin/
sudo cp deploy/systemd/imagent.service /etc/systemd/system/
# 编辑 service：User=你的用户；ExecStart/Environment 路径
sudo systemctl daemon-reload
sudo systemctl enable --now imagent
journalctl -u imagent -f          # 看日志
```

## macOS（launchd）

```bash
cp target/release/imagent /usr/local/bin/
cp deploy/launchd/com.imagent.plist ~/Library/LaunchAgents/
# 编辑 plist：ProgramArguments 路径、日志路径
launchctl load ~/Library/LaunchAgents/com.imagent.plist
tail -f /tmp/imagent.log
# 卸载：launchctl unload ~/Library/LaunchAgents/com.imagent.plist
```

## 指标（Prometheus）

`config.toml` 默认 `metrics_addr = "127.0.0.1:9100"`（设 `null` 关闭）。

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
