//! `imagent join`：新 Mac 网关接入向导（Web 驾驶舱多终端接入，零 token）。
//!
//! 把此前分散的接入流程（手改 config、手生成密钥、手抄公钥到网页管理页
//! 登记）封装成一条命令：收集参数 → 幂等装配密钥 → 合并写 config → 打印
//! 接入凭证 + 登记引导。
//!
//! 1. **收集参数**（缺参交互逐项录入 / `--relay --name --workdir` 全参数
//!    可脚本化）：
//!    - relay：`wss://…/imagent/gateway` 形态（wss://|ws:// 开头且以
//!      /gateway 结尾）；无默认值，参数与交互都无输入即报错退出；
//!    - 机器名：缺省 `hostname -s`（读取失败回退提示输入）；规范化
//!      `[a-zA-Z0-9_-]`（非法字符替换 `-`）、截断 32——与 relay 机器表
//!      （machines.rs）/ SPA 客户端校验同规；空结果报错；打印最终名；
//!    - workdir：绝对路径 + is_dir + `imagent_core::validate_workdir`
//!      （过宽拒绝）；交互校验失败重试 ≤3 次，参数式直接报错。
//! 2. **密钥幂等装配**（接入事实）：认证密钥 `load_or_generate(<imagent_home>/
//!    web-gateway-key)`——已存在即加载复用，**绝不重建**（密钥 = 身份，
//!    重建等于丢掉 relay 机器表已登记的身份）；E2E 密钥 `load_or_generate_e2e
//!    (<imagent_home>/web-e2e-key)`，路径与 `start` 装配处同源（main.rs
//!    build_platform web 分支 / WebPlatform::new）。认证走 ed25519 挑战
//!    应答（prove 验签即上线），新机器无需任何 GATEWAY_TOKEN。
//! 3. **写 config**（`<imagent_home>/config.toml`）：toml 反序列化-改-
//!    序列化合并更新四键（platforms / web_relay_url / web_machine_id /
//!    default_workdir），其余键原样保留。core 的 `Config` 只有 Deserialize
//!    （无 Serialize），故经 `toml::Table` 中转——未知字段也能保住（比
//!    Config 结构体往返更保形）；既有文件解析失败 → 中止不覆盖（提示
//!    手动修复）。
//! 4. **打印接入凭证 + 引导**：64 hex 公钥一行（便于选中复制）+ 指纹
//!    （公钥前 16 hex、4 位分组——relay 机器表 `pubkey_fp` 同款口径，
//!    与网页管理页列表可对照）+「添加机器」登记引导 + E2E TOFU 指纹提示。
//! 5. **依赖检查（警告不阻断）**：claude CLI 不在 PATH 时提示安装。
//!
//! 输出惯例：凭证/引导走 stdout（凭证块连续 println，无日志穿插，公钥
//! 整行可选中复制）；警告（claude 缺失、E2E 降级）走 stderr，不混入。
//! 密钥生成时 `imagent_web::key` 内部的 tracing 提示发生在凭证块之前，
//! 不破坏凭证块的连续性。

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::setup;

/// 交互录入校验失败的最大重试次数（relay / workdir 共用）。
const MAX_PROMPT_ATTEMPTS: usize = 3;

/// 向导入口（main 只做参数接线，全部逻辑在本模块便于单测）。
pub fn run(relay: Option<String>, name: Option<String>, workdir: Option<PathBuf>) -> Result<()> {
    println!("=== imagent join：Web 驾驶舱网关接入向导 ===");
    let home = imagent_core::paths::imagent_home();

    // 1. relay 地址（无默认值：参数或交互，空输入即报错退出）。
    let relay = match relay {
        Some(r) => validate_relay_url(&r)?,
        None => prompt_relay()?,
    };

    // 2. 机器名：参数显式 > 交互（hostname 短名作预填默认，回车接受 / 输入
    //    覆盖——机器名是手机侧栏天天看的标签，hostname 常为「Xs-MacBook-Pro」
    //    形态，接入时应给改名机会；且须与手机「添加机器」登记名一字不差，
    //    relay 按名查公钥验签）。hostname 不可读 → 无默认必填。
    let name = match name {
        Some(n) => normalize_machine_name(&n)?,
        None => prompt_machine_name(hostname_short().as_deref())?,
    };
    println!("机器名：{name}（手机「添加机器」须填完全相同的名字）");

    // 3. 工作目录（绝对路径 + is_dir + 过宽拒绝；参数式直接报错，交互重试 ≤3）。
    let workdir = collect_workdir(workdir.as_deref())?;

    // 4. 密钥幂等装配（认证 ed25519 + E2E X25519；路径与 start 同源）。
    let keys = prepare_keys(&home)?;
    if keys.gateway_fresh {
        println!(
            "已生成网关 ed25519 密钥对（{}，0600）——密钥即本机身份，relay 登记后切勿删除",
            home.join("web-gateway-key").display()
        );
    } else {
        println!(
            "复用既有网关密钥（{}）——已登记身份保持不变",
            home.join("web-gateway-key").display()
        );
    }
    if let Some(err) = &keys.e2e_error {
        eprintln!(
            "⚠️ E2E 密钥装配失败（{err}）：业务帧将明文中继（网关认证不受影响）——与 imagent start 的降级口径一致"
        );
    }

    // 5. 合并写 config（已有则仅更新四键、其余保留；损坏即中止不覆盖）。
    let config_existed = home.join("config.toml").exists();
    let cfg_path = merge_write_config(&home, &relay, &name, &workdir)?;
    if config_existed {
        println!("已合并更新既有 config（join 四键更新，其余键保留）");
    }

    // 6. 接入凭证 + 登记引导（凭证块连续打印在 stdout，便于整块选中复制）。
    println!("\n✅ 配置已写入 {}", cfg_path.display());
    println!("🔑 本机网关公钥（ed25519）：");
    println!("   {}", keys.gateway_pubkey_hex);
    println!("   指纹：{}", keys.gateway_fp_grouped);
    println!(
        "📱 接入最后一步：在任意已登录浏览器 → 侧栏「管理」→ 机器管理 →「添加机器」\n   （名称 {name}，公钥粘贴上方 64 位 hex）→ 通行钥匙验证。"
    );
    match &keys.e2e_fp_grouped {
        Some(fp) => println!(
            "然后运行：imagent start（常驻建议 imagent service install）\n首次连接网页会弹 E2E 指纹确认（本机 E2E 指纹 {fp}），与终端 start 日志核对后信任。"
        ),
        None => println!(
            "然后运行：imagent start（常驻建议 imagent service install）\n首次连接网页会弹 E2E 指纹确认，与终端 start 日志核对后信任。"
        ),
    }

    // 7. 依赖检查（警告不阻断）：claude CLI 缺失提示安装。
    if !claude_cli_in_path() {
        eprintln!(
            "⚠️ 未在 PATH 找到 claude CLI：默认后端 agent=claude-cli 依赖它，imagent start 前请先安装；已改用 codex/gemini 后端可忽略"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 参数收集与校验（纯函数 / 薄 IO，便于单测）
// ---------------------------------------------------------------------------

/// relay 地址校验（纯函数）：trim 后须以 `wss://` 或 `ws://` 开头且以
/// `/gateway` 结尾；非法给出形态提示。合法返回 trim 后的原值。
fn validate_relay_url(raw: &str) -> Result<String> {
    let t = raw.trim();
    let ok = (t.starts_with("wss://") || t.starts_with("ws://")) && t.ends_with("/gateway");
    if !ok {
        return Err(anyhow!(
            "relay 地址格式非法：{t:?}\n正确形态：wss://<ecs>:8443/imagent/gateway（本地调试可 ws://127.0.0.1:<port>/gateway）"
        ));
    }
    Ok(t.to_string())
}

/// 机器名规范化（纯函数）：委托 `imagent_web::sanitize_machine_id`（网关
/// hello 装配同一规则：非法字符替换 `-`、剥首尾 `-`、截断 32）。**二次
/// 收敛**：截断可能残留尾部 `-`，而 start 装配期 `resolve_machine_id` 会对
/// config 值再做一次规范化——这里二次过 sanitize 保证写入值是定点，
/// join 打印的机器名 = start 实际上线的机器名。规范化后为空 → 报错。
fn normalize_machine_name(raw: &str) -> Result<String> {
    let once = imagent_web::sanitize_machine_id(raw.trim()).ok_or_else(|| {
        anyhow!("机器名规范化后为空（输入 {raw:?} 须含 ASCII 字母数字 - _，≤32 字符）")
    })?;
    Ok(imagent_web::sanitize_machine_id(&once).unwrap_or(once))
}

/// hostname 短名（缺省机器名来源；与 web crate 的 default_machine_id 同
/// 数据源、`-s` 直取短名）。读取失败（无 hostname 命令/非零退出/空输出）
/// → None，由调用方回退提示输入。
fn hostname_short() -> Option<String> {
    let out = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// 交互录入 relay（无默认值）：空输入报错退出；非法重试 ≤3 次。
fn prompt_relay() -> Result<String> {
    for _ in 0..MAX_PROMPT_ATTEMPTS {
        let raw = setup::prompt("relay 地址（wss://<ecs>:8443/imagent/gateway）", "")?;
        if raw.is_empty() {
            return Err(anyhow!(
                "relay 地址不能为空（形如 wss://<ecs>:8443/imagent/gateway），join 退出"
            ));
        }
        match validate_relay_url(&raw) {
            Ok(u) => return Ok(u),
            Err(e) => println!("❌ {e}，请重输。"),
        }
    }
    Err(anyhow!(
        "relay 地址连续 {MAX_PROMPT_ATTEMPTS} 次输入非法，join 退出（可参数式重试：imagent join --relay <url>）"
    ))
}

/// 交互录入机器名（hostname 不可用时的回退路径）。
/// 机器名交互：hostname 可用时作预填默认（回车接受，输入覆盖）；不可用则
/// 无默认必填。规范化在返回前统一做。
fn prompt_machine_name(default: Option<&str>) -> Result<String> {
    let raw = match default {
        Some(h) => setup::prompt(
            "机器名（ASCII 字母数字 - _，≤32；须与手机「添加机器」登记名一致）",
            h,
        )?,
        None => setup::prompt(
            "机器名（hostname 不可用，请手输；ASCII 字母数字 - _，≤32）",
            "",
        )?,
    };
    if raw.is_empty() {
        return Err(anyhow!("机器名不能为空，join 退出"));
    }
    normalize_machine_name(&raw)
}

/// 收集工作目录：参数式校验失败直接报错；交互式重试 ≤3 次（校验 =
/// `imagent_core::validate_workdir`：绝对路径 + is_dir + 过宽位置拒绝）。
fn collect_workdir(param: Option<&Path>) -> Result<PathBuf> {
    if let Some(w) = param {
        imagent_core::validate_workdir(w)
            .map_err(|e| anyhow!("工作目录校验失败：{e}（参数式直接报错，交互式可重输）"))?;
        return Ok(w.to_path_buf());
    }
    for _ in 0..MAX_PROMPT_ATTEMPTS {
        let raw = setup::prompt("工作目录（绝对路径，如 /Users/me/proj）", "")?;
        if raw.is_empty() {
            return Err(anyhow!("工作目录不能为空，join 退出"));
        }
        match imagent_core::validate_workdir(Path::new(&raw)) {
            Ok(()) => return Ok(PathBuf::from(raw)),
            Err(e) => println!("❌ {e}，请重输。"),
        }
    }
    Err(anyhow!(
        "工作目录连续 {MAX_PROMPT_ATTEMPTS} 次校验失败，join 退出（可参数式重试：imagent join --workdir <绝对路径>）"
    ))
}

// ---------------------------------------------------------------------------
// 密钥幂等装配（复用 imagent_web::key，绝不重建）
// ---------------------------------------------------------------------------

/// join 装配产物：凭证打印所需（密钥本体不出本模块）。
struct JoinKeys {
    /// 网关 ed25519 公钥（64 hex 一行，登记用）。
    gateway_pubkey_hex: String,
    /// 公钥前 16 hex、4 位分组（relay 机器表 pubkey_fp 同款口径）。
    gateway_fp_grouped: String,
    /// 认证密钥是否本次新生成（提示用）。
    gateway_fresh: bool,
    /// E2E（X25519 gk）指纹：SHA-256 前 16 hex、4 位分组（SPA 确认弹层
    /// 同款展示）；装配失败为 None。
    e2e_fp_grouped: Option<String>,
    /// E2E 装配失败原因（降级不阻断，由调用方打 stderr 警告）。
    e2e_error: Option<String>,
}

/// 幂等装配两把密钥（路径与 `start` 装配处逐一对应）：
/// - 认证：`<home>/web-gateway-key`（main.rs build_platform web 分支同款）；
/// - E2E：`<home>/web-e2e-key`（WebPlatform::new 同款）。
///
/// 认证密钥失败（文件损坏且无法生成）→ Err 中止（打印的公钥就是接入凭证，
/// 加载不出就不能往下走）；E2E 失败 → 降级带回错误（start 的 new_with_e2e
/// 同款取舍：内容保密增强失败不阻断认证接入）。
fn prepare_keys(home: &Path) -> Result<JoinKeys> {
    let gateway_path = home.join("web-gateway-key");
    let gateway_fresh = !gateway_path.exists();
    let sk = imagent_web::key::load_or_generate(&gateway_path).map_err(|e| {
        anyhow!("网关认证密钥装配失败（{e}）——已登记身份的密钥文件绝不重建，请按错误指引修复后重试")
    })?;
    let gateway_pubkey_hex = hex::encode(sk.verifying_key().to_bytes());
    let gateway_fp_grouped = group_hex4(&gateway_pubkey_hex[..16]);

    let e2e_path = home.join("web-e2e-key");
    let (e2e_fp_grouped, e2e_error) = match imagent_web::key::load_or_generate_e2e(&e2e_path) {
        Ok((_, public)) => (
            Some(group_hex4(&imagent_web::key::e2e_fingerprint(&public))),
            None,
        ),
        Err(e) => (None, Some(e)),
    };
    Ok(JoinKeys {
        gateway_pubkey_hex,
        gateway_fp_grouped,
        gateway_fresh,
        e2e_fp_grouped,
        e2e_error,
    })
}

/// 16 hex → 4 位一组（`a1b2c3…` → `a1b2 c3d4 …`；与 SPA 指纹展示一致）。
fn group_hex4(raw: &str) -> String {
    raw.chars()
        .collect::<Vec<_>>()
        .chunks(4)
        .map(|g| g.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// config 合并写入
// ---------------------------------------------------------------------------

/// 合并写 `<home>/config.toml`：读原文 → `toml::Table` 反序列化 → 更新
/// join 四键 → 序列化写回（0600）。已有文件的其余键（含本程序不认识的
/// 未知键）原样保留；文件不存在按空表起写；**解析失败 → 中止不覆盖**
/// （提示手动修复，防把手工调好的配置清零）。返回 config 路径。
fn merge_write_config(home: &Path, relay: &str, name: &str, workdir: &Path) -> Result<PathBuf> {
    let cfg_path = home.join("config.toml");
    std::fs::create_dir_all(home)?;
    let mut table: toml::Table = match std::fs::read_to_string(&cfg_path) {
        Ok(raw) => toml::from_str(&raw).map_err(|e| {
            anyhow!(
                "已有 config.toml 解析失败（{}）：{e}\njoin 已中止且未改写该文件——请先手动修复（或备份后删除重建），再重新运行 join",
                cfg_path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(e) => return Err(anyhow!("读取 {} 失败：{e}", cfg_path.display())),
    };
    table.insert(
        "platforms".into(),
        toml::Value::Array(vec![
            toml::Value::String("web".into()),
            toml::Value::String("local".into()),
        ]),
    );
    table.insert(
        "web_relay_url".into(),
        toml::Value::String(relay.to_string()),
    );
    table.insert(
        "web_machine_id".into(),
        toml::Value::String(name.to_string()),
    );
    table.insert(
        "default_workdir".into(),
        toml::Value::String(workdir.to_string_lossy().into_owned()),
    );
    let out =
        toml::to_string(&table).map_err(|e| anyhow!("config 序列化失败：{e}（未写入磁盘）"))?;
    std::fs::write(&cfg_path, out).map_err(|e| anyhow!("写 {} 失败：{e}", cfg_path.display()))?;
    // config 可能含 secret（wecom_secret 等），与 setup / profile import 同款 0600。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(cfg_path)
}

// ---------------------------------------------------------------------------
// 依赖检查
// ---------------------------------------------------------------------------

/// claude CLI 是否在 PATH（扫描 PATH 目录下的可执行文件，不 spawn 子进程）。
fn claude_cli_in_path() -> bool {
    let Ok(path_var) = std::env::var("PATH") else {
        return false;
    };
    path_var.split(':').any(|dir| {
        let candidate = Path::new(dir).join("claude");
        candidate.is_file() && is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_p: &Path) -> bool {
    true
}

// ---------------------------------------------------------------------------
// 单元测试（任务说明 §测试：机器名 / relay URL / config 写入 / 密钥幂等）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 每用例独立临时 home（进程 id + 原子计数防并发撞名）。
    fn temp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "imagent-join-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 机器名规范化：非法字符替换、截断 32、空/全非法报错；写入值是
    /// sanitize 定点（start 装配期再规范化不漂移——join 打印名 = 上线名）。
    #[test]
    fn machine_name_normalization() {
        assert_eq!(normalize_machine_name("home-mac").unwrap(), "home-mac");
        assert_eq!(
            normalize_machine_name("  Uzziahs MacBook Pro ").unwrap(),
            "Uzziahs-MacBook-Pro",
            "首尾空白剥除、内部非法字符替换 -"
        );
        assert_eq!(
            normalize_machine_name("mac.book@01").unwrap(),
            "mac-book-01"
        );
        let long = normalize_machine_name(&"m".repeat(64)).unwrap();
        assert_eq!(long.chars().count(), 32, "截断到 32 char");
        // 空结果报错：空串 / 纯空白 / 全标点 / 非 ASCII。
        assert!(normalize_machine_name("").is_err());
        assert!(normalize_machine_name("   ").is_err());
        assert!(normalize_machine_name("...").is_err());
        assert!(
            normalize_machine_name("家").is_err(),
            "非 ASCII 替换为 - 后为空"
        );
        // 定点：33 字符输入截断后可能残留尾部 `-`，产出须再规范化不变。
        let edge = format!("a{}-c", "b".repeat(30)); // a + 30b + -c = 33 char
        let n = normalize_machine_name(&edge).unwrap();
        assert!(!n.ends_with('-'), "截断残留尾 - 已二次收敛：{n}");
        assert_eq!(
            imagent_web::sanitize_machine_id(&n).as_deref(),
            Some(n.as_str()),
            "写入值是 sanitize 定点（start 再规范化不漂移）"
        );
    }

    /// hostname 缺省路径：`hostname -s` 可得且规范化为合法机器名
    /// （unix 开发/CI 机常态；读取失败路径由 None 回退提示，不在单测模拟）。
    #[test]
    fn hostname_default_available() {
        let h = hostname_short().expect("hostname -s 应可得");
        assert!(!h.is_empty());
        assert!(
            normalize_machine_name(&h).is_ok(),
            "hostname 应规范化为合法机器名：{h}"
        );
    }

    /// relay URL 校验正反例（wss://|ws:// 开头且 /gateway 结尾；首尾空白
    /// 剥除后原样返回）。
    #[test]
    fn relay_url_validation() {
        for ok in [
            "wss://ecs.example.com:8443/imagent/gateway",
            "ws://127.0.0.1:9000/gateway",
        ] {
            assert!(validate_relay_url(ok).is_ok(), "应合法：{ok}");
        }
        for bad in [
            "",                                 // 空
            "https://ecs:8443/imagent/gateway", // 非 ws(s) scheme
            "wss://ecs:8443/imagent",           // 缺 /gateway 尾
            "wss://ecs:8443/gateway/x",         // /gateway 非结尾
            "imagent/gateway",                  // 无 scheme
        ] {
            assert!(validate_relay_url(bad).is_err(), "应拒绝：{bad}");
        }
        // 首尾空白剥除；错误信息含形态提示。
        assert_eq!(
            validate_relay_url("  wss://ecs:8443/imagent/gateway\n").unwrap(),
            "wss://ecs:8443/imagent/gateway"
        );
        let err = validate_relay_url("https://x/gateway")
            .unwrap_err()
            .to_string();
        assert!(err.contains("wss://") && err.contains("/gateway"), "{err}");
    }

    /// 全新 home：四键写入（值逐项断言）+ 0600 权限 + 返回路径正确。
    #[test]
    fn config_write_fresh_home() {
        let home = temp_home("fresh");
        let cfg = merge_write_config(
            &home,
            "wss://ecs:8443/imagent/gateway",
            "home-mac",
            Path::new("/Users/x/proj"),
        )
        .unwrap();
        assert_eq!(cfg, home.join("config.toml"));
        let raw = std::fs::read_to_string(&cfg).unwrap();
        let t: toml::Table = toml::from_str(&raw).unwrap();
        let platforms: Vec<&str> = t["platforms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(platforms, vec!["web", "local"]);
        assert_eq!(
            t["web_relay_url"].as_str(),
            Some("wss://ecs:8443/imagent/gateway")
        );
        assert_eq!(t["web_machine_id"].as_str(), Some("home-mac"));
        assert_eq!(t["default_workdir"].as_str(), Some("/Users/x/proj"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "config 写入即 0600");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 已有 config 合并更新：join 四键更新，其余键（含未知键）保留。
    #[test]
    fn config_write_merges_existing() {
        let home = temp_home("merge");
        std::fs::write(
            home.join("config.toml"),
            concat!(
                "default_workdir = \"/old/ws\"\n",
                "platforms = [\"feishu\"]\n",
                "web_relay_url = \"wss://old:1/imagent/gateway\"\n",
                "allowed_senders = [\"feishu:ou_1\"]\n",
                "wecom_bot_id = \"bot-1\"\n",
                "custom_future_key = 42\n",
            ),
        )
        .unwrap();
        merge_write_config(
            &home,
            "wss://new:8443/imagent/gateway",
            "mini-m2",
            Path::new("/new/ws"),
        )
        .unwrap();
        let t: toml::Table =
            toml::from_str(&std::fs::read_to_string(home.join("config.toml")).unwrap()).unwrap();
        // 四键更新。
        assert_eq!(
            t["web_relay_url"].as_str(),
            Some("wss://new:8443/imagent/gateway")
        );
        assert_eq!(t["web_machine_id"].as_str(), Some("mini-m2"));
        assert_eq!(t["default_workdir"].as_str(), Some("/new/ws"));
        let platforms: Vec<&str> = t["platforms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(platforms, vec!["web", "local"]);
        // 其余键保留（白名单 / 既有平台凭据 / 未知键）。
        let senders: Vec<&str> = t["allowed_senders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(senders, vec!["feishu:ou_1"]);
        assert_eq!(t["wecom_bot_id"].as_str(), Some("bot-1"));
        assert_eq!(t["custom_future_key"].as_integer(), Some(42));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 损坏 config：中止且不覆盖（文件内容原样，错误提示手动修复）。
    #[test]
    fn config_write_corrupt_aborts_without_overwrite() {
        let home = temp_home("corrupt");
        let bad = "this is [ not = toml";
        std::fs::write(home.join("config.toml"), bad).unwrap();
        let err = merge_write_config(
            &home,
            "wss://ecs:8443/imagent/gateway",
            "m",
            Path::new("/w"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("解析失败") && err.contains("未改写"),
            "错误应说明中止且未覆盖：{err}"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            bad,
            "损坏文件须原样保留"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 密钥幂等：重复 prepare（=重复 join）认证密钥文件内容与 mtime 均不变、
    /// 公钥不变（身份稳定）；E2E 密钥同源路径落盘且指纹稳定。
    #[test]
    fn gateway_key_idempotent_across_repeated_join() {
        let home = temp_home("keys");
        let first = prepare_keys(&home).unwrap();
        assert!(first.gateway_fresh, "首次应生成");
        assert_eq!(first.gateway_pubkey_hex.len(), 64, "公钥 64 hex");
        let key = home.join("web-gateway-key");
        let content1 = std::fs::read_to_string(&key).unwrap();
        let mtime1 = std::fs::metadata(&key).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let second = prepare_keys(&home).unwrap();
        assert!(!second.gateway_fresh, "二次应复用（绝不重建）");
        assert_eq!(
            std::fs::read_to_string(&key).unwrap(),
            content1,
            "密钥文件内容不变"
        );
        assert_eq!(
            std::fs::metadata(&key).unwrap().modified().unwrap(),
            mtime1,
            "mtime 不变（未被重写）"
        );
        assert_eq!(
            second.gateway_pubkey_hex, first.gateway_pubkey_hex,
            "公钥不变"
        );
        // E2E 密钥：同源路径（start 的 WebPlatform::new 同款文件名）+ 指纹稳定。
        let e2e = home.join("web-e2e-key");
        assert!(e2e.exists(), "E2E 密钥落盘路径与 start 同源");
        let e2e_content1 = std::fs::read_to_string(&e2e).unwrap();
        let _ = prepare_keys(&home).unwrap();
        assert_eq!(std::fs::read_to_string(&e2e).unwrap(), e2e_content1);
        assert!(second.e2e_fp_grouped.is_some(), "E2E 指纹可得");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// E2E 密钥损坏：降级不阻断（与 start 同款取舍）——认证密钥照常装配，
    /// e2e 指纹 None + 错误原因带回。
    #[test]
    fn e2e_corrupt_degrades_without_blocking() {
        let home = temp_home("e2e-bad");
        std::fs::write(home.join("web-e2e-key"), "garbage!").unwrap();
        let keys = prepare_keys(&home).unwrap();
        assert!(keys.gateway_fresh, "认证密钥照常生成");
        assert_eq!(keys.gateway_pubkey_hex.len(), 64);
        assert!(keys.e2e_fp_grouped.is_none(), "E2E 指纹应缺失");
        assert!(keys.e2e_error.is_some(), "降级原因应带回");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 指纹分组：16 hex → 4 位一组（空串安全）。
    #[test]
    fn fingerprint_grouping() {
        assert_eq!(group_hex4("a1b2c3d4e5f60718"), "a1b2 c3d4 e5f6 0718");
        assert_eq!(group_hex4(""), "");
    }
}
