//! T14（v13 运维批 #1）backup 子命令实现：`VACUUM INTO` 一致性快照 + config
//! 副本 + MANIFEST.txt + 保留策略。纯运维产物组装，不含运行时状态；CLI 定义
//! 与子命令分发留在 main。

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

// ---------------------------------------------------------------------------
// T14（v13 运维批 #1）：backup 子命令——VACUUM INTO 一致性快照 + 保留策略
// ---------------------------------------------------------------------------

/// 保留策略：备份根目录内单个 profile 的备份最多保留份数（超出自动清最旧）。
pub(crate) const BACKUP_KEEP: usize = 10;

/// unix 秒 → UTC 年月日时分秒（Howard Hinnant civil_from-days 算法；不引
/// chrono——feishu 侧引是为审批卡本地时间，这里刻意 UTC：备份目录名排序即时
/// 间序、无 DST 歧义，纯函数便于测试拨钟）。
fn utc_civil(unix_secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = unix_secs.div_euclid(86_400);
    let secs = unix_secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = ((doy - (153 * mp + 2) / 5 + 1) as u32).clamp(1, 31);
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (secs / 3600) as u32,
        (secs % 3600 / 60) as u32,
        (secs % 60) as u32,
    )
}

/// 备份目录名的时间戳段：`YYYYMMDD-HHMMSS`（UTC，见 [`utc_civil`]）。
fn backup_timestamp(now: std::time::SystemTime) -> String {
    let secs = match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        // 时钟早于 epoch（系统时钟异常）：取负值继续格式化，fail-open——
        // 备份是运维兜底动作，不应因时钟异常拒绝执行。
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let (y, mo, d, h, mi, s) = utc_civil(secs);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// 文件 sha256（hex）+ 尺寸（字节）。媒体不进快照、产物尺寸有限，整读入
/// 内存可接受（备份是低频运维路径）。
fn sha256_and_size(path: &Path) -> anyhow::Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    let size = bytes.len() as u64;
    Ok((hex::encode(Sha256::digest(&bytes)), size))
}

/// 生成一份备份：单目录 `imagent-backup-<profile>-<YYYYMMDD-HHMMSS>`，内含
/// `imagent.db.snapshot`（VACUUM INTO 快照，先写同盘 tmp 再 rename——对齐
/// 仓内 temp+rename 先例）+ `config.toml` 副本（缺失则仅在 MANIFEST 记录）
/// + `MANIFEST.txt`。返回备份目录路径。
///
/// 原子性：任一步失败即删掉整个新目录——「目录存在 = 备份完整」，避免半截
/// 目录混进保留策略的排序。`now` 由调用方注入（测试拨钟可控）。
pub(crate) async fn run_backup(
    store: &imagent_store::Store,
    data_dir: &Path,
    backup_root: &Path,
    profile: &str,
    now: std::time::SystemTime,
) -> Result<PathBuf> {
    let ts = backup_timestamp(now);
    let unix_secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = backup_root.join(format!("imagent-backup-{profile}-{ts}"));
    if dir.exists() {
        return Err(anyhow!(
            "备份目录已存在（同秒时间戳冲突，稍后重试即可）：{}",
            dir.display()
        ));
    }
    std::fs::create_dir_all(&dir)?;
    // 备份目录 0700：快照含 credentials 表（即便 enc/keyring marker 形态），
    // 与 ~/.imagent 主目录收紧姿态一致（--out 可指到 umask 更宽的位置）。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if let Err(e) = build_backup_artifacts(store, data_dir, &dir, &ts, unix_secs, profile).await {
        // 失败回收整个目录：不留半截备份占保留名额/误导排序。
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e.context(format!("备份产物生成失败（已清理 {}）", dir.display())));
    }
    Ok(dir)
}

/// 组装单份备份的三件产物（快照 / config 副本 / MANIFEST）。
async fn build_backup_artifacts(
    store: &imagent_store::Store,
    data_dir: &Path,
    dir: &Path,
    ts: &str,
    unix_secs: u64,
    profile: &str,
) -> Result<()> {
    use anyhow::Context as _;

    // ① db 快照：VACUUM INTO 同盘 tmp → rename（VACUUM INTO 要求目标不存在，
    // 先清掉上次异常残留）。
    let snapshot = dir.join("imagent.db.snapshot");
    let tmp = dir.join(".imagent.db.snapshot.tmp");
    let _ = std::fs::remove_file(&tmp);
    let schema_version = store
        .backup_snapshot(&tmp)
        .await
        .context("VACUUM INTO 快照失败")?;
    std::fs::rename(&tmp, &snapshot).context("快照 rename 进备份目录失败")?;
    tighten_backup_file(&snapshot)?;

    // ② config.toml 副本（可能尚不存在——首次 setup 前；缺失不阻断，MANIFEST
    // 如实记录）。fs::copy 会带上源文件权限，再统一收紧 0600（副本可能含
    // wecom_secret 明文，与 profile export 产物同姿态）。
    let config_src = data_dir.join("config.toml");
    let config_dst = dir.join("config.toml");
    if config_src.is_file() {
        std::fs::copy(&config_src, &config_dst)
            .with_context(|| format!("复制 config.toml 失败（{}）", config_src.display()))?;
        tighten_backup_file(&config_dst)?;
    }
    let snapshot_info = sha256_and_size(&snapshot)?;
    let config_info = if config_dst.is_file() {
        Some(sha256_and_size(&config_dst)?)
    } else {
        None
    };

    // ③ MANIFEST.txt。
    let manifest = build_manifest(
        env!("CARGO_PKG_VERSION"),
        schema_version,
        ts,
        unix_secs,
        profile,
        &snapshot_info,
        config_info.as_ref(),
    );
    let manifest_path = dir.join("MANIFEST.txt");
    std::fs::write(&manifest_path, manifest)?;
    tighten_backup_file(&manifest_path)?;
    Ok(())
}

/// 备份产物统一收紧 0600（仅 unix）。VACUUM INTO 新建文件按 umask（典型
/// 0644）落盘世界可读；快照含 credentials 表、config 副本可能含 secret。
#[cfg(unix)]
fn tighten_backup_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
#[cfg(not(unix))]
fn tighten_backup_file(_path: &Path) -> Result<()> {
    Ok(())
}

/// MANIFEST.txt 内容。keyring 不在快照内的提示必须醒目（换机恢复后第一件事
/// 就是重录凭据，漏了会以为「恢复失败」）。
fn build_manifest(
    version: &str,
    schema_version: i64,
    ts: &str,
    unix_secs: u64,
    profile: &str,
    snapshot: &(String, u64),
    config_copy: Option<&(String, u64)>,
) -> String {
    let mut m = String::new();
    m.push_str("imagent 备份清单（MANIFEST）\n");
    m.push_str("======================================================================\n\n");
    m.push_str(&format!("生成时间    : {ts} UTC（unix {unix_secs}）\n"));
    m.push_str(&format!("imagent 版本 : {version}\n"));
    m.push_str(&format!(
        "schema 版本  : store user_version = {schema_version}\n"
    ));
    m.push_str(&format!("profile      : {profile}\n\n"));
    m.push_str("文件清单（sha256）\n");
    m.push_str("----------------------------------------------------------------------\n");
    m.push_str(&format!(
        "  imagent.db.snapshot  {} 字节\n    sha256={}\n",
        snapshot.1, snapshot.0
    ));
    match config_copy {
        Some((hash, size)) => m.push_str(&format!(
            "  config.toml          {size} 字节\n    sha256={hash}\n"
        )),
        None => m.push_str("  config.toml          缺失（备份时无 config，未复制）\n"),
    }
    m.push_str("\n⚠️ keyring 凭据不在本快照内（换机/重装后必须重录）\n");
    m.push_str("----------------------------------------------------------------------\n");
    m.push_str(
        "keyring（iLink 登录态等）存放在 OS 钥匙串，机器绑定、不随快照迁移：\n\
         \x20 - iLink : `imagent login` 重新扫码登录\n\
         \x20 - WeCom : 重配 config.toml 的 wecom_bot_id / wecom_secret\n\
         \x20 - 飞书  : 重设环境变量 IMAGENT_FEISHU_APP_SECRET\n\n",
    );
    m.push_str("媒体目录不进快照\n");
    m.push_str("----------------------------------------------------------------------\n");
    m.push_str("<imagent_home>/media 为入站媒体缓存（可重新获取、体积不可控），未纳入快照。\n\n");
    m.push_str("恢复步骤\n");
    m.push_str("----------------------------------------------------------------------\n");
    m.push_str(
        "1. 停止 imagent（start 终端 Ctrl-C / `imagent service uninstall` / kill）。\n\
         2. cp imagent.db.snapshot <imagent_home>/imagent.db\n\
         \x20  （同时删除旧库残留的 imagent.db-wal / imagent.db-shm，避免旧 WAL 混入新库）\n\
         3. 如有 config.toml 副本：cp config.toml <imagent_home>/config.toml\n\
         4. 重录凭据（见上方 keyring 提示）。\n\
         5. `imagent start`——schema 迁移自动向前（旧快照 + 新版二进制 = 自动迁移）。\n\n",
    );
    m.push_str("备份语义与限制\n");
    m.push_str("----------------------------------------------------------------------\n");
    m.push_str(
        "- 本备份不持实例锁：服务运行中即可执行，无需先停机。db 快照由 SQLite\n\
         \x20 `VACUUM INTO` 生成，包含快照开始前已提交的全部事务（含 WAL 中尚未\n\
         \x20 checkpoint 的已提交数据）；快照开始后新提交的数据不在快照内——这是\n\
         \x20 不停机备份的固有语义，恢复后以快照点为准继续。\n\
         - 降级不可恢复：schema 更新的快照不能被旧版 imagent 打开（启动时拒绝\n\
         \x20 比代码新的 user_version）。恢复请使用与生成时相同或更新的版本；\n\
         \x20 反向（旧快照 + 新二进制）安全，迁移自动向前。\n",
    );
    m
}

/// 保留策略：`backup_root` 下 `imagent-backup-<profile>-` 前缀的目录按名排序
/// （时间戳零填充 → 字典序即时序），仅保留最新 `keep` 份，返回被删目录名
/// （best-effort：单个删除失败跳过不阻断）。其他 profile 的备份、无关目录、
/// 普通文件一律不动。
pub(crate) fn prune_backups(backup_root: &Path, profile: &str, keep: usize) -> Vec<String> {
    let prefix = format!("imagent-backup-{profile}-");
    let Ok(rd) = std::fs::read_dir(backup_root) else {
        return vec![];
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(&prefix))
        .collect();
    names.sort();
    if names.len() <= keep {
        return vec![];
    }
    let excess = names.len() - keep;
    names
        .into_iter()
        .take(excess)
        .filter(|n| std::fs::remove_dir_all(backup_root.join(n)).is_ok())
        .collect()
}

/// backup 命令的终端摘要（取舍与 keyring 提示必须落到用户可见处，不只是文档）。
pub(crate) fn print_backup_summary(dir: &Path, pruned: &[String]) {
    println!("✅ 备份完成：{}", dir.display());
    println!("   ├─ imagent.db.snapshot（VACUUM INTO 一致性快照）");
    let has_config = dir.join("config.toml").is_file();
    println!(
        "   ├─ config.toml（{}）",
        if has_config {
            "副本"
        } else {
            "缺失——备份时无 config，未复制"
        }
    );
    println!("   └─ MANIFEST.txt（sha256 / 恢复步骤 / 限制说明）");
    println!(
        "ℹ️  运行中备份：快照含开始前已提交的全部事务，其后新写入不在内（无需停机；取舍详见 MANIFEST.txt）。"
    );
    println!(
        "⚠️ keyring 凭据（iLink 登录态等）不在快照内——恢复后需 `imagent login` 重录 / 重设 secret（步骤见 MANIFEST.txt）。"
    );
    println!("媒体目录不进快照；backups/ 自动保留最近 {BACKUP_KEEP} 份。");
    if !pruned.is_empty() {
        println!(
            "🗑️ 已自动清理旧备份 {} 份：{}",
            pruned.len(),
            pruned.join(", ")
        );
    }
}

/// T14（v13 运维批 #1）：backup 子命令单测——CLI 解析 / 时间戳 / 产物清单与
/// MANIFEST 内容 / 保留策略 / 失败回收。时间戳全部参数化注入（拨钟可控）。
#[cfg(test)]
mod backup_tests {
    use super::*;
    use crate::{Cli, Cmd};
    use clap::Parser as _;

    /// CLI 解析：`backup` 无参（默认目录）与 `--out <dir>` 两形态。
    #[test]
    fn backup_cli_parse() {
        let c = Cli::try_parse_from(["imagent", "backup"]).expect("bare backup 应可解析");
        assert!(
            matches!(c.cmd, Cmd::Backup { out: None }),
            "缺省不应带 --out"
        );
        let c = Cli::try_parse_from(["imagent", "backup", "--out", "/tmp/bk-out"])
            .expect("--out 应可解析");
        match c.cmd {
            Cmd::Backup { out } => assert_eq!(out, Some(PathBuf::from("/tmp/bk-out"))),
            _ => panic!("期望 Backup 子命令"),
        }
    }

    /// 时间戳格式：UTC 已知值核对（epoch / 2023-11-14 / 2024 闰日），零填充
    /// 保证字典序 = 时间序（保留策略的排序前提）。
    #[test]
    fn backup_timestamp_known_values() {
        use std::time::Duration;
        assert_eq!(backup_timestamp(std::time::UNIX_EPOCH), "19700101-000000");
        let t = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(backup_timestamp(t), "20231114-221320");
        let leap = std::time::UNIX_EPOCH + Duration::from_secs(1_709_164_800);
        assert_eq!(backup_timestamp(leap), "20240229-000000");
    }

    /// 产物清单 + MANIFEST 内容：真 Store 跑一次 run_backup（now 注入固定
    /// 时间），断言三产物存在、MANIFEST 含版本/schema/sha256/keyring 提示/
    /// 恢复步骤/取舍说明，快照内容=快照时刻（备份前写入可见）。
    #[tokio::test]
    async fn backup_artifacts_and_manifest() {
        let root = std::env::temp_dir().join(format!("imagent-bk-art-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data_dir = root.join("home");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("config.toml"), "platform = \"feishu\"\n").unwrap();
        let store = imagent_store::Store::open(&data_dir.join("imagent.db"))
            .await
            .expect("open store");
        store
            .add_allowed_sender("bk-art", None, Some("test"))
            .await
            .unwrap();

        let out_root = root.join("backups");
        let fixed_now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_777_777_777);
        let dir = run_backup(&store, &data_dir, &out_root, "default", fixed_now)
            .await
            .expect("run_backup");

        // 目录名：profile + 注入的时间戳（1777777777 = 2026-05-03 03:09:37 UTC）。
        assert!(
            dir.ends_with("imagent-backup-default-20260503-030937"),
            "{dir:?}"
        );

        // 三产物齐全。
        let snapshot = dir.join("imagent.db.snapshot");
        assert!(snapshot.is_file(), "快照应存在");
        assert!(dir.join("config.toml").is_file(), "config 副本应存在");
        let manifest = std::fs::read_to_string(dir.join("MANIFEST.txt")).unwrap();

        // MANIFEST 关键内容。
        assert!(manifest.contains(env!("CARGO_PKG_VERSION")), "imagent 版本");
        assert!(
            manifest.contains(&format!("user_version = {}", imagent_store::SCHEMA_VERSION)),
            "schema 版本"
        );
        let (hash, size) = sha256_and_size(&snapshot).unwrap();
        assert!(manifest.contains(&hash), "快照 sha256 应记录在 MANIFEST");
        assert!(manifest.contains(&format!("{size} 字节")));
        assert!(manifest.contains("keyring"), "keyring 不在快照的提示");
        assert!(manifest.contains("imagent login"), "重录凭据步骤");
        assert!(manifest.contains("恢复步骤"), "恢复步骤段");
        assert!(manifest.contains("VACUUM INTO"), "一致性取舍说明");
        assert!(manifest.contains("降级不可恢复"), "降级限制说明");
        assert!(manifest.contains("media"), "媒体不进快照说明");

        // 快照内容 = 快照时刻：备份前写入可见（一致性）。
        let snap_store = imagent_store::Store::open(&snapshot)
            .await
            .expect("打开快照");
        let senders = snap_store.list_allowed_senders().await.unwrap();
        assert!(senders.iter().any(|s| s == "bk-art"));

        // 产物权限 0600（unix）——快照含 credentials 表、config 副本可能含 secret。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in ["imagent.db.snapshot", "config.toml", "MANIFEST.txt"] {
                let mode = dir.join(f).metadata().unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{f} 应为 0600");
            }
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// config 缺失：备份仍产出（db 快照为主），MANIFEST 如实记录缺失。
    #[tokio::test]
    async fn backup_without_config_still_works() {
        let root = std::env::temp_dir().join(format!("imagent-bk-nocfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data_dir = root.join("home");
        std::fs::create_dir_all(&data_dir).unwrap();
        let store = imagent_store::Store::open(&data_dir.join("imagent.db"))
            .await
            .expect("open store");
        let dir = run_backup(
            &store,
            &data_dir,
            &root.join("backups"),
            "work",
            std::time::SystemTime::now(),
        )
        .await
        .expect("run_backup");
        assert!(dir.join("imagent.db.snapshot").is_file());
        assert!(!dir.join("config.toml").exists(), "无 config 不应产出副本");
        let manifest = std::fs::read_to_string(dir.join("MANIFEST.txt")).unwrap();
        assert!(manifest.contains("缺失"), "MANIFEST 应记录 config 缺失");
        assert!(
            dir.to_string_lossy().contains("imagent-backup-work-"),
            "目录名带 profile"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 同秒时间戳冲突：第二次报错（不覆盖既有备份）。
    #[tokio::test]
    async fn backup_rejects_duplicate_timestamp() {
        let root = std::env::temp_dir().join(format!("imagent-bk-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data_dir = root.join("home");
        std::fs::create_dir_all(&data_dir).unwrap();
        let store = imagent_store::Store::open(&data_dir.join("imagent.db"))
            .await
            .expect("open store");
        let now = std::time::SystemTime::now();
        let first = run_backup(&store, &data_dir, &root.join("backups"), "default", now)
            .await
            .expect("first");
        let err = run_backup(&store, &data_dir, &root.join("backups"), "default", now)
            .await
            .expect_err("同秒应冲突");
        assert!(err.to_string().contains("冲突"), "{err}");
        // 既有备份原样保留（仍是完整三产物）。
        assert!(first.join("MANIFEST.txt").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 保留策略：12 份 → 只保留最新 10、最旧 2 被删；其他 profile / 无关
    /// 目录 / 同名前缀的普通文件一律不动。
    #[test]
    fn backup_retention_prunes_oldest() {
        let root = std::env::temp_dir().join(format!("imagent-bk-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..12 {
            // 时间戳目录名直接手工构造（不依赖真实时钟）。
            let d = root.join(format!("imagent-backup-default-202601{i:02}-000000"));
            std::fs::create_dir_all(&d).unwrap();
        }
        // 干扰项。
        std::fs::create_dir_all(root.join("imagent-backup-work-20260101-000000")).unwrap();
        std::fs::create_dir_all(root.join("not-a-backup")).unwrap();
        std::fs::write(
            root.join("imagent-backup-default-20260105-000000.txt"),
            b"f",
        )
        .unwrap();

        let removed = prune_backups(&root, "default", BACKUP_KEEP);
        assert_eq!(
            removed,
            vec![
                "imagent-backup-default-20260100-000000".to_string(),
                "imagent-backup-default-20260101-000000".to_string(),
            ],
            "应删最旧 2 份"
        );
        assert!(!root.join("imagent-backup-default-20260101-000000").exists());
        assert!(
            root.join("imagent-backup-default-20260111-000000").exists(),
            "最新保留"
        );
        assert!(
            root.join("imagent-backup-work-20260101-000000").exists(),
            "其他 profile 不动"
        );
        assert!(root.join("not-a-backup").exists(), "无关目录不动");
        assert!(
            root.join("imagent-backup-default-20260105-000000.txt")
                .exists(),
            "普通文件不动"
        );

        // 不超额时不删任何东西；根目录不存在安全返回空。
        assert!(
            prune_backups(&root, "default", BACKUP_KEEP).is_empty(),
            "已恰好 10 份"
        );
        assert!(prune_backups(&root.join("nope"), "default", BACKUP_KEEP).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
