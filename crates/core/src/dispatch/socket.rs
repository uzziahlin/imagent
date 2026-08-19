//! 权限审批 Unix socket：MCP 子命令 ↔ 主进程的请求/回复管道。

use super::*;

impl Dispatcher {
    /// spawn socket accept task：每个连接独立 spawn，读权限请求 → send_text 询问
    /// 用户 → register 等 receiver → 写回复回 socket。
    #[cfg(unix)]
    pub(super) fn spawn_socket_accept(self: &Arc<Self>, sock: String) {
        // 清理可能残留的旧 socket 文件。
        let _ = std::fs::remove_file(&sock);
        let listener = match std::os::unix::net::UnixListener::bind(&sock) {
            Ok(l) => l,
            Err(e) => {
                // P2-B：bind 失败用 error 级别——Ask 权限闭环将完全不可用（降级为
                // 无审批），是安全 posture 退化，需显著告警而非静默 warn。
                error!(
                    target: "imagent::core",
                    sock = %sock,
                    error = %e,
                    "bind permission socket 失败：Ask 权限闭环不可用（降级为无审批，安全 posture 退化）"
                );
                return;
            }
        };
        // 转为非阻塞，包进 tokio。
        listener.set_nonblocking(true).ok();
        let listener = match tokio::net::UnixListener::from_std(listener) {
            Ok(l) => l,
            Err(e) => {
                warn!(target: "imagent::core", error = %e, "from_std permission socket failed");
                return;
            }
        };
        // chmod 0600：只允许 owner（本进程同 uid）连接。父目录 ~/.imagent 应为 0700（由 store 保证）。
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)) {
            warn!(
                target: "imagent::core",
                sock = %sock,
                error = %e,
                "chmod permission socket 0600 失败，Ask 权限闭环鉴权减弱"
            );
        }
        // P5-9b：握手 token——同 uid 进程裸 connect 即可伪造 conv_id 推送审批请求
        //（P2-7 残余）。token 随机生成并写 <sock_dir>/permission.token（0600），MCP
        // 子进程（claude 经 --mcp-config spawn）读取后在连接首行回传，不符即丢弃。
        // 说明：同 uid 进程仍能从文件/env/cmdline 拿到 token，属提高伪造门槛而非
        // 绝对防护（绝对防护需继承 fd 或抽象命名空间 socket，另行迭代）。
        let token = format!("imagent-perm:{:032x}", rand::random::<u128>());
        let token_path = std::path::Path::new(&sock)
            .parent()
            .map(|d| d.join("permission.token"))
            .unwrap_or_else(|| std::path::PathBuf::from("permission.token"));
        if let Err(e) = std::fs::write(&token_path, &token) {
            error!(
                target: "imagent::core",
                error = %e,
                ?token_path,
                "写 permission.token 失败：所有权限请求将因握手失败被拒（fail-closed）"
            );
        } else if let Err(e) =
            std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600))
        {
            warn!(target: "imagent::core", error = %e, "chmod permission.token 0600 失败");
        }
        // R-2：accept task 监听 shutdown（SIGTERM 时停止 accept，原实现永驻）；
        // 每个连接的 handle_permission_socket 纳入 self.tasks，drain 时一并等待。
        let this = self.clone();
        let expected_token = token;
        tokio::spawn(async move {
            // 鉴权基准：只接受与本进程同 uid 的连接（MCP 子进程由本进程 spawn，必然同 uid）。
            // P2-7/P5-9b 威胁模型：peer_uid 防「跨 uid 伪造」；握手 token 把「同 uid
            // 裸 connect 伪造 conv_id」的门槛从零提高到需读到 token（见上方注释）。
            let expected_uid = current_uid();
            loop {
                tokio::select! {
                    _ = this.shutdown.notified() => {
                        info!(target: "imagent::core", "permission socket accept task 收到 shutdown，停止");
                        break;
                    }
                    res = listener.accept() => match res {
                        Ok((stream, _)) => {
                            match peer_uid(&stream) {
                                Some(uid) if uid == expected_uid => {
                                    let platform = this.platform.clone();
                                    let router = this.router.clone();
                                    let permission_ask_timeout = this.permission_ask_timeout;
                                    let expected_token = expected_token.clone();
                                    this.tasks.lock().await.spawn(async move {
                                        Self::handle_permission_socket(
                                            stream,
                                            platform,
                                            router,
                                            permission_ask_timeout,
                                            expected_token,
                                        )
                                        .await;
                                    });
                                }
                                Some(uid) => {
                                    warn!(
                                        target: "imagent::core",
                                        peer_uid = uid,
                                        expected_uid = expected_uid,
                                        "拒绝非本进程 uid 的权限 socket 连接（疑似伪造）"
                                    );
                                }
                                None => {
                                    warn!(
                                        target: "imagent::core",
                                        "无法获取权限 socket 对端 uid（平台不支持 peer cred），拒绝连接"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            warn!(target: "imagent::core", error = %e, "permission socket accept 失败");
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        }
                    }
                }
            }
        });
    }

    /// 读一行权限 socket 报文（15s 超时 + 64KiB 上限）。None = EOF/超时/超长
    ///（后两者记日志）。
    #[cfg(unix)]
    async fn read_socket_line(
        reader: &mut tokio::io::BufReader<&mut tokio::net::UnixStream>,
    ) -> Option<String> {
        match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            Self::read_line_capped(reader, 64 * 1024),
        )
        .await
        {
            Ok(Ok(line)) => line,
            Ok(Err(e)) => {
                warn!(target: "imagent::core", error = %e, "permission socket 读行失败/超长");
                None
            }
            Err(_) => {
                warn!(target: "imagent::core", "permission socket 读行超时（15s）");
                None
            }
        }
    }

    /// 读一行（到 `\n`），上限 `max_bytes` 字节，超限返 Err（P1-9：防同 uid 进程
    /// 发巨大行 OOM）。返回 None 表示对端 EOF（未发数据即关）。
    #[cfg(unix)]
    pub(crate) async fn read_line_capped<R: tokio::io::AsyncBufRead + Unpin>(
        reader: &mut R,
        max_bytes: usize,
    ) -> std::io::Result<Option<String>> {
        use tokio::io::AsyncBufReadExt;
        let mut buf: Vec<u8> = Vec::with_capacity(512);
        loop {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return if buf.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
                };
            }
            if let Some(nl) = available.iter().position(|&b| b == b'\n') {
                buf.extend_from_slice(&available[..=nl]);
                reader.consume(nl + 1);
                return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
            }
            buf.extend_from_slice(available);
            let n = available.len();
            reader.consume(n);
            if buf.len() > max_bytes {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("permission request line exceeds {max_bytes} bytes"),
                ));
            }
        }
    }

    /// 写一行 JSON 回复到 socket，带写超时（P1-9：防对端不读导致 write_all 长时阻塞）。
    /// best-effort：超时/出错仅返回，连接由调用方 drop。
    #[cfg(unix)]
    async fn write_permission_reply(stream: &mut tokio::net::UnixStream, reply: PermissionReply) {
        use tokio::io::AsyncWriteExt;
        let resp = serde_json::json!({
            "allow": reply.allow,
            "message": reply.message,
        });
        let mut out = resp.to_string();
        out.push('\n');
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let _ = stream.write_all(out.as_bytes()).await;
            let _ = stream.flush().await;
        })
        .await;
    }

    /// 处理单个 socket 连接：读请求行 → send_text 询问 → 等回复 → 写回复。
    ///
    /// - **P1-3**：send_text 失败时回写 deny 并 return（不挂 pending——否则用户看不到
    ///   询问，agent 会卡满 agent_timeout，期间该 conv 消息全被当回复吞）。
    /// - **P1-8**：超时/router-drop 时 `router.cancel` 清理 pending map 残留。
    /// - **P1-9**：读行加上限（64KiB）+ 读超时（15s）+ 写超时（10s），防 OOM / 挂死。
    #[cfg(unix)]
    async fn handle_permission_socket(
        mut stream: tokio::net::UnixStream,
        platform: Arc<dyn Platform>,
        router: Arc<PermissionRouter>,
        permission_ask_timeout: std::time::Duration,
        expected_token: String,
    ) {
        // P5-9b：读两行——首行握手 token、次行 JSON 请求。必须共用一个 BufReader：
        // 分开建会把第二行的数据吞进被丢弃的缓冲区。reader 在块内 drop 以释放
        // stream 借用（后续写回需 &mut stream）。
        let req_line = {
            use tokio::io::BufReader;
            let mut reader = BufReader::new(&mut stream);
            let token_line = Self::read_socket_line(&mut reader).await;
            let Some(token_line) = token_line else {
                return; // EOF，对端未发即关
            };
            if token_line.trim() != expected_token {
                warn!(
                    target: "imagent::core",
                    "权限 socket 握手 token 不符，丢弃连接（疑似同 uid 伪造）"
                );
                return;
            }
            Self::read_socket_line(&mut reader).await
        };
        let Some(line) = req_line else {
            return; // token 对了但没发请求（EOF/超时/超长已记日志）
        };
        let req: serde_json::Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(e) => {
                warn!(target: "imagent::core", raw = %line, error = %e, "permission socket 非 JSON");
                return;
            }
        };
        let conv_id = req
            .get("conv_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let tool_name = req
            .get("tool_name")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let input_str = req.get("input").map(|v| v.to_string()).unwrap_or_default();
        if conv_id.is_empty() {
            return;
        }
        let conv = ConvId(conv_id.clone());
        // P4-4：询问用户——平台支持交互卡片时发「按钮卡片」（send_permission_ask
        // 覆写），否则默认纯文本。按钮点击由平台侧转成 text="y"/"n" 的入站消息，
        // 复用 recv 循环的审批回复路由，core 不感知按钮。
        let input_summary = truncate_str(&input_str, 80);
        // P1-3：发送失败 → 回写 deny 并 return，不挂 pending。
        if let Err(e) = platform
            .send_permission_ask(&conv, &tool_name, &input_summary, &ReplyHint::None)
            .await
        {
            warn!(target: "imagent::core", conv_id = %conv_id, error = %e, "send permission ask 失败，回 deny 不挂 pending");
            Self::write_permission_reply(
                &mut stream,
                PermissionReply {
                    allow: false,
                    message: Some("send_text failed: IM 不可达".into()),
                },
            )
            .await;
            return;
        }
        // 注册 pending，等回复（recv 循环 route 到这里）。
        let rx = router.register(&conv_id).await;
        // P1-G/S-3：权限回复等待独立预算 permission_ask_timeout（默认 300s，不挤占
        // agent_timeout 的执行预算）。agent 死或用户长时间不回复时，超时回 deny 并 drop
        // receiver，避免 pending 永驻把后续消息误当回复吞。
        // P1-8：超时/router-drop 分支显式 cancel，移除 pending map 残留。
        let reply: PermissionReply = match tokio::time::timeout(permission_ask_timeout, rx).await {
            Ok(Ok(r)) => {
                METRICS
                    .permission_decisions
                    .with_label_values(&[if r.allow { "allow" } else { "deny" }])
                    .inc();
                r
            }
            Ok(Err(_)) => {
                router.cancel(&conv_id).await;
                METRICS
                    .permission_decisions
                    .with_label_values(&["dropped"])
                    .inc();
                PermissionReply {
                    allow: false,
                    message: Some("permission router dropped".into()),
                }
            }
            Err(_elapsed) => {
                router.cancel(&conv_id).await;
                METRICS
                    .permission_decisions
                    .with_label_values(&["timeout"])
                    .inc();
                PermissionReply {
                    allow: false,
                    message: Some(format!(
                        "permission ask timed out after {permission_ask_timeout:?}"
                    )),
                }
            }
        };
        // 写回 socket（一行 JSON）。
        Self::write_permission_reply(&mut stream, reply).await;
    }
}

/// 本进程的 uid（peer-uid 鉴权用）。
#[cfg(unix)]
#[allow(unsafe_code)] // crate 顶层 `#![deny(unsafe_code)]`，此处显式豁免
pub(crate) fn current_uid() -> u32 {
    // SAFETY: getuid/geteuid 无参数、无副作用，永远安全。
    // P2-8：Linux SO_PEERCRED 返回对端 real uid → 比对 getuid；
    // macOS LOCAL_PEERCRED 返回 effective uid → 比对 geteuid（避免 setuid 部署下
    // real != effective 导致 Ask 闭环全部误拒、可用性归零）。
    #[cfg(target_os = "macos")]
    {
        unsafe { libc::geteuid() }
    }
    #[cfg(not(target_os = "macos"))]
    {
        unsafe { libc::getuid() }
    }
}

/// 取 UnixStream 对端的 uid（用于权限 socket 鉴权）。
///
/// - Linux: `SO_PEERCRED`
/// - macOS: `LOCAL_PEERCRED`
/// - 其它 unix: 返回 None（调用方应拒绝）。
#[cfg(unix)]
#[allow(unsafe_code)] // crate 顶层 `#![deny(unsafe_code)]`，此处显式豁免
pub(crate) fn peer_uid(stream: &tokio::net::UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    // SAFETY: getsockopt 对已连接的 unix socket 按 optname 填充固定大小的输出缓冲，
    // 传入正确的 len。MaybeUninit/zeroed 避免读取未初始化字段。
    unsafe {
        #[cfg(target_os = "linux")]
        {
            let mut cred: std::mem::MaybeUninit<libc::ucred> = std::mem::MaybeUninit::uninit();
            let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            let rc = libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                cred.as_mut_ptr() as *mut libc::c_void,
                &mut len,
            );
            if rc == 0 {
                Some((*cred.as_ptr()).uid)
            } else {
                None
            }
        }
        #[cfg(target_os = "macos")]
        {
            let mut xucred: libc::xucred = std::mem::zeroed();
            let mut len = std::mem::size_of::<libc::xucred>() as libc::socklen_t;
            let rc = libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERCRED,
                &mut xucred as *mut libc::xucred as *mut libc::c_void,
                &mut len,
            );
            // cr_uid == u32::MAX 表示未填充/无效。
            if rc == 0 && xucred.cr_uid != u32::MAX {
                Some(xucred.cr_uid)
            } else {
                None
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = fd;
            None
        }
    }
}
