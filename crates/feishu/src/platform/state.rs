//! per-conv 会话状态（`ConvState`）+ 后台巡检（housekeeping）+ 入站媒体落盘。
//!
//! T19 拆分自 platform.rs（纯移动）：`ConvState` 是 drain（入站登记）/ ask
//! （审批复用槽）/ platform（发送锚点）三方共用的唯一 per-conv 状态载体，
//! 独立成模块使其「单表单结构、锁纪律、粗上限淘汰」的不变量有唯一出处；
//! 媒体落盘与 GC 同属**磁盘态**（`~/.imagent/media`），随迁于此。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;
use tracing::{info, warn};

use imagent_core::{CoreError, Result};

use super::ask::AskSlot;
use super::PLATFORM;

/// T13：per 评论者的回复锚点条目（comment_id + LRU 时间戳）。
#[derive(Debug, Clone)]
pub(super) struct CommentAnchorEntry {
    comment_id: String,
    touched: Instant,
}

/// per-conv 评论者锚点表上限（LRU 淘汰最久未活跃——单文档同时活跃的评论者
/// 远低于此；与 ConvState 表的 housekeeping 粗上限独立，此处按 touched 精确 LRU）。
const COMMENT_ANCHOR_CAP: usize = 64;

/// 飞书 Platform 适配器。
///
/// 持有发消息所需的 core 配置 + 凭据 + token 缓存；收消息由后台 WS task 推入
/// inbound channel。token 走 lazy 刷新（不用后台定时 task），避免过期窗口。
/// per-conv 会话状态（v1.18 review「ConvState 收敛」）：原 9 张独立的
/// `conv → X` 表（发起者/评论锚/回复锚/最近入站/话题活跃/卡片尾巴/审批 note/
/// 审批复用槽/下沉标记）收敛为单表单结构——「每 conv 轮次串行」这个此前只能
/// 靠通读推演的隐含不变量自此有唯一载体，锁纪律（单锁、锁内无 IO）与粗上限
/// 淘汰（housekeeping）也只需做一次。字段语义见各字段注释（均原样迁移）。
#[derive(Debug)]
pub(super) struct ConvState {
    /// 最近一次入站消息 sender（轮次发起者近似——审批卡/终止按钮的点击者校验锚）。
    pub(super) sender: Option<String>,
    /// 评论 conv 的**回退**锚点：最近一条评论的 comment_id（轮次发起者无评论
    /// 记录时用它回复——进程重启后 per 评论者表已清、或发起者非评论触发）。
    pub(super) comment_anchor: Option<String>,
    /// T13 回复锚定评论者：评论者 open_id → 其最近评论 id。回复按**轮次发起者**
    /// （[`ConvState::round_initiator`]）取锚——A @bot 提问后 B 抢先评论，A 的
    /// 回答不再被拽到 B 的评论线程下（此前单一「最近评论」锚点的归属错乱）。
    /// 容量 [`COMMENT_ANCHOR_CAP`]，LRU 淘汰（见 [`ConvState::note_comment`]）。
    pub(super) comment_anchors: HashMap<String, CommentAnchorEntry>,
    /// 群 conv 回复锚点（本轮发起消息 id——send_typing 从 last_inbound 提升）。
    pub(super) reply_anchor: Option<String>,
    /// 最近一条入站消息 id（回复锚点候选）。
    pub(super) last_inbound: Option<String>,
    /// 话题免 @ 窗口的最近活跃时刻（窗口判定按 elapsed，过期条目无需清理）。
    pub(super) thread_active_at: Option<Instant>,
    /// 最新卡片平台消息 id（强提醒 urgent_app 的加急对象）。
    pub(super) card_tail: Option<String>,
    /// 审批卡 note 行缓存（排队计数不变不重画）。
    pub(super) ask_note: Option<String>,
    /// 审批/问题卡复用槽（P8-2：下一个询问原地 patch 复用末张卡）。
    pub(super) ask_slot: Option<AskSlot>,
    /// 本轮流式卡发送之后是否发过询问卡（终态「结果下沉」判定，P8-2）。
    pub(super) asks_since_card: bool,
    /// v1.23 发起者锚定：本轮**首条消息**的 sender（dispatch 轮首注入）——
    /// 此前用「最近 sender 近似」，群内 B 发一条 steering/排队即把发起者翻成
    /// B（连坐审批/终止按钮的点击权校验）。None = 无轮次记录（回退 sender）。
    pub(super) round_initiator: Option<String>,
    /// v1.21 LRU：最近活跃时刻（入站消息/评论锚点/发起者更新时刷新）。
    /// housekeeping 超上限时按此排序驱逐最久未活跃的会话（替代 v1.18 的
    /// 「整体选择性驱逐」——活跃会话不再被误伤，评论会话/挂起审批仍豁免）。
    pub(super) last_touched: Instant,
}

impl Default for ConvState {
    fn default() -> Self {
        Self {
            sender: None,
            comment_anchor: None,
            comment_anchors: HashMap::new(),
            reply_anchor: None,
            last_inbound: None,
            thread_active_at: None,
            card_tail: None,
            ask_note: None,
            ask_slot: None,
            asks_since_card: false,
            round_initiator: None,
            last_touched: Instant::now(),
        }
    }
}

impl ConvState {
    /// T13：登记一条入站评论（drain 评论分支调用）——刷新「最近评论」回退锚点
    /// 与评论者本人锚点。超 [`COMMENT_ANCHOR_CAP`] 时 LRU 淘汰最久未活跃的
    /// **其他**评论者（同评论者更新走覆盖，不触发淘汰）。
    pub(super) fn note_comment(&mut self, sender: &str, comment_id: &str) {
        self.comment_anchor = Some(comment_id.to_string());
        if self.comment_anchors.len() >= COMMENT_ANCHOR_CAP
            && !self.comment_anchors.contains_key(sender)
        {
            if let Some(oldest) = self
                .comment_anchors
                .iter()
                .min_by_key(|(_, e)| e.touched)
                .map(|(k, _)| k.clone())
            {
                self.comment_anchors.remove(&oldest);
            }
        }
        self.comment_anchors.insert(
            sender.to_string(),
            CommentAnchorEntry {
                comment_id: comment_id.to_string(),
                touched: Instant::now(),
            },
        );
    }

    /// T13：解析评论回复目标锚点。优先**轮次发起者**的最近评论（A 的回答落回
    /// A 的评论线程；发起者同人多次评论取其最新一条——仍是本人线程）；发起者
    /// 未记录（重启/合成消息）或其无评论记录时，回退「最近一条评论」（与修复
    /// 前行为一致，不更差）。发起者评论被 LRU 淘汰（>64 评论者）同样回退。
    pub(super) fn resolve_comment_anchor(&self, initiator: Option<&str>) -> Option<String> {
        initiator
            .and_then(|s| self.comment_anchors.get(s))
            .map(|e| e.comment_id.clone())
            .or_else(|| self.comment_anchor.clone())
    }
}
/// housekeeping 巡检覆盖的 per-conv 状态表（粗上限淘汰，见 housekeeping_loop）。
/// ConvState 收敛后仅剩这一张。
pub(super) struct HousekeepingMaps {
    pub(super) conv_states: Arc<Mutex<HashMap<String, ConvState>>>,
}

/// v1.18 迭代（深度 review 结构性项）：后台巡检——
/// ① 媒体目录 GC：`~/.imagent/media` 此前只进不出（50MB/文件，长期运行磁盘
///    单调增长）。删除 mtime 超 [`MEDIA_RETENTION`]（7 天）的文件；在飞轮次
///    引用的媒体最长活几小时，7 天余量充足。
/// ② per-conv map 粗上限：锚点/发起者/复用槽等表对不活跃会话的条目无限累积
///    （无 touch 时间戳，做不了精确 LRU）；沿用 thread_active 的「超量整体
///    重置」先例（[`PER_CONV_MAP_CAP`]），最坏影响是旧会话首个新轮次的锚点/
///    标注缺失（cosmetic），换内存有界。
/// 节奏：首扫延迟 10 分钟（单测频繁构造 platform，不让测试进程触碰真实
/// ~/.imagent/media），此后每 24h 一轮。
pub(super) async fn housekeeping_loop(maps: HousekeepingMaps) {
    tokio::time::sleep(std::time::Duration::from_secs(600)).await;
    loop {
        let removed = match tokio::task::spawn_blocking(sweep_media_dir).await {
            Ok(n) => n,
            Err(e) => {
                warn!(target: "feishu", error = %e, "媒体 GC 任务 join 失败");
                0
            }
        };
        if removed > 0 {
            info!(target: "feishu", removed, "媒体目录 GC：删除 {removed} 个超期文件");
        }
        // v1.18 review（agent-1 #4）：选择性驱逐——整体 clear() 有两个非
        // cosmetic 代价：① comment conv 的 comment_anchor 被清后 send_text
        // 直接报「评论线程缺少回复目标」（硬失败，非观感）；② 审批等待中的
        // ask_slot 丢失断掉 note 联动。改为：超限时先驱逐「无挂起审批的非
        // 评论会话」条目；仍超限才整体清空（防御性兜底，实际不可达）。
        {
            let mut states = maps.conv_states.lock().await;
            if states.len() > PER_CONV_MAP_CAP {
                // v1.21 精确 LRU（替代 v1.18 的整体选择性驱逐）：按 last_touched
                // 排序驱逐最久未活跃的条目——活跃会话不再被误伤；评论会话
                //（锚点丢失 = 回复硬失败）与挂起审批（note 联动）仍豁免。
                let exempt = |key: &str, st: &ConvState| {
                    key.starts_with("feishu:comment:")
                        || st
                            .ask_slot
                            .as_ref()
                            .is_some_and(|s| s.pending_req.is_some())
                };
                let over = states.len() - PER_CONV_MAP_CAP;
                let mut victims: Vec<(Instant, String)> = states
                    .iter()
                    .filter(|(k, st)| !exempt(k, st))
                    .map(|(k, st)| (st.last_touched, k.clone()))
                    .collect();
                victims.sort_unstable();
                let n = victims.len().min(over);
                for (_, k) in victims.drain(..n) {
                    states.remove(&k);
                }
                if n > 0 {
                    warn!(target: "feishu", evicted = n, "per-conv 状态表超上限（>{PER_CONV_MAP_CAP}），LRU 驱逐 {n} 个最久未活跃会话（评论会话与挂起审批豁免）");
                }
                // 兜底清空：豁免条目本身超限时表仍不收缩（每文档评论一个
                // conv 键的长跑慢泄漏面）——评论锚丢失是硬失败，但无界增长
                // 更糟；恢复路径：下一条评论事件重建锚点。
                if states.len() > PER_CONV_MAP_CAP {
                    warn!(
                        target: "feishu",
                        remained = states.len(),
                        "LRU 驱逐后仍超上限（豁免条目过多），整体清空兜底"
                    );
                    states.clear();
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(24 * 3600)).await;
    }
}

/// per-conv 状态表粗上限（见 housekeeping_loop 文档；远超单部署常见会话数）。
pub(super) const PER_CONV_MAP_CAP: usize = 1024;
/// 媒体文件保留期（housekeeping GC）。
const MEDIA_RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// 扫描媒体目录删除超期文件，返回删除数（同步 IO，调用方 spawn_blocking）。
fn sweep_media_dir() -> usize {
    match media_dir() {
        Ok(dir) => sweep_media_dir_at(&dir, MEDIA_RETENTION),
        Err(e) => {
            warn!(target: "feishu", error = %e, "媒体 GC：目录不可用");
            0
        }
    }
}

/// [`sweep_media_dir`] 的可测试形态：按 mtime 删除超期**文件**（不动子目录）。
fn sweep_media_dir_at(dir: &std::path::Path, retention: std::time::Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let stale = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > retention);
        if stale && std::fs::remove_file(e.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}
/// 媒体目录：`<imagent_home>/media/`（0700；P4-10：随 profile 隔离）。
fn media_dir() -> Result<std::path::PathBuf> {
    let dir = imagent_core::paths::imagent_home().join("media");
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| CoreError::Platform(PLATFORM, format!("create media dir {dir:?}: {e}")))?;
    }
    Ok(dir)
}

/// 把媒体字节落盘到 `~/.imagent/media/`，返回本地路径字符串。
///
/// 原名透传（安全批次）：file 消息带原始 `file_name`（含扩展名）——按原名落盘，
/// agent 侧拿到的文件名/扩展名与用户发送的一致（此前统一 `<key>.bin`，下游按
/// 扩展名识别格式会失效）。文件名做净化（剥路径分隔符，防 `../` 逃逸）；缺原名的
/// file 与图片回退 `<key>.<默认扩展名>`。图片消息 content 无原始文件名，真实格式
/// 只有飞书侧知道（image_key 不带扩展信息）——默认 **png**（无损通用形态，jpg
/// 有损假设会二次压缩误导；下载字节原样落盘，仅扩展名标注取舍）。
/// 照 ilink `persist_media`：目录 0700、文件 0600（解密后的私聊媒体不暴露给同机其他用户）。
/// 取舍：原名不再天然全局唯一（同名文件后到覆盖先到）——换「agent 拿到真实文件
/// 名/扩展名」的收益，覆盖窗口极窄（同会话同名连发），可接受。
pub(super) fn persist_media(
    kind: &str,
    key: &str,
    file_name: Option<&str>,
    bytes: &[u8],
) -> Result<String> {
    let dir = media_dir()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    // 净化：剥路径分隔符与目录段，仅留文件名本体；空/全非法回退资源 key。
    let safe_name = file_name
        .map(|n| {
            n.rsplit(['/', '\\'])
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .filter(|n| !n.is_empty());
    let name = match (kind, safe_name) {
        // file：原名原名扩展名整体保留（无扩展名也照旧——原样最忠实）。
        ("file", Some(n)) => n,
        // image：content 无原名；若 post/file 路径带名则用其扩展名，否则默认 png。
        ("image", Some(n)) => {
            let ext = n.rsplit('.').next().unwrap_or("");
            let base = n.rsplit_once('.').map(|(b, _)| b).unwrap_or(n.as_str());
            let ext = if ext.is_empty() || ext == n {
                "png"
            } else {
                ext
            };
            format!("{base}.{ext}")
        }
        _ => {
            let ext = if kind == "image" { "png" } else { "bin" };
            format!("{key}.{ext}")
        }
    };
    // W4-3：同名媒体不覆盖——后到的文件加序号后缀（此前后到覆盖先到，用户
    // 连发同名文件时前一份丢失、agent 读到错文件）。
    let path = {
        let p = dir.join(&name);
        if !p.exists() {
            p
        } else {
            let stem = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("media")
                .to_string();
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{e}"))
                .unwrap_or_default();
            let mut i = 1u32;
            loop {
                let cand = dir.join(format!("{stem}-{i}{ext}"));
                if !cand.exists() {
                    break cand;
                }
                i += 1;
            }
        }
    };
    std::fs::write(&path, bytes)
        .map_err(|e| CoreError::Platform(PLATFORM, format!("write media {path:?}: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(path.to_string_lossy().into_owned())
}
#[cfg(test)]
mod tests {
    use super::*;

    /// housekeeping 媒体 GC：按 mtime 删除超期文件、保留新文件与子目录。
    #[test]
    fn sweep_media_dir_at_removes_only_stale_files() {
        let dir = std::env::temp_dir().join(format!(
            "imagent_media_gc_{}_{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // 新文件（保留）。
        std::fs::write(dir.join("fresh.png"), b"new").unwrap();
        // 超期文件：mtime 拨回 8 天前（删除）。
        let old = dir.join("stale.bin");
        std::fs::write(&old, b"old").unwrap();
        let ancient = std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 3600);
        let f = std::fs::File::options().write(true).open(&old).unwrap();
        f.set_modified(ancient).unwrap();
        drop(f);
        // 子目录不动（防误删未来扩展形态）。
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        let removed = sweep_media_dir_at(&dir, std::time::Duration::from_secs(7 * 24 * 3600));
        assert_eq!(removed, 1, "只删超期文件");
        assert!(dir.join("fresh.png").exists(), "新文件保留");
        assert!(!old.exists(), "超期文件已删");
        assert!(dir.join("subdir").exists(), "子目录不动");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_media_same_name_gets_suffix() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let name = format!("同名{}-{nanos}.txt", std::process::id());
        let p1 = persist_media("file", "k9", Some(&name), b"first").unwrap();
        let p2 = persist_media("file", "k10", Some(&name), b"second").unwrap();
        assert_ne!(p1, p2, "同名不应覆盖: {p1} vs {p2}");
        let stem = name.strip_suffix(".txt").unwrap_or(&name);
        assert!(p2.ends_with(&format!("{stem}-1.txt")), "后缀序号: {p2}");
        assert_eq!(std::fs::read(&p1).unwrap(), b"first", "先到的文件内容保留");
        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
    }

    #[test]
    fn persist_media_original_name() {
        let dir = std::env::temp_dir().join(format!("imagent_persist_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // media_dir 固定 ~/.imagent/media，直接测同目录语义（写入该目录并清理）。
        let base = media_dir().unwrap();
        let p1 = persist_media("file", "k1", Some("报告 v2.pdf"), b"x").unwrap();
        assert!(p1.ends_with("报告 v2.pdf"), "{p1}");
        // 净化：路径分隔符段被剥。
        let p2 = persist_media("file", "k2", Some("../../evil.sh"), b"x").unwrap();
        assert!(p2.ends_with("evil.sh") && !p2.contains(".."), "{p2}");
        // 图片无原名：key.png（默认 png，取舍见函数注释）。
        let p3 = persist_media("image", "img_k3", None, b"x").unwrap();
        assert!(p3.ends_with("img_k3.png"), "{p3}");
        // 图片带名（未来路径）：用其扩展名。
        let p4 = persist_media("image", "img_k4", Some("photo.jpg"), b"x").unwrap();
        assert!(p4.ends_with("photo.jpg"), "{p4}");
        // 无名 file：key.bin。
        let p5 = persist_media("file", "k5", None, b"x").unwrap();
        assert!(p5.ends_with("k5.bin"), "{p5}");
        for f in [
            "报告 v2.pdf",
            "evil.sh",
            "img_k3.png",
            "photo.jpg",
            "k5.bin",
        ] {
            let _ = std::fs::remove_file(base.join(f));
        }
        let _ = dir;
    }
    /// 评论者锚点表 LRU：超 [`COMMENT_ANCHOR_CAP`]（64）淘汰最久未活跃条目；
    /// 同评论者更新走覆盖不触发淘汰；「最近评论」回退锚点不受 LRU 影响。
    #[test]
    fn comment_anchor_map_lru_cap() {
        let mut st = ConvState::default();
        for i in 0..COMMENT_ANCHOR_CAP {
            st.note_comment(&format!("ou_{i}"), "c");
        }
        assert_eq!(st.comment_anchors.len(), COMMENT_ANCHOR_CAP);
        // 拨定 LRU 序：手工把 ou_0 的 touched 置为当前（必然晚于上面全部插入）。
        let now = Instant::now();
        st.comment_anchors.get_mut("ou_0").unwrap().touched = now;
        // 同评论者（ou_1）更新：不淘汰、len 不变、id 已刷新。
        st.note_comment("ou_1", "c_1b");
        assert_eq!(st.comment_anchors.len(), COMMENT_ANCHOR_CAP);
        assert_eq!(
            st.resolve_comment_anchor(Some("ou_1")).as_deref(),
            Some("c_1b")
        );
        // 新评论者进入：LRU 淘汰最久未活跃的 ou_2（ou_0 已拨新、ou_1 刚更新）。
        st.note_comment("ou_new", "c_new");
        assert_eq!(st.comment_anchors.len(), COMMENT_ANCHOR_CAP, "上限不变");
        assert!(st.comment_anchors.contains_key("ou_0"), "活跃者保留");
        assert!(st.comment_anchors.contains_key("ou_1"), "刚更新者保留");
        assert!(
            !st.comment_anchors.contains_key("ou_2"),
            "最久未活跃者被淘汰"
        );
        assert_eq!(
            st.resolve_comment_anchor(Some("ou_new")).as_deref(),
            Some("c_new")
        );
        // 被淘汰者回退「最近评论」（c_new 刚登记即最近）。
        assert_eq!(
            st.resolve_comment_anchor(Some("ou_2")).as_deref(),
            Some("c_new")
        );
    }
}
