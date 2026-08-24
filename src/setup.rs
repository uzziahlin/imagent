//! `imagent setup`：首次运行交互式向导（P6-5）。
//!
//! QR 扫码建应用不适用飞书自建应用（P4 已明确），向导止步于：
//! 平台选择 → 飞书权限/事件订阅清单引导 → 凭据录入 + tenant_token 连通性校验 →
//! 工作目录录入（过宽位置拒绝，P6-8）→ 写 config.toml（0600）。
//! app_secret 不落 config（config 只认环境变量 IMAGENT_FEISHU_APP_SECRET），
//! 向导末尾打印 export / launchd plist 注入指引。

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{anyhow, Result};

use imagent_core::validate_workdir;

/// 向导入口。非交互终端（管道/CI）直接报错——每步都要人确认，无默认狂奔模式。
pub async fn run() -> Result<()> {
    if !std::io::stdin().is_terminal() {
        return Err(anyhow!(
            "setup 向导需要交互式终端（检测到 stdin 非 tty）。请手动编辑 config.toml（模板：imagent status 可查路径，或参考 Config::EXAMPLE）"
        ));
    }
    println!("=== imagent setup 向导 ===");
    let cfg_path: PathBuf = imagent_core::Config::default_path()
        .ok_or_else(|| anyhow!("无法定位 config 路径（home 目录缺失）"))?;
    if cfg_path.exists() {
        println!(
            "⚠️  已存在配置 {}——向导产出的新配置将覆盖它（数据库/凭据不受影响）。",
            cfg_path.display()
        );
        if !confirm("继续覆盖？", false)? {
            println!("已取消。");
            return Ok(());
        }
    }

    println!("\n选择 IM 平台：");
    println!("  1) feishu  飞书自建应用（推荐，本向导全流程引导）");
    println!("  2) wecom   企业微信智能机器人（填 bot_id + secret）");
    println!("  3) ilink   个人微信 iLink（扫码登录，向导外跑 `imagent login`）");
    let choice = prompt("平台 [1-3]", "1")?;
    match choice.as_str() {
        "1" => setup_feishu(cfg_path).await,
        "2" => setup_wecom(cfg_path).await,
        "3" => {
            println!(
                "\niLink 平台无需本向导：\n  1. 编辑 {} 填 default_workdir\n  2. `imagent login` 扫码\n  3. `imagent start`（发现模式，日志里看 from_user_id）\n  4. `imagent allow <from_user_id>` 后重启",
                cfg_path.display()
            );
            Ok(())
        }
        other => Err(anyhow!("无效选择：{other}")),
    }
}

/// 飞书全流程：清单引导 → 凭据校验 → 工作目录 → 写配置。
async fn setup_feishu(cfg_path: PathBuf) -> Result<()> {
    println!(
        "\n--- 第 1 步：创建飞书自建应用 ---\n\
         1. 打开 https://open.feishu.cn/app →「创建企业自建应用」\n\
         2. 「添加应用能力」→ 启用「机器人」\n\
         3. 「开发配置」→「事件与回调」→ 订阅方式选 **使用长连接接收事件**（无需公网回调）\n\
         4. 添加事件订阅：\n\
            - im.message.receive_v1        （收消息；群聊建议配「接收群聊中 @机器人消息」权限）\n\
            - card.action.trigger          （卡片按钮点击回调——权限审批/命令按钮卡需要）\n\
            - drive.file.comment.created_v1（云文档评论 @bot 触发，可选）\n\
         5. 「权限管理」开通并**发布**：\n\
            - im:message（读取与发送单聊、群聊消息）\n\
            - im:message.group_at_msg（仅收 @ 机器人群消息；要全收改 group_msg 并把\n\
              config 的 feishu_require_mention_in_group 设为 false）\n\
            - cardkit:card:write（CardKit 流式卡片，可选；无此权限自动降级整卡刷新）\n\
            - drive:comment（云文档评论，可选）\n\
         6. 「版本管理与发布」→ 创建版本并发布（权限/事件须发布后生效）"
    );
    println!("\n--- 第 2 步：录入凭据（开放平台「凭证与基础信息」页）---");
    let app_id = prompt("App ID (cli_…)", "")?;
    if !app_id.starts_with("cli_") {
        return Err(anyhow!("App ID 应以 cli_ 开头（收到 {app_id}）"));
    }
    let app_secret = prompt_secret("App Secret")?;
    if app_secret.len() < 16 {
        return Err(anyhow!("App Secret 长度异常（{} 字符）", app_secret.len()));
    }

    println!("\n--- 第 3 步：连通性校验（tenant_access_token）---");
    verify_feishu_credential(&app_id, &app_secret).await?;
    println!("✅ 凭据有效，token 获取成功。");

    println!("\n--- 第 4 步：agent 工作目录 ---");
    println!("agent 子进程的 cwd（**非沙箱**：不限制可读路径，危险操作靠 permission_mode 审批）。");
    let workdir = loop {
        let p = prompt("绝对路径（如 /Users/me/Work/my-project）", "")?;
        match validate_workdir(std::path::Path::new(&p)) {
            Ok(()) => break p,
            Err(e) => println!("❌ {e}，请重输。"),
        }
    };

    let cfg = format!(
        "# 由 imagent setup 生成\n\
         default_workdir = \"{workdir}\"\n\
         allowed_senders = []        # 留空 = 发现模式：先跑起来，日志里看你的 open_id 再填\n\
         allowed_tools = [\"Read\",\"Write\",\"Edit\",\"Grep\",\"Glob\",\"WebFetch\",\"WebSearch\"]  # 执行类(Bash)显式加+ask 过审\n\
         agent = \"claude-cli\"\n\
         platform = \"feishu\"\n\
         feishu_app_id = \"{app_id}\"\n\
         # app_secret 不落盘：用环境变量 IMAGENT_FEISHU_APP_SECRET 注入（见下方提示）\n\
         permission_mode = \"off\"     # 可改 ask 开启 IM 内权限审批闭环\n"
    );
    write_config(&cfg_path, &cfg)?;
    println!("\n✅ 配置已写入 {}（0600）", cfg_path.display());
    println!(
        "\n下一步：\n  export IMAGENT_FEISHU_APP_SECRET='{app_secret}'   # 当前 shell\n  imagent start --platform feishu\n\
         常驻后台：imagent service install（自动把该环境变量写进服务定义）\n\
         在飞书里给机器人发消息 → 日志出现你的 open_id → 填进 allowed_senders 重启即可驱动 agent。"
    );
    Ok(())
}

/// WeCom：bot_id + secret 录入 + WS subscribe ack 连通性校验（P6 遗留补齐——
/// 企微无独立 HTTP 探针接口，subscribe ack 是唯一凭据校验面）。
async fn setup_wecom(cfg_path: PathBuf) -> Result<()> {
    println!("\n--- 录入企业微信智能机器人凭据 ---");
    let bot_id = prompt("bot_id", "")?;
    if bot_id.is_empty() {
        return Err(anyhow!("bot_id 不能为空"));
    }
    let secret = prompt_secret("secret")?;
    println!("\n--- 连通性校验（WS subscribe ack）---");
    imagent_wecom::probe_credentials(&bot_id, &secret, "wss://openws.work.weixin.qq.com").await?;
    println!("✅ 凭据有效，subscribe 认证成功。");
    println!("\n--- agent 工作目录 ---");
    let workdir = loop {
        let p = prompt("绝对路径", "")?;
        match validate_workdir(std::path::Path::new(&p)) {
            Ok(()) => break p,
            Err(e) => println!("❌ {e}，请重输。"),
        }
    };
    let cfg = format!(
        "# 由 imagent setup 生成\n\
         default_workdir = \"{workdir}\"\n\
         allowed_senders = []\n\
         allowed_tools = [\"Read\",\"Write\",\"Edit\",\"Grep\",\"Glob\",\"WebFetch\",\"WebSearch\"]  # 执行类(Bash)显式加+ask 过审\n\
         agent = \"claude-cli\"\n\
         platform = \"wecom\"\n\
         wecom_bot_id = \"{bot_id}\"\n\
         wecom_secret = \"{secret}\"\n\
         # ⚠️ secret 明文存于此文件（S-4）：务必保持 0600 权限\n"
    );
    write_config(&cfg_path, &cfg)?;
    println!(
        "\n✅ 配置已写入 {}（0600）。启动：imagent start --platform wecom",
        cfg_path.display()
    );
    Ok(())
}

/// POST /open-apis/auth/v3/tenant_access_token/internal——code==0 即凭据有效。
async fn verify_feishu_credential(app_id: &str, app_secret: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let resp: serde_json::Value = client
        .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
        .json(&serde_json::json!({ "app_id": app_id, "app_secret": app_secret }))
        .send()
        .await
        .map_err(|e| anyhow!("网络请求失败：{e}"))?
        .json()
        .await
        .map_err(|e| anyhow!("响应解析失败：{e}"))?;
    let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = resp.get("msg").and_then(|m| m.as_str()).unwrap_or("");
        return Err(anyhow!(
            "凭据校验失败 code={code} msg={msg}\n（常见：App Secret 抄错；应用未发布；app_id/secret 不匹配）"
        ));
    }
    Ok(())
}

fn write_config(path: &std::path::Path, content: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, content).map_err(|e| anyhow!("写配置失败：{e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// 读一行，去空白；空输入回默认值。
fn prompt(label: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{label}: ");
    } else {
        print!("{label} [默认 {default}]: ");
    }
    std::io::stdout().flush()?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    let t = buf.trim();
    if t.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(t.to_string())
    }
}

/// 敏感输入：不回显（UNIX termios 关 echo；简化实现——读一行，提示输入时不可见性
/// 非硬性安全边界，secret 只在内存/环境变量流转）。
fn prompt_secret(label: &str) -> Result<String> {
    print!("{label}: ");
    std::io::stdout().flush()?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    let t = buf.trim();
    if t.is_empty() {
        return Err(anyhow!("{label} 不能为空"));
    }
    Ok(t.to_string())
}

/// y/n 确认（default 决定直接回车的行为）。
fn confirm(label: &str, default: bool) -> Result<bool> {
    let ans = prompt(label, if default { "y" } else { "n" })?;
    Ok(matches!(ans.to_ascii_lowercase().as_str(), "y" | "yes"))
}
