# Web 驾驶舱：部署与升级

网页驾驶舱由三部分组成：

| 部件 | 跑在哪 | 作用 |
|---|---|---|
| `imagent-relay` | 一台有域名的服务器 | 中继：认证、转发、离线缓冲。**前端页面内嵌在它里面**，不需要单独部署 |
| `imagent` 网关 | 你的每台电脑（`platforms` 含 `web`） | 主动向中继建连，电脑不开放任何端口 |
| 浏览器 / PWA | 手机、桌面 | 打开中继地址即用 |

中继只做转发，不懂业务；浏览器与网关之间的消息端到端加密，中继看不到明文。

## 1. 部署中继

### 1.1 构建

中继是一个独立的二进制。在 Linux 服务器上直接构建，或在 Mac 上交叉编译出静态链接版本：

```bash
cargo build --release -p imagent-relay                                   # Linux 上
cargo zigbuild --release -p imagent-relay --target x86_64-unknown-linux-musl   # Mac 上（需 cargo-zigbuild 与 zig）
```

构建会把 `web-ui/` 的前端产物（`crates/relay/static/`）一起打进去，所以升级前端等于重新构建并部署中继。

### 1.2 环境变量

中继没有免认证模式，下面两个变量缺一个就拒绝启动。放进一个 `0600` 的文件，比如 `/opt/imagent-relay/env`：

```bash
IMAGENT_RELAY_USER_TOKEN=<openssl rand -hex 32 生成>      # 浏览器登录用的访问令牌
IMAGENT_RELAY_GATEWAY_KEYS=home-mac=<第一台电脑的网关公钥>   # 64 位十六进制，由 imagent join 打印
```

这两个值**只在首次启动时起作用**（作为种子写入状态目录）。之后以状态目录里的文件为准，再改环境变量不会生效：

| 状态目录下的文件 | 内容 | 之后怎么改 |
|---|---|---|
| `machines.json` | 各台电脑的公钥 | 网页「管理 → 机器管理」 |
| `user_token_hash` | 访问令牌的哈希 | 网页「管理 → 访问令牌 → 轮换」 |
| `sessions.json` | 已登录设备 | 网页「管理 → 已登录设备」 |
| `credentials.json` | passkey | 网页「管理 → 通行钥匙」 |

其它可选变量：

| 变量 | 缺省 | 说明 |
|---|---|---|
| `IMAGENT_RELAY_STATE_DIR` | `/var/lib/imagent-relay` | 状态目录 |
| `IMAGENT_RELAY_BIND` | `127.0.0.1:3010` | 监听地址。只允许本机回环，公网入口交给反向代理 |
| `IMAGENT_RELAY_GATEWAY_TOKENS` | — | 电脑用令牌而非密钥认证时的「机器名 = 令牌」表，与密钥表至少配一个 |
| `IMAGENT_RELAY_RP_ID` | 按请求的域名推导 | passkey 的依赖方标识。固定下来之后换域名或端口不影响已注册的 passkey |
| `IMAGENT_PUSH_VAPID_PRIVATE` | 自动生成 | Web Push 的签名私钥，通常不用设 |

### 1.3 systemd 服务

`/etc/systemd/system/imagent-relay.service`：

```ini
[Unit]
Description=imagent relay (Web cockpit frame relay, /imagent/*)
After=network.target

[Service]
Type=simple
DynamicUser=yes
EnvironmentFile=/opt/imagent-relay/env
StateDirectory=imagent-relay
ExecStart=/opt/imagent-relay/imagent-relay
Restart=on-failure
RestartSec=3
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
ProtectClock=true
ProtectHostname=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=true
RestrictRealtime=true
RestrictSUIDGID=true
LockPersonality=true
MemoryDenyWriteExecute=yes
CapabilityBoundingSet=
SystemCallFilter=@system-service
SystemCallErrorNumber=EPERM

[Install]
WantedBy=multi-user.target
```

`DynamicUser` 让服务以临时用户运行，`StateDirectory` 负责创建并授权状态目录，其余是对进程的系统调用与文件系统收紧。

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now imagent-relay
systemctl status imagent-relay
curl -fsS http://127.0.0.1:3010/imagent/healthz     # 返回 200 即正常
```

### 1.4 反向代理

中继只监听本机，公网入口由反向代理终结 TLS。以 Caddy 为例：

```
你的域名 {
    handle_path /imagent/* {
        reverse_proxy 127.0.0.1:3010
    }
}
```

`handle_path` 会去掉 `/imagent` 前缀再转发，中继同时接受带前缀和不带前缀的路径。WebSocket 不需要额外配置。其它反向代理（Nginx、Traefik 等）照此配置即可：把 `/imagent/*` 转到 `127.0.0.1:3010`，支持 WebSocket 升级。

### 1.5 首次登录

浏览器打开 `https://你的域名/imagent/`，选「使用访问令牌」，粘贴 `IMAGENT_RELAY_USER_TOKEN` 的值。访问令牌只用于这一次登录，之后浏览器持有的是 30 天有效的设备会话。

登录后建议到「管理 → 通行钥匙」注册一个 passkey，之后可以用面容或指纹登录，并在「访问令牌」里轮换掉最初那个。

## 2. 接入电脑

在每台要被远程驾驶的电脑上：

```bash
imagent join --relay wss://你的域名/imagent/gateway --name home-mac --workdir ~/work
```

它会生成这台电脑的密钥（`~/.imagent/web-gateway-key`）、把 `platforms` / `web_relay_url` / `web_machine_id` 写进 `config.toml`，并打印一串公钥和指纹。重复执行是安全的，已有密钥不会被重建。

- **第一台电脑**：此时中继还没启动过，公钥要写进 1.2 节的 `IMAGENT_RELAY_GATEWAY_KEYS`。
- **之后的电脑**：在网页「管理 → 机器管理 → 添加」里填机器名和公钥。给每台电脑不同的 `--name`（1–32 位，字母、数字、`_`、`-`），同时在线最多 4 台。

然后在 `config.toml` 里把网页消息的发送者加进白名单，再启动：

```toml
allowed_senders = ["web:user"]      # 与其它平台的发送者并列即可
```

```bash
imagent start
```

第一次在浏览器里连上这台电脑时，会弹出加密指纹让你核对，与 `imagent join` 打印的指纹（或网页「管理 → 已信任机器指纹」）一致就点「信任并启用加密」。

## 3. 手机通知

iPhone 需要先把页面「添加到主屏幕」，再在侧栏打开「通知」。任务完成、出错、需要审批、本地 Claude Code 窗口回信时，手机会收到推送，点开直达对应的会话。正在开着驾驶舱的设备不会重复收到推送。

电脑上打开了 peer 通道（`peer_enabled`）后还有两种：关注的本地窗口跑完一轮（侧栏「本地窗口」里点星标），以及任何本地窗口开始等你确认。

推送的标题里带有消息的开头（最多 32 个字），这一小段是明文经过中继的；消息正文的其余部分仍然端到端加密。

## 4. 升级

先升中继，再升各台电脑上的网关。

**中继**（前端随中继一起升级）。替换二进制并重启，网关和浏览器会在一两秒内自动重连：

```bash
scp target/x86_64-unknown-linux-musl/release/imagent-relay 你的服务器:/opt/imagent-relay/imagent-relay.new
# 在服务器上：
cd /opt/imagent-relay
sha256sum imagent-relay.new                     # 与本地 sha256 一致再继续
cp -p imagent-relay imagent-relay.v<旧版本号>.bak
mv imagent-relay.new imagent-relay
systemctl restart imagent-relay
systemctl show imagent-relay -p MainPID         # PID 变了说明重启成功
curl -fsS http://127.0.0.1:3010/imagent/healthz
curl -s http://127.0.0.1:3010/imagent/sw.js | grep 'const BUILD'
```

最后一行输出的构建戳，应当等于本地 `shasum -a 256 crates/relay/static/index.html | cut -c1-16` 的结果。对不上说明部署的不是刚构建的那份。

**网关**。用新版本的 `imagent` 二进制替换旧的，然后重启服务（`imagent service install` 重装一次即可）。重启时正在执行的任务会先跑完再退出。

驾驶舱侧栏底部显示的是前端版本，也就是中继的版本。网关的版本目前不会显示在页面上，如果升级后某个功能不生效，先确认各台电脑上的网关是否也都升级了。

## 5. 常见问题

- **电脑显示离线**：检查这台电脑上的 `imagent` 是否在运行，`config.toml` 里的 `web_relay_url` 是否指向中继、`web_machine_id` 是否与网页里登记的机器名一致。
- **提示「登录会话数已达上限」**：最多 32 个登录会话。在任一已登录设备的「管理 → 已登录设备」里吊销不用的会话，再重新登录。本机的草稿和缓存不会被清掉。
- **状态栏短暂显示「重连中」**：中继对每个连接有频率限制，快速连续切换会话等操作可能触发，随后会自动重连，不需要处理。
- **换了域名后 passkey 登录不了**：passkey 绑定在注册时的域名上。用访问令牌登录后重新注册一个。
- **语音输入按钮不出现**：电脑上的 `config.toml` 需要同时配置 `whisper_bin` 和 `whisper_model`，见[配置参考](CONFIG.md)。
