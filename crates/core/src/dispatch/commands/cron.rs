//! `/cron` 定时任务（v1.18 头牌）：5 字段 cron 表达式 + store 持久化 + 到期
//! 经正常 handle 管线注入（鉴权/会话域/审批链全继承）。
//!
//! 引擎为纯 rust 实现（不引 chrono）：字段匹配集合 + 逐分钟步进搜下次触发，
//! 本地时区经 `libc::localtime_r`（含 DST；每次 next_after 取一次偏移，换点
//! 小时内的边界不追求精确——分钟级任务场景可接受）。

use super::*;
use crate::UserId;

/// 单字段匹配集合。`wildcard` 区分「无约束 *」与受限（`*/n` 算受限）——
/// Vixie cron 的 dom/dow OR 语义依赖它。
#[derive(Debug, Clone)]
struct FieldSpec {
    values: Vec<u32>,
    wildcard: bool,
}

fn parse_field(spec: &str, lo: u32, hi: u32) -> Option<FieldSpec> {
    let mut values = Vec::new();
    let mut wildcard = true;
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().ok()?.max(1)),
            None => (part, 1),
        };
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((x, y)) = range.split_once('-') {
            (x.parse::<u32>().ok()?, y.parse::<u32>().ok()?)
        } else {
            let v = range.parse::<u32>().ok()?;
            (v, v)
        };
        if a < lo || b > hi || a > b {
            return None;
        }
        if range != "*" {
            wildcard = false;
        }
        let mut v = a;
        while v <= b {
            values.push(v);
            v += step;
        }
    }
    values.sort_unstable();
    values.dedup();
    if values.is_empty() {
        return None;
    }
    Some(FieldSpec { values, wildcard })
}

/// 解析后的 cron 表达式（分 时 日 月 周，本地时区）。
#[derive(Debug, Clone)]
pub(super) struct CronSpec {
    minute: FieldSpec,
    hour: FieldSpec,
    dom: FieldSpec,
    mon: FieldSpec,
    dow: FieldSpec,
}

impl CronSpec {
    /// 解析 5 字段表达式（空白分隔）。非法返回 None。
    pub(super) fn parse(expr: &str) -> Option<Self> {
        let mut it = expr.split_whitespace();
        let spec = Self {
            minute: parse_field(it.next()?, 0, 59)?,
            hour: parse_field(it.next()?, 0, 23)?,
            dom: parse_field(it.next()?, 1, 31)?,
            mon: parse_field(it.next()?, 1, 12)?,
            dow: parse_field(it.next()?, 0, 6)?, // 0 = 周日
        };
        it.next().is_none().then_some(spec)
    }

    /// 五元组是否命中（供测试与 next_after 共用；dow 0=周日）。
    fn matches(&self, min: u32, hour: u32, dom: u32, mon: u32, dow: u32) -> bool {
        let dom_ok = self.dom.values.contains(&dom);
        let dow_ok = self.dow.values.contains(&dow);
        // Vixie 语义：dom 与 dow 都受限时按 OR（「15 号」或「每周二」），否则 AND
        //（未受限侧恒真）。
        let day_ok = if !self.dom.wildcard && !self.dow.wildcard {
            dom_ok || dow_ok
        } else {
            dom_ok && dow_ok
        };
        self.minute.values.contains(&min)
            && self.hour.values.contains(&hour)
            && self.mon.values.contains(&mon)
            && day_ok
    }

    /// from 之后（严格大于）的下次触发时刻（epoch 秒，分钟对齐）。无解
    /// （如 2 月 30 日）在 366 天内搜不到时返回 None。
    pub(super) fn next_after(&self, from_epoch: i64) -> Option<i64> {
        // v1.18 review：偏移量随步进重算——此前整个 366 天窗口共用起点的单个
        // 偏移，跨 DST 换点后的触发按旧偏移对齐（每天 9:00 的任务在换点次日
        // 8:00 触发，错的 next_run 还被落库顺延）。日变更粒度刷新 + 候选命中
        // 时实时偏移复核（换点发生在凌晨、日边界重算覆盖不到当天 2-3 点后的
        // 场景），localtime_r 调用量每天至多几次。
        let mut offset = local_offset_secs(from_epoch);
        let mut cur_day = (from_epoch + offset).div_euclid(86_400);
        // 从 from 的下一分钟边界开始步进（60s 对齐）。
        let mut t = from_epoch - from_epoch.rem_euclid(60) + 60;
        let limit = t + 366 * 24 * 3600;
        while t <= limit {
            let day = (t + offset).div_euclid(86_400);
            if day != cur_day {
                offset = local_offset_secs(t);
                cur_day = (t + offset).div_euclid(86_400);
            }
            let (min, hour, dom, mon, dow) = civil_fields(t + offset);
            if self.matches(min, hour, dom, mon, dow) {
                // 命中后用候选时刻的实时偏移复核：换点窗口内旧偏移可能给出
                // 假命中/假错过——刷新偏移重算本分钟（不推进 t）。
                let fresh = local_offset_secs(t);
                if fresh != offset {
                    offset = fresh;
                    continue;
                }
                return Some(t);
            }
            t += 60;
        }
        None
    }
}

/// epoch → 本地 UTC 偏移秒（libc::localtime_r；DST 由系统处理）。
// SAFETY（deny 豁免，同 socket::peer_uid 先例）：仅读入参标量、写入零初始化
// 的 tm 结构体，localtime_r 按 sig 消费两者，无跨线程共享状态（返回值线程
// 局部，glibc localtime_r 本身线程安全）。
#[allow(unsafe_code)]
fn local_offset_secs(epoch: i64) -> i64 {
    unsafe {
        let t = epoch as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i64
    }
}

/// 本地 epoch 秒 → (分, 时, 日, 月, 周几)。civil 换算用 Hinnant 算法
/// （days_from_civil），与 libc 结果一致且免结构体依赖。
fn civil_fields(local_secs: i64) -> (u32, u32, u32, u32, u32) {
    let days = local_secs.div_euclid(86_400);
    let secs_of_day = local_secs.rem_euclid(86_400);
    let hour = secs_of_day / 3600;
    let minute = secs_of_day % 3600 / 60;
    // 1970-01-01 是周四（dow=4）。
    let dow = (days + 4).rem_euclid(7) as u32;
    let (_y, m, d) = civil_from_days(days);
    (minute as u32, hour as u32, d, m, dow)
}

/// days since epoch → (年, 月, 日)（Howard Hinnant civil_from_days）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 本地时间展示（MM-dd HH:MM，/cron list 回执用）。
pub(super) fn format_local(epoch: i64) -> String {
    let (min, hour, dom, mon, _) = civil_fields(epoch + local_offset_secs(epoch));
    format!("{mon:02}-{dom:02} {hour:02}:{min:02}")
}

/// 短 id：内容稳定哈希取 6 hex（与 dedup 回退 key 同哈希族，仅作标识不做安全用途）。
pub(super) fn cron_id(conv: &str, expr: &str, prompt: &str) -> String {
    use std::hash::{Hash, Hasher};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (conv, expr, prompt, now).hash(&mut h);
    format!("{:06x}", h.finish() & 0xff_ffff)
}

impl Dispatcher {
    /// `/cron` 命令入口：add <5 字段> <指令> / list / rm <id>。
    pub(super) async fn cmd_cron(
        &self,
        conv: &ConvId,
        sender: &UserId,
        hint: &ReplyHint,
        parts: &[&str],
    ) {
        let usage = "用法：/cron add <分 时 日 月 周> <指令>…（如 `/cron add 0 9 * * * 给我今日站会摘要`，\"* * * * *\" 每分钟、`0 9 * * 1-5` 工作日 9 点）\n/cron list 列出本会话任务 · /cron rm <id> 删除";
        let Some(sub) = parts.get(1).map(|s| s.to_ascii_lowercase()) else {
            self.reply(conv, usage, hint).await;
            return;
        };
        match sub.as_str() {
            "add" => {
                // parts: /cron add f1 f2 f3 f4 f5 prompt…
                if parts.len() < 7 {
                    self.reply(conv, &format!("⚠️ 缺少表达式或指令。\n{usage}"), hint)
                        .await;
                    return;
                }
                let expr = parts[2..7].join(" ");
                let prompt = parts[7..].join(" ");
                let Some(spec) = CronSpec::parse(&expr) else {
                    self.reply(conv, &format!("⚠️ 表达式非法：`{expr}`\n{usage}"), hint)
                        .await;
                    return;
                };
                let now = super::super::now_secs();
                let Some(next) = spec.next_after(now) else {
                    self.reply(
                        conv,
                        &format!("⚠️ 表达式一年内无触发时刻（如 2 月 30 日）：`{expr}`"),
                        hint,
                    )
                    .await;
                    return;
                };
                // 每会话上限 20 条：定时注入与手打同权，防滥用占满轮次。
                let jobs = self.store.list_cron_jobs().await.unwrap_or_default();
                if jobs.iter().filter(|j| j.conv == conv.0).count() >= 20 {
                    self.reply(
                        conv,
                        "⚠️ 本会话定时任务已达上限（20 条），请先 /cron rm 清理。",
                        hint,
                    )
                    .await;
                    return;
                }
                let id = cron_id(&conv.0, &expr, &prompt);
                let job = imagent_store::CronJobRow {
                    id: id.clone(),
                    conv: conv.0.clone(),
                    sender: sender.0.clone(),
                    expr: expr.clone(),
                    prompt: prompt.clone(),
                    created_at: now,
                    last_run: None,
                    next_run: next,
                    enabled: true,
                };
                if let Err(e) = self.store.insert_cron_job(&job).await {
                    warn!(target: "imagent::core", error = %e, "定时任务落库失败");
                    self.reply(conv, "⚠️ 创建失败（存储错误），请稍后重试。", hint)
                        .await;
                    return;
                }
                self.reply(
                conv,
                &format!(
                    "✅ 定时任务已创建 `{id}`\n⏰ `{expr}`（本地时区）\n📝 {prompt}\n⏭️ 下次执行：{}",
                    format_local(next)
                ),
                hint,
            )
            .await;
            }
            "list" => {
                let jobs = self
                    .store
                    .list_cron_jobs()
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|j| j.conv == conv.0)
                    .collect::<Vec<_>>();
                if jobs.is_empty() {
                    self.reply(conv, "📭 本会话没有定时任务（/cron add 创建）。", hint)
                        .await;
                    return;
                }
                let mut body = String::from("⏰ 本会话定时任务：");
                for j in jobs {
                    // v1.18 review：优先展示调度器权威的 next_run（触发时已按实际
                    // fire 时刻顺延）——此前从 last_run 重算，停机跨过首跑的任务
                    // 会显示一个过去的「下次执行」直到下个 tick。
                    let next = if j.next_run > 0 {
                        j.next_run
                    } else {
                        j.last_run.unwrap_or(0)
                    };
                    let next = if next > 0 {
                        next
                    } else {
                        CronSpec::parse(&j.expr)
                            .and_then(|s| s.next_after(j.created_at))
                            .unwrap_or(j.next_run)
                    };
                    body.push_str(&format!(
                        "\n- `{}` `{}`（下次 {}）→ {}",
                        j.id,
                        j.expr,
                        format_local(next),
                        truncate_str(&j.prompt, 40)
                    ));
                }
                self.reply(conv, &body, hint).await;
            }
            "rm" => {
                let Some(id) = parts.get(2) else {
                    self.reply(conv, &format!("⚠️ 缺少 id。\n{usage}"), hint)
                        .await;
                    return;
                };
                match self.store.get_cron_job(id).await {
                    Ok(Some(job)) if job.conv == conv.0 => {
                        // v1.18 review：与其它授权门统一走 is_admin（两侧 trim）——
                        // 直接 contains 会漏掉配置里带首尾空格的 admin。
                        if job.sender != sender.0 && !self.is_admin(&sender.0) {
                            self.reply(conv, "⛔ 只能删除自己创建的任务（管理员可删任意）。", hint)
                                .await;
                            return;
                        }
                        match self.store.delete_cron_job(id).await {
                            Ok(true) => {
                                self.reply(conv, &format!("🗑️ 已删除定时任务 `{id}`。",), hint)
                                    .await
                            }
                            Ok(false) => self.reply(conv, "⚠️ 任务不存在。", hint).await,
                            Err(e) => {
                                warn!(target: "imagent::core", error = %e, "定时任务删除失败");
                                self.reply(conv, "⚠️ 删除失败（存储错误）。", hint).await;
                            }
                        }
                    }
                    Ok(Some(_)) => self.reply(conv, "⛔ 任务不属于本会话。", hint).await,
                    Ok(None) => self.reply(conv, "⚠️ 任务不存在。", hint).await,
                    Err(e) => {
                        warn!(target: "imagent::core", error = %e, "定时任务查询失败");
                        self.reply(conv, "⚠️ 查询失败（存储错误）。", hint).await;
                    }
                }
            }
            _ => self.reply(conv, usage, hint).await,
        }
    }

    /// 调度器主体（run() 内 spawn，30s tick）：到期任务先重排（防长轮次执行期
    /// 重复入队）再合成消息走 handle——白名单/会话域/审批链与手打消息完全同权。
    pub(crate) async fn fire_due_cron_jobs(self: &Arc<Self>) {
        let now = super::super::now_secs();
        let due = match self.store.due_cron_jobs(now).await {
            Ok(d) => d,
            Err(e) => {
                warn!(target: "imagent::core", error = %e, "定时任务查询失败（本轮跳过）");
                return;
            }
        };
        for job in due {
            let next = match CronSpec::parse(&job.expr).and_then(|s| s.next_after(now)) {
                Some(n) => n,
                None => {
                    warn!(target: "imagent::core", id = %job.id, "定时任务表达式已无解，停用");
                    let _ = self.store.set_cron_enabled(&job.id, false).await;
                    continue;
                }
            };
            if let Err(e) = self.store.bump_cron_job(&job.id, now, next).await {
                warn!(target: "imagent::core", error = %e, id = %job.id, "定时任务重排失败（跳过本次触发）");
                continue;
            }
            info!(target: "imagent::core", id = %job.id, conv_id = %job.conv, "定时任务触发");
            let msg = InboundMessage {
                conv_id: ConvId(job.conv.clone()),
                sender: UserId(job.sender.clone()),
                text: Some(format!("⏰ 定时任务触发，请执行：{}", job.prompt)),
                media: vec![],
                media_errors: Vec::new(),
                mentions: Vec::new(),
                mentioned_bot: false,
                ask_req: None,
                reply_to: None,
                source_msg_id: None,
                control: None,
                reply_hint: ReplyHint::None,
            };
            // 调度器只分发不执行：handle 对空闲 conv 会内联跑完整轮 agent（分钟
            // 级），若在 tick 循环里 await 会（a）队头阻塞所有 conv 的其它 cron
            // 任务、（b）select 饿死收不到 shutdown。与 recv 循环同款：spawn 进
            // tasks，drain（P1-5）一并覆盖 cron 驱动的轮次。
            let this = self.clone();
            self.tasks.lock().await.spawn(async move {
                this.handle(msg).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_parse_fields() {
        assert!(CronSpec::parse("* * * * *").is_some());
        assert!(CronSpec::parse("*/5 9-18 * * 1-5").is_some());
        assert!(CronSpec::parse("0 9 1,15 * *").is_some());
        // 字段数与值域。
        assert!(CronSpec::parse("* * * *").is_none());
        assert!(CronSpec::parse("60 * * * *").is_none());
        assert!(CronSpec::parse("* 24 * * *").is_none());
        assert!(CronSpec::parse("* * 0 * *").is_none());
        assert!(CronSpec::parse("* * * 13 *").is_none());
        assert!(CronSpec::parse("* * * * 7").is_none());
        assert!(CronSpec::parse("5-1 * * * *").is_none());
        assert!(CronSpec::parse("* * * * * extra").is_none());
    }

    #[test]
    fn cron_every_minute() {
        let s = CronSpec::parse("* * * * *").unwrap();
        let t0 = 1_800_000_000_i64; // 任意时刻
        let next = s.next_after(t0).unwrap();
        assert!(next > t0 && next <= t0 + 60, "下一分钟边界: {next}");
        assert_eq!(next % 60, 0);
    }

    #[test]
    fn cron_step_alignment_local() {
        // */5：下次触发的本地分钟必为 5 的倍数（时区偏移为整分钟时成立——全球
        // 现行时区均整分钟/整小时）。自洽校验：与 civil_fields 同一偏移源。
        let s = CronSpec::parse("*/5 * * * *").unwrap();
        let t0 = 1_800_000_123_i64;
        let next = s.next_after(t0).unwrap();
        let (min, ..) = civil_fields(next + local_offset_secs(next));
        assert_eq!(min % 5, 0);
    }

    #[test]
    fn cron_dom_or_dow() {
        // dom 与 dow 同时受限 → OR 语义：每月 1 号或每个周日命中。
        let s = CronSpec::parse("0 0 1 * 0").unwrap();
        // 任取一天验证 matches 语义（直接走内部匹配，避免时区耦合）。
        // 2026-09-03 是周四（dow=4）：1 号但非周日 → 应命中（OR）。
        assert!(s.matches(0, 0, 1, 9, 4));
        // 周日但非 1 号 → 命中。
        assert!(s.matches(0, 0, 6, 9, 0));
        // 既非 1 号也非周日 → 不命中。
        assert!(!s.matches(0, 0, 6, 9, 4));
        // 只限定 dom（dow 为 *）→ dow 无关，dom 必须命中。
        let s2 = CronSpec::parse("0 0 1 * *").unwrap();
        assert!(s2.matches(0, 0, 1, 9, 0));
        assert!(s2.matches(0, 0, 1, 9, 3));
        assert!(!s2.matches(0, 0, 2, 9, 0));
    }

    #[test]
    fn cron_impossible_date() {
        // 2 月 30 日永不存在 → 一年内无解。
        let s = CronSpec::parse("0 0 30 2 *").unwrap();
        assert!(s.next_after(1_800_000_000).is_none());
    }

    #[test]
    fn cron_civil_roundtrip_known_dates() {
        // 1970-01-01（周四）与 2026-09-03（周四）的 civil 换算。
        assert_eq!(civil_fields(0), (0, 0, 1, 1, 4));
        // 2026-09-03 00:00:00 UTC = 1_788_393_600（周四）。
        assert_eq!(civil_fields(1_788_393_600), (0, 0, 3, 9, 4));
    }

    #[test]
    fn cron_format_local_shape() {
        let s = format_local(1_788_393_600);
        assert_eq!(s.len(), 11, "MM-dd HH:MM: {s}");
        assert!(s.ends_with(":00"));
    }
}
