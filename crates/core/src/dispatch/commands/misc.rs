//! 状态 / 环境类命令（自检、工作目录、工作空间、媒体、帮助）。

use super::*;

impl Dispatcher {
    /// /status —— 本会话 + 全局运行状态。
    pub(super) async fn cmd_status(&self, conv: &ConvId, hint: &ReplyHint) {
        // P4-7：本会话 + 全局运行状态。
        let running_here = self.running.lock().await.contains_key(&conv.0);
        let queued_here = self
            .queues
            .lock()
            .await
            .get(&conv.0)
            .map(|q| q.len())
            .unwrap_or(0);
        let in_flight = self.running.lock().await.len();
        let wd = self.resolve_workdir(&conv.0).await;
        let name_key = active_name_key(&conv.0);
        let (sess, active) = tokio::join!(
            self.store.get_session(&conv.0),
            self.store.get_config(&name_key)
        );
        let sess_desc = match sess {
            Ok(Some(row)) => {
                let name = active.ok().flatten().unwrap_or_default();
                let label = if name.is_empty() {
                    "未命名".to_string()
                } else {
                    name
                };
                format!(
                    "{label}（{}…，{}）",
                    &row.session_id[..row.session_id.len().min(12)],
                    row.agent_kind
                )
            }
            _ => "无（下条消息新建）".to_string(),
        };
        let text = format!(
                            "📊 状态\n- 平台/后端：{} / {}\n- 本会话：{}，排队 {} 条\n- 会话：{sess_desc}\n- 工作目录：{}\n- 全局在飞任务：{in_flight}\n- 运行时长：{}",
                            self.platform.name(),
                            self.backend.name(),
                            if running_here { "任务在跑" } else { "无任务" },
                            queued_here,
                            wd.display(),
                            format_uptime(self.started_at.elapsed()),
                        );
        self.reply(conv, &text, hint).await;
    }

    /// /doctor —— 自检（workdir/store/在飞任务）。
    pub(super) async fn cmd_doctor(&self, conv: &ConvId, hint: &ReplyHint) {
        // P4-7：自检——workdir / store / 后端 / 在飞任务。
        let mut lines = Vec::new();
        let wd = self.resolve_workdir(&conv.0).await;
        match std::fs::metadata(&wd) {
            Ok(m) if m.is_dir() => lines.push(format!("✅ 工作目录可用：{}", wd.display())),
            Ok(_) => lines.push(format!("⚠️ 工作目录不是目录：{}", wd.display())),
            Err(e) => lines.push(format!("⚠️ 工作目录不可访问：{}（{e}）", wd.display())),
        }
        // store 写读回环（config KV）。
        let probe_key = format!("doctor_probe:{}", now_secs());
        match self.store.set_config(&probe_key, "1").await {
            Ok(()) => match self.store.get_config(&probe_key).await {
                Ok(Some(v)) if v == "1" => lines.push("✅ 存储读写正常（SQLite）".into()),
                _ => lines.push("⚠️ 存储读回异常".into()),
            },
            Err(e) => lines.push(format!("⚠️ 存储写入失败：{e}")),
        }
        let _ = self.store.delete_config(&probe_key).await;
        let n_sess = self.store.count_sessions().await.unwrap_or(-1);
        if n_sess >= 0 {
            lines.push(format!("✅ 会话映射：{n_sess} 条"));
        } else {
            lines.push("⚠️ 会话映射计数失败".into());
        }
        let in_flight = self.running.lock().await.len();
        lines.push(if in_flight == 0 {
            "✅ 无在飞任务".to_string()
        } else {
            format!("ℹ️ 在飞任务 {in_flight} 个（/stop 可中断）")
        });
        lines.push(format!(
            "ℹ️ 平台 {} / 后端 {}（{}）",
            self.platform.name(),
            self.backend.name(),
            if self.platform.supports_streaming_card(conv) {
                "支持流式卡片"
            } else {
                "纯文本"
            }
        ));
        let text = format!("🩺 自检结果：\n{}", lines.join("\n"));
        self.reply(conv, &text, hint).await;
    }

    /// /reconnect —— 强制平台重连。
    pub(super) async fn cmd_reconnect(&self, conv: &ConvId, hint: &ReplyHint) {
        // P4-7：强制平台重连（排查长连接僵死）。
        match self.platform.reconnect().await {
            Ok(()) => {
                self.reply(conv, "🔌 已触发平台重连（后台进行中，稍候生效）。", hint)
                    .await
            }
            Err(e) => {
                self.reply(
                    conv,
                    &format!("⚠️ 重连指令失败：{e}（平台可能不支持，可重启 imagent）"),
                    hint,
                )
                .await
            }
        }
    }

    /// /cd [path] —— 查看/切换 per-conv 工作目录。
    pub(super) async fn cmd_cd(&self, conv: &ConvId, hint: &ReplyHint, parts: &[&str]) {
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
        if arg.is_empty() {
            let wd = self.resolve_workdir(&conv.0).await;
            self.reply(conv, &format!("当前工作目录：{}", wd.display()), hint)
                .await;
            return;
        }
        let p = std::path::Path::new(arg);
        if !p.is_absolute() {
            self.reply(conv, "用法：/cd <绝对路径>（须绝对路径）", hint)
                .await;
            return;
        }
        if !p.is_dir() {
            self.reply(conv, &format!("目录不存在：{arg}"), hint).await;
            return;
        }
        // 改 per-conv workdir：取 conv 锁串行，与在飞 agent task 隔离。
        let _conv_lock = self.acquire_conv_lock(&conv.0).await;
        let _conv_guard = _conv_lock.lock().await;
        match self.store.set_config(&workdir_key(&conv.0), arg).await {
            Ok(_) => {
                // P5 快赢：/resume 列表缓存随 workdir 失效——列表按
                // conv 当前目录扫描，切目录后旧序号指向的是旧目录的
                // 会话（且接管前有 cwd 校验兜底）。
                self.resume_cache.lock().await.remove(&conv.0);
                self.reply(
                    conv,
                    &format!("✅ 工作目录已切到 {arg}（下条消息生效）"),
                    hint,
                )
                .await
            }
            Err(e) => self.reply(conv, &format!("保存失败：{e}"), hint).await,
        }
    }

    /// /ws [list|save|use|remove] —— 命名工作空间管理。
    pub(super) async fn cmd_ws(&self, conv: &ConvId, hint: &ReplyHint, parts: &[&str]) {
        let sub = parts.get(1).map(|s| s.trim()).unwrap_or("");
        let arg = parts.get(2).map(|s| s.trim()).unwrap_or("");
        match sub {
            "" | "list" => match self.store.list_config("workspace:").await {
                Ok(rows) if rows.is_empty() => self.reply(conv, "（暂无命名工作空间）", hint).await,
                Ok(rows) => {
                    let body = rows
                        .iter()
                        .map(|(k, v)| {
                            format!("- {}：{v}", k.strip_prefix("workspace:").unwrap_or(k))
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.reply(conv, &format!("命名工作空间：\n{body}"), hint)
                        .await
                }
                Err(e) => self.reply(conv, &format!("列出失败：{e}"), hint).await,
            },
            "save" => {
                if arg.is_empty() {
                    self.reply(conv, "用法：/ws save <name>", hint).await;
                    return;
                }
                let wd = self.resolve_workdir(&conv.0).await;
                match self
                    .store
                    .set_config(&workspace_key(arg), &wd.to_string_lossy())
                    .await
                {
                    Ok(_) => {
                        self.reply(
                            conv,
                            &format!("✅ 已保存工作空间「{arg}」= {}", wd.display()),
                            hint,
                        )
                        .await
                    }
                    Err(e) => self.reply(conv, &format!("保存失败：{e}"), hint).await,
                }
            }
            "use" => {
                if arg.is_empty() {
                    self.reply(conv, "用法：/ws use <name>", hint).await;
                    return;
                }
                match self.store.get_config(&workspace_key(arg)).await {
                    Ok(Some(path)) => {
                        let p = std::path::Path::new(&path);
                        if !p.is_dir() {
                            self.reply(conv, &format!("目录不存在：{path}"), hint).await;
                            return;
                        }
                        // 改 per-conv workdir：取 conv 锁串行，与在飞 agent task 隔离（同 /cd）。
                        let _conv_lock = self.acquire_conv_lock(&conv.0).await;
                        let _conv_guard = _conv_lock.lock().await;
                        match self.store.set_config(&workdir_key(&conv.0), &path).await {
                            Ok(_) => {
                                // P5-第五批：同 /cd——切目录后失效
                                // /resume 列表缓存（列表按当前目录扫描）。
                                self.resume_cache.lock().await.remove(&conv.0);
                                self.reply(conv, &format!("✅ 已切到「{arg}」（{path}）"), hint)
                                    .await
                            }
                            Err(e) => self.reply(conv, &format!("切换失败：{e}"), hint).await,
                        }
                    }
                    Ok(None) => {
                        self.reply(conv, &format!("无此工作空间：{arg}"), hint)
                            .await
                    }
                    Err(e) => self.reply(conv, &format!("读取失败：{e}"), hint).await,
                }
            }
            "remove" => {
                if arg.is_empty() {
                    self.reply(conv, "用法：/ws remove <name>", hint).await;
                    return;
                }
                match self.store.delete_config(&workspace_key(arg)).await {
                    Ok(_) => {
                        self.reply(conv, &format!("✅ 已删除工作空间「{arg}」"), hint)
                            .await
                    }
                    Err(e) => self.reply(conv, &format!("删除失败：{e}"), hint).await,
                }
            }
            _ => {
                self.reply(
                    conv,
                    "用法：/ws [list|save <name>|use <name>|remove <name>]",
                    hint,
                )
                .await
            }
        }
    }

    /// /img <path> —— 发送 workdir 内图片（路径越界拒绝）。
    pub(super) async fn cmd_img(&self, conv: &ConvId, hint: &ReplyHint, parts: &[&str]) {
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
        if arg.is_empty() {
            self.reply(
                conv,
                "用法：/img <图片路径>（相对当前工作目录或绝对路径）",
                hint,
            )
            .await;
            return;
        }
        let wd = self.resolve_workdir(&conv.0).await;
        let raw = std::path::Path::new(arg);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            wd.join(raw)
        };
        // 安全校验：canonicalize 后必须仍在 workdir 内——与 agent 的
        // Read 权限对齐（能 Read 才能发），防任意路径（~/.ssh 等）外传。
        let wd_real = match wd.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                self.reply(conv, &format!("工作目录不可用：{e}"), hint)
                    .await;
                return;
            }
        };
        let real = match joined.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                self.reply(conv, &format!("文件不存在：{arg}"), hint).await;
                return;
            }
        };
        if !real.starts_with(&wd_real) {
            self.reply(
                conv,
                &format!("拒绝：{arg} 不在当前工作目录内（/cd 可切换）"),
                hint,
            )
            .await;
            return;
        }
        if !real.is_file() {
            self.reply(conv, &format!("不是文件：{arg}"), hint).await;
            return;
        }
        let ext_ok = real
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                matches!(
                    e.to_ascii_lowercase().as_str(),
                    "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
                )
            })
            .unwrap_or(false);
        if !ext_ok {
            self.reply(conv, "仅支持图片（png/jpg/jpeg/gif/webp/bmp）", hint)
                .await;
            return;
        }
        let media = MediaRef {
            kind: "image".to_string(),
            url: real.to_string_lossy().into_owned(),
        };
        match self.platform.send_media(conv, &media, hint).await {
            Ok(()) => self.reply(conv, &format!("✅ 已发送：{arg}"), hint).await,
            Err(e) => self.reply(conv, &format!("发送失败：{e}"), hint).await,
        }
    }

    /// /help —— 命令总表。
    pub(super) async fn cmd_help(&self, conv: &ConvId, hint: &ReplyHint) {
        self.reply(
                            conv,
                            "命令：\n/new 重置会话\n/switch <name> 切命名会话\n/sessions 列会话\n/resume [n] 恢复历史/本机会话\n/compact 压缩上下文\n/cd [path] 切工作目录\n/ws [list|save|use|remove] 命名工作空间\n/img <path> 发图片\n/perm <off|allow|deny|ask> 权限模式\n/stop 中断当前任务\n/config [k v] 查看/热改配置\n/status 状态\n/doctor 自检\n/reconnect 重连\n/allow <id> 授权\n/disallow <id> 撤权\n/chat [allow|deny|list] 会话白名单\n/list 白名单\n/whoami 我的id\n/help 帮助",
                            hint,
                        )
                        .await;
    }
}
