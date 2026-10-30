// v1.7 命令面板数据（纯 SPA，无协议改动）：内置斜杠命令表 + 过滤。
// 命令名与参数以 crates/core/src/dispatch/commands/mod.rs 的命令分派注册表
// （KNOWN_COMMAND_WORDS）及各命令实现里的用法文案为准核对；描述自拟精炼。

/** 单条命令条目：name 含 / 前缀（与后端分派臂逐字一致）。 */
export type SlashCommand = {
  name: string;
  /** 参数提示（如 "<绝对路径>"；无参命令缺省）。 */
  args?: string;
  /** 中文一句话描述（命令面板正文，也参与过滤匹配）。 */
  desc: string;
  /** 分组名（面板分组标题）。 */
  group: string;
};

/** /model 行附注 + 快捷 chips 的模型名（点击直接填 `/model <名> `）。 */
export const MODEL_CHIPS: readonly string[] = ["default", "sonnet", "opus", "haiku"];

/** 内置命令表（按分组排列；填充文本 = `${name} `，光标留参数位）。 */
export const COMMANDS: readonly SlashCommand[] = [
  // ---- 会话与任务 ----
  { name: "/stop", args: "[all]", desc: "中断当前任务；加 all 硬停并清空排队", group: "会话与任务" },
  { name: "/tasks", desc: "查看任务清单（待办与执行状态）", group: "会话与任务" },
  { name: "/resume", args: "[序号]", desc: "列出并恢复历史会话（按序号选择）", group: "会话与任务" },
  { name: "/retry", desc: "重试上一条指令", group: "会话与任务" },
  { name: "/queue", args: "[drop <序号>]", desc: "查看排队消息；drop 按序号丢弃", group: "会话与任务" },
  { name: "/status", desc: "查看会话状态与上下文水位", group: "会话与任务" },
  { name: "/export", args: "[序号]", desc: "导出会话记录（序号同 /resume 列表）", group: "会话与任务" },
  // ---- 目录与空间 ----
  { name: "/cd", args: "<绝对路径>", desc: "切换当前会话的工作目录", group: "目录与空间" },
  {
    name: "/ws",
    args: "[list|save <名>|use <名>|remove <名>]",
    desc: "命名工作空间：列出/保存/切换/删除",
    group: "目录与空间",
  },
  // ---- 模型与权限 ----
  {
    name: "/model",
    args: "<名称|default>",
    desc: "切换模型（需管理员；default 恢复默认）· 常用：sonnet / opus / haiku",
    group: "模型与权限",
  },
  {
    name: "/perm",
    args: "<auto|off|allow|deny|ask>",
    desc: "权限模式热切；list 查看会话授权、revoke 撤销",
    group: "模型与权限",
  },
  { name: "/timeout", args: "<分钟|off|default>", desc: "空闲看门狗：设时长/关闭/恢复默认", group: "模型与权限" },
  // ---- 维护 ----
  { name: "/compact", desc: "压缩会话上下文（长会话减负）", group: "维护" },
  {
    name: "/cron",
    args: "[add|list|rm|enable|disable]",
    desc: "定时任务：添加/列出/删除/启停",
    group: "维护",
  },
  { name: "/delegate", args: "[名称 子任务]", desc: "委托副轮执行子任务（不占主会话上下文）", group: "维护" },
  { name: "/help", desc: "查看全部命令帮助", group: "维护" },
];

/**
 * 按输入前缀过滤命令：name 前缀匹配；desc 包含关键词（剥 / 后匹配中文描述，
 * 仅 2 字符及以上——单字符会命中描述里的英文单词碎片，纯噪声）。
 * 入参约定为「以 / 开头且不含空白」的查询串——含空白即已进入参数输入态，
 * 调用方（Composer）不弹面板。
 */
export function filterCommands(query: string): SlashCommand[] {
  const q = query.startsWith("/") ? query.slice(1) : query;
  return COMMANDS.filter(
    (c) => c.name.startsWith(query) || (q.length >= 2 && c.desc.includes(q)),
  );
}
