import { useMemo, useRef, useState } from "react";
import { convDisplayName, DEFAULT_MACHINE, normalizeExpiryMs } from "../lib/protocol";
import type { ThemePref } from "../lib/theme";

/** 主题三态选项（value 对齐 lib/theme 的 ThemePref；展示序即选择序）。 */
const THEME_OPTIONS: { value: ThemePref; label: string }[] = [
  { value: "system", label: "跟随系统" },
  { value: "light", label: "浅色" },
  { value: "dark", label: "深色" },
];

/** v1.41.4 左滑露出删除按钮的宽度 = 吸附打开位移（px）：垃圾桶图标 +「删除」
 *  文案一行放下的最小舒适宽度。 */
const SWIPE_REVEAL_PX = 76;

/** 松手吸附阈值（px）：位移超过该值松手 → 吸附打开（露出删除按钮），否则
 *  回弹关闭。约露出宽度过半即开，对齐移动端 swipe-to-reveal 惯例。 */
const SWIPE_OPEN_PX = 40;

/** 方向判定阈值（px）：按下后位移超过它才判方向——横向分量占优认领左滑
 * （项跟手），纵向占优放行页面垂直滚动（touch-action: pan-y 下浏览器接管
 * 滚动并派发 pointercancel 收尾，本组件不 preventDefault 垂直滚动）。 */
const SWIPE_DIRECTION_PX = 10;

/** 侧边栏会话项数据（App 从 state 派生）：conv 全名 + 显示名 + 最后消息
 *  预览/时间 + 未读计数（非当前 conv 收到新消息时 reducer 累加，点选清零）。 */
export type ConvItem = {
  convId: string;
  /** 会话显示名（App 经 convTitle 派生传入：自动会话 web:s-xxxx 用首条消息
   *  派生的自动 title，手动名/default/local 恒 convId 剥前缀）。 */
  title?: string;
  preview: string;
  at: number;
  /** 未读条数（0 = 无徽标）。 */
  unread?: number;
  /** v1.37 运行态（App 派生：该会话存在流式中的卡片）：标题行左侧渲染呼吸
   *  动画小圆点（风格随状态条在线点）。 */
  running?: boolean;
};

/** 会话过期横幅阈值：剩余不足 3 天开始提示。 */
const EXPIRY_WARN_DAYS = 3;

/** 会话项时间：当天显示 HH:mm，否则 M/D HH:mm。 */
function formatTime(at: number): string {
  if (!at) {
    return "";
  }
  const d = new Date(at);
  const hm = d.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false });
  if (d.toDateString() === new Date().toDateString()) {
    return hm;
  }
  return `${d.getMonth() + 1}/${d.getDate()} ${hm}`;
}

/**
 * 会话侧边栏（桌面常驻 / 移动抽屉共用）：标题 + Mac 在线状态点、机器切换器
 * （v1.1 多机模式）、会话列表（当前选中机器的会话池，当前项高亮，每项含
 * 名字/最后消息一行预览/时间）、底部「+ 新会话」。
 *
 * 交互语义：
 * - 点选会话 = 仅切换 SPA 本地当前 conv（不发包）；
 * - 左滑会话项（v1.41.4）= 横向位移占优且超判定阈值后项跟手左移，松手过半
 *   吸附打开露出删除按钮，点击按钮弹删除确认（App 级弹层二次确认后发
 *   conv_delete 业务帧，网关删三处 + 快照收敛）；滑动松手后与打开态点本体
 *   均吞 click（不切会话）；纵向占优放行页面垂直滚动（不 preventDefault）；
 *   一次只开一项；桌面（≥md）hover 显小删除图标兜底；传入 onConvDelete
 *   才启用（缺席 = 旧入口不显示行为变化）；
 * - 点选机器 = 切换 currentMachine（App 侧把当前 conv 回到该机器默认会话，
 *   并发一次带 machine 的 sync 拉该机器卡片缓存）；
 * - 新会话（v1.6）= 打开 App 级弹层（会话名 + 工作目录选择 WorkspacePicker），
 *   确认后由上层发 `/switch <名字>` message 命令帧（convId 留空落默认会话）
 *   并按需绑定目录（conv_workdir_set），SPA 同时把当前 conv 切到 `web:<名字>`。
 *   名字留空时上层落 s-xxxx 自动短 ID（协议零改动），会话项显示名由首条消息
 *   派生（title，见 ConvItem）。
 * - 未收到任何 machines 帧（旧 relay）：不显示机器区，单机行为与升级前一致。
 * - v1.2 会话层：底部「管理」入口（仅会话模式；v1.5 起为管理面板——机器/
 *   令牌/设备/钥匙/指纹）与「会话 N 天后过期」续期横幅（剩余 < 3 天；点击
 *   进入登出流程）。
 * - 主题三态开关：底部「外观」段选（跟随系统/浅色/深色），偏好由 App 持久
 *   化并应用到 document（lib/theme.ts），本组件纯展示上抛 onThemeChange。
 *
 * 已知限制：会话内消息历史仅 SPA 内存保留，页面刷新后仅当前卡片由 sync
 * 重发恢复（text/user 回显不重放）。
 */
export function Sidebar({
  convs,
  current,
  machineOnline,
  machines,
  machinesKnown,
  currentMachine,
  sessionExpiresAt,
  onRenewSession,
  onOpenAdmin,
  onTogglePush,
  pushEnabled,
  pushAvailable,
  pushBusy,
  themePref,
  onThemeChange,
  onSelect,
  onSelectMachine,
  onCreate,
  onConvDelete,
}: {
  convs: ConvItem[];
  current: string;
  /** 旧网关 machine 帧在线态（仅单机模式显示）。 */
  machineOnline: boolean;
  /** 在线机器全量列表（machines 帧）。 */
  machines: string[];
  /** 收到过 machines 帧 = 多机模式（显示机器切换器）。 */
  machinesKnown: boolean;
  currentMachine: string;
  /** v1.2 会话过期时间（ms/秒 epoch 容错）；缺省 = 旧 relay 直连模式。 */
  sessionExpiresAt?: number;
  /** 过期横幅点击 → 登出流程（重新登录即续期 30 天）。 */
  onRenewSession?: () => void;
  /** v1.5 管理面板入口（App 仅会话模式传入；旧 relay 不显示）。 */
  onOpenAdmin?: () => void;
  /** v1.10 通知开关点击（App 仅会话模式传入；旧 relay 无推送端点不显示）。
   *  不可用（能力缺失 / iOS 未添加主屏）时点击由 App 给引导提示。 */
  onTogglePush?: () => void;
  /** v1.10 通知开关当前态：已开启（订阅生效）/ 未开启。 */
  pushEnabled: boolean;
  /** v1.10 通知能力可用（pushSupported）；不可用时状态列显示「不可用」。 */
  pushAvailable: boolean;
  /** v1.10 开关流程进行中（enable/disable 请求期间禁点）。 */
  pushBusy: boolean;
  /** 主题三态偏好当前值（App 状态镜像；main.tsx 启动时已应用过一次）。 */
  themePref: ThemePref;
  /** 「外观」三段选择回调（App 落盘 saveThemePref + applyTheme 应用）。 */
  onThemeChange?: (pref: ThemePref) => void;
  onSelect: (conv: string) => void;
  /** 机器切换器点选（切 currentMachine + 触发该机器 sync）。 */
  onSelectMachine: (machine: string) => void;
  /** 打开新会话弹层（v1.6：会话名 + 工作目录选择在 App 级弹层完成）。 */
  onCreate: () => void;
  /** v1.41.4 左滑删除入口（露出删除按钮 / 桌面 hover 小图标点击后由 App 级
   *  确认弹层接手；缺席 = 不启用手势与删除入口）。 */
  onConvDelete?: (conv: string) => void;
}) {
  // v1.41.4 左滑删除手势状态：swipeOpenId = 当前吸附打开（露出删除按钮）的
  // 会话项，一次只开一项（认领新滑动即收旧项）；drag = 进行中的跟手位移
  // （渲染态，pointermove 高频更新驱动 translateX）；gestureRef = 手势进行时
  // 纯内存数据（起点/方向判定/当前位移，无渲染依赖不进 state）；
  // swallowClickRef 吞掉滑动松手后浏览器补发的 click（不切会话；v1.41.3
  // 吞 click 防误触语义的沿用）。
  const [swipeOpenId, setSwipeOpenId] = useState<string | null>(null);
  const [drag, setDrag] = useState<{ id: string; dx: number } | null>(null);
  const gestureRef = useRef<{
    id: string;
    startX: number;
    startY: number;
    /** 已通过方向判定认领为横向滑动。 */
    claimed: boolean;
    /** 认领时项的基准位移（已打开项从吸附位移起算，右滑可收回）。 */
    base: number;
    /** 当前位移（px，负值向左，已夹在 [-SWIPE_REVEAL_PX, 0]）。 */
    offset: number;
  } | null>(null);
  const swallowClickRef = useRef(false);
  const swipeEnabled = onConvDelete !== undefined;

  // v1.2 过期横幅：剩余 < 3 天提示（天数向上取整，至少 1 天）
  const daysLeft = useMemo(() => {
    if (sessionExpiresAt === undefined) {
      return null;
    }
    const remain = normalizeExpiryMs(sessionExpiresAt) - Date.now();
    return Math.max(1, Math.ceil(remain / 86_400_000));
  }, [sessionExpiresAt]);
  const expiryWarning = daysLeft !== null && daysLeft < EXPIRY_WARN_DAYS;

  // 机器行：在线列表 + 当前选中机器掉线时补一行灰点（保持可见可选，帧会回
  // machine_offline toast 提示原因）。default 不是真实机器，不补。
  const machineRows = useMemo(() => {
    const rows = machines.map((id) => ({ id, online: true }));
    if (
      machinesKnown &&
      currentMachine !== DEFAULT_MACHINE &&
      !machines.includes(currentMachine)
    ) {
      rows.push({ id: currentMachine, online: false });
    }
    return rows;
  }, [machines, machinesKnown, currentMachine]);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex items-center gap-2 border-b border-line px-4 py-3">
        <span className="text-sm font-semibold tracking-wide text-ink">imagent</span>
        {machinesKnown ? (
          <span className="ml-auto text-[11px] text-ghost">{machines.length} 台在线</span>
        ) : (
          <>
            <span
              className={`h-2 w-2 rounded-full ${machineOnline ? "bg-emerald-400" : "bg-slate-500"}`}
              title={`Mac ${machineOnline ? "在线" : "离线"}`}
              aria-label={`Mac ${machineOnline ? "在线" : "离线"}`}
            />
            <span className={`text-xs ${machineOnline ? "text-muted" : "text-ghost"}`}>
              Mac {machineOnline ? "在线" : "离线"}
            </span>
          </>
        )}
      </div>

      {/* v1.1 机器切换器：仅多机模式（收到过 machines 帧）显示 */}
      {machinesKnown && (
        <div className="border-b border-line p-2">
          <div className="px-2 pb-1 text-[11px] font-medium uppercase tracking-wider text-ghost">
            机器
          </div>
          <div className="space-y-0.5" role="listbox" aria-label="机器列表">
            {machineRows.length === 0 && (
              <div className="px-2.5 py-1.5 text-xs text-ghost">（无在线机器）</div>
            )}
            {machineRows.map((m) => {
              const active = m.id === currentMachine;
              return (
                <button
                  key={m.id}
                  type="button"
                  onClick={() => onSelectMachine(m.id)}
                  aria-selected={active}
                  className={
                    "flex w-full items-center gap-2 rounded-lg px-2.5 py-1.5 text-left transition-colors " +
                    (active
                      ? "bg-emerald-600/15 text-ink"
                      : "text-muted hover:bg-panel2 hover:text-ink")
                  }
                >
                  <span
                    className={
                      "h-2 w-2 shrink-0 rounded-full " +
                      (m.online ? "bg-emerald-400" : "bg-slate-500")
                    }
                    aria-hidden="true"
                  />
                  <span className="min-w-0 flex-1 truncate font-mono text-xs font-medium">
                    {m.id}
                  </span>
                  {!m.online && (
                    <span className="shrink-0 text-[10px] text-ghost">离线</span>
                  )}
                </button>
              );
            })}
          </div>
        </div>
      )}

      <nav className="min-h-0 flex-1 space-y-1 overflow-y-auto overscroll-contain p-2" aria-label="会话列表">
        {convs.map((item) => {
          const active = item.convId === current;
          const open = swipeOpenId === item.convId;
          const dragging = drag !== null && drag.id === item.convId;
          // 项本体位移：拖动中跟手（无过渡）；吸附态（打开 -76 / 关闭 0）带
          // 回弹过渡。删除按钮仅在开/拖时挂载——关闭态半透明底色的项会让
          // rose 按钮透出，不挂载即不存在透出问题（也顺带不可聚焦）。
          const offset = dragging ? drag.dx : open ? -SWIPE_REVEAL_PX : 0;
          return (
            <div key={item.convId} className="group relative overflow-hidden rounded-xl">
              {/* 左滑露出的删除按钮（rose 危险配色对齐 ConvDeleteConfirm 确认钮）：
                  绝对定位在项右侧被滑出的区域，项本体左移露出；点击只负责打开
                  App 级确认弹层（onConvDelete），发帧与本地移除全在既有链路 */}
              {swipeEnabled && (open || dragging) && (
                <button
                  type="button"
                  onClick={() => {
                    setSwipeOpenId(null);
                    onConvDelete?.(item.convId);
                  }}
                  className="absolute inset-y-0 right-0 flex w-[76px] items-center justify-center gap-1 rounded-xl bg-rose-600 text-xs font-semibold text-white transition-colors hover:bg-rose-500 active:bg-rose-700"
                >
                  <svg
                    viewBox="0 0 20 20"
                    className="h-4 w-4 shrink-0"
                    fill="none"
                    stroke="currentColor"
                    strokeWidth="1.6"
                    aria-hidden="true"
                  >
                    <path
                      d="M4 6h12M8 6V4.5A1.5 1.5 0 0 1 9.5 3h1A1.5 1.5 0 0 1 12 4.5V6M6 6l.7 9a1.5 1.5 0 0 0 1.5 1.4h3.6a1.5 1.5 0 0 0 1.5-1.4L14 6"
                      strokeLinecap="round"
                      strokeLinejoin="round"
                    />
                  </svg>
                  删除
                </button>
              )}
              <button
                type="button"
                onClick={() => {
                  // 点其他项：先收起已开项，照常切换会话
                  if (swipeOpenId && swipeOpenId !== item.convId) {
                    setSwipeOpenId(null);
                  }
                  // v1.41.4：刚完成滑动（浏览器补发 click）/ 打开态点本体（仅收起）
                  // → 吞本次 click 不切会话（v1.41.3 吞 click 防误触语义沿用）
                  if (swallowClickRef.current) {
                    swallowClickRef.current = false;
                    return;
                  }
                  if (open) {
                    setSwipeOpenId(null);
                    return;
                  }
                  onSelect(item.convId);
                }}
                onPointerDown={(e) => {
                  if (!swipeEnabled) {
                    return;
                  }
                  // 鼠标仅主键触发（右键语义不同）；已有手势进行中（多指）忽略。
                  if (e.pointerType === "mouse" && e.button !== 0) {
                    return;
                  }
                  if (gestureRef.current) {
                    return;
                  }
                  swallowClickRef.current = false;
                  gestureRef.current = {
                    id: item.convId,
                    startX: e.clientX,
                    startY: e.clientY,
                    claimed: false,
                    base: open ? -SWIPE_REVEAL_PX : 0,
                    offset: open ? -SWIPE_REVEAL_PX : 0,
                  };
                }}
                onPointerMove={(e) => {
                  const g = gestureRef.current;
                  if (!g || g.id !== item.convId) {
                    return;
                  }
                  const dx = e.clientX - g.startX;
                  const dy = e.clientY - g.startY;
                  // 坐标缺失（兜底形态事件）按无法判定处理：放弃手势（不认领
                  // 不跟手——对齐旧交互的防御语义）
                  if (!Number.isFinite(dx) || !Number.isFinite(dy)) {
                    gestureRef.current = null;
                    return;
                  }
                  if (!g.claimed) {
                    // 位移未超判定阈值：继续观察（点选意图，不动）
                    if (
                      Math.abs(dx) < SWIPE_DIRECTION_PX &&
                      Math.abs(dy) < SWIPE_DIRECTION_PX
                    ) {
                      return;
                    }
                    // 纵向占优：放行垂直滚动（touch-action: pan-y 下浏览器接管
                    // 滚动并以 pointercancel 收尾），放弃手势
                    if (Math.abs(dx) <= Math.abs(dy)) {
                      gestureRef.current = null;
                      return;
                    }
                    // 横向占优：认领左滑手势；同时收起其他已开项（一次只开一项）
                    g.claimed = true;
                    if (swipeOpenId && swipeOpenId !== item.convId) {
                      setSwipeOpenId(null);
                    }
                  }
                  // 跟手：位移夹在 [-76, 0]（右滑到头即 0，不过度拉出）
                  g.offset = Math.max(-SWIPE_REVEAL_PX, Math.min(0, g.base + dx));
                  setDrag({ id: g.id, dx: g.offset });
                }}
                onPointerUp={() => {
                  const g = gestureRef.current;
                  gestureRef.current = null;
                  if (!g || g.id !== item.convId || !g.claimed) {
                    return;
                  }
                  // 松手吸附：过吸附阈值（-40）吸附打开，否则回弹关闭；
                  // 吞掉紧随的 click（滑动语义不切会话）
                  swallowClickRef.current = true;
                  setDrag(null);
                  setSwipeOpenId(g.offset < -SWIPE_OPEN_PX ? g.id : null);
                }}
                onPointerCancel={() => {
                  // 浏览器接管（垂直滚动开始）等场景：放弃手势回吸附态
                  if (gestureRef.current?.id === item.convId) {
                    gestureRef.current = null;
                    setDrag(null);
                  }
                }}
                onPointerLeave={() => {
                  // 鼠标拖动中滑出条目：同 pointercancel 放弃
                  if (gestureRef.current?.id === item.convId) {
                    gestureRef.current = null;
                    setDrag(null);
                  }
                }}
                onContextMenu={(e) => {
                  // 触屏按压唤起的系统菜单（iOS 预览/callout）与左滑手势冲突，屏蔽；
                  // 桌面右键同理不给系统菜单。
                  e.preventDefault();
                }}
                aria-current={active ? "page" : undefined}
                title={swipeEnabled ? `${convDisplayName(item.convId)}（左滑删除）` : undefined}
                className={
                  "relative block w-full select-none rounded-xl px-3 py-2 text-left " +
                  // 拖动中关过渡（跟手）；吸附态带过渡（吸附/回弹动画）
                  (dragging ? "" : "transition-all duration-150 ") +
                  (active
                    ? "bg-emerald-600/15 text-ink"
                    : "text-muted hover:bg-panel2 hover:text-ink")
                }
                style={{
                  transform: `translateX(${offset}px)`,
                  WebkitTouchCallout: "none",
                  // 纵向滚动交浏览器（pan-y），横向由本组件 JS 接管
                  touchAction: swipeEnabled ? "pan-y" : undefined,
                }}
              >
                <span className="flex items-center gap-2">
                  {/* v1.37 运行态呼吸点（emerald + animate-pulse，形态随状态条
                      在线点；占位不挤压标题——flex 收缩由 min-w-0 承担） */}
                  {item.running && (
                    <span
                      className="h-2 w-2 shrink-0 animate-pulse rounded-full bg-emerald-400"
                      title="运行中"
                      aria-label="运行中"
                    />
                  )}
                  <span className="min-w-0 flex-1 truncate text-sm font-medium">
                    {item.title || convDisplayName(item.convId)}
                  </span>
                  {/* 未读徽标优先于当前项小圆点（有未读必非当前项） */}
                  {item.unread && item.unread > 0 ? (
                    <span
                      className="flex h-4 min-w-[1rem] shrink-0 items-center justify-center rounded-full bg-emerald-500 px-1 text-[10px] font-semibold leading-none text-white"
                      aria-label={`${item.unread} 条未读`}
                    >
                      {item.unread > 99 ? "99+" : item.unread}
                    </span>
                  ) : (
                    active && (
                      <span
                        className="h-1.5 w-1.5 shrink-0 rounded-full bg-emerald-500"
                        aria-hidden="true"
                      />
                    )
                  )}
                </span>
                <span className="mt-0.5 flex items-baseline gap-2">
                  <span className="min-w-0 flex-1 truncate text-xs text-faint">
                    {item.preview || "（暂无消息）"}
                  </span>
                  <span className="shrink-0 text-[11px] leading-4 text-ghost">
                    {formatTime(item.at)}
                  </span>
                </span>
              </button>
              {/* 桌面（≥md，对齐项目响应式断点先例）hover 兜底：右侧小删除图标
                  （16px 灰垃圾桶，hover 项时 0→1），点击同样打开 App 级确认弹层。
                  触屏（<md）恒隐藏——移动端走左滑；不可见时 pointer-events 关闭
                  防幽灵点击区；开/拖态不渲染（避让露出的删除按钮）。accessible
                  name 用通用「删除会话」：条目本体即上下文，且不带会话名——
                  避免与按会话名/预览文本检索条目的既有测试查询撞名。 */}
              {swipeEnabled && !open && !dragging && (
                <button
                  type="button"
                  onClick={() => onConvDelete?.(item.convId)}
                  aria-label="删除会话"
                  className="pointer-events-none absolute right-2 top-1/2 z-10 -translate-y-1/2 rounded-md bg-panel/90 p-1 text-ghost opacity-0 transition-opacity duration-100 hover:text-err focus-visible:pointer-events-auto focus-visible:opacity-100 md:pointer-events-auto md:group-hover:opacity-100"
                >
                  <svg
                    viewBox="0 0 20 20"
                    className="h-4 w-4"
                    fill="none"
                    stroke="currentColor"
                    strokeWidth="1.6"
                    aria-hidden="true"
                  >
                    <path
                      d="M4 6h12M8 6V4.5A1.5 1.5 0 0 1 9.5 3h1A1.5 1.5 0 0 1 12 4.5V6M6 6l.7 9a1.5 1.5 0 0 0 1.5 1.4h3.6a1.5 1.5 0 0 0 1.5-1.4L14 6"
                      strokeLinecap="round"
                      strokeLinejoin="round"
                    />
                  </svg>
                </button>
              )}
            </div>
          );
        })}
      </nav>

      <div className="border-t border-line p-2">
        {/* v1.2 会话过期横幅：点击进入登出流程（重新登录续期 30 天） */}
        {expiryWarning && onRenewSession && (
          <button
            type="button"
            onClick={onRenewSession}
            className="mb-1.5 w-full rounded-lg border border-amber-400/60 bg-amber-50 px-3 py-1.5 text-left text-xs text-amber-800 transition-colors hover:bg-amber-100 dark:border-amber-500/40 dark:bg-amber-950/60 dark:text-amber-200 dark:hover:bg-amber-900/60"
          >
            会话 {daysLeft} 天后过期，重新登录可续期
          </button>
        )}
        {/* v1.10 Web Push 通知开关（仅会话模式）：右侧状态列显示已开启/
            未开启/不可用；点击由 App 走 enable/disable 订阅流（不可用时给
            引导提示，如 iOS「先添加到主屏幕」） */}
        {onTogglePush && (
          <button
            type="button"
            onClick={onTogglePush}
            disabled={pushBusy}
            className="mb-1.5 flex w-full items-center rounded-lg px-3 py-1.5 text-left text-xs text-muted transition-colors hover:bg-panel2 hover:text-ink disabled:cursor-wait disabled:opacity-60"
          >
            <span>通知</span>
            <span className="ml-auto shrink-0 text-[11px] text-ghost">
              {pushAvailable ? (pushEnabled ? "已开启" : "未开启") : "不可用"}
            </span>
          </button>
        )}
        {/* 主题三态开关（跟随系统/浅色/深色）：aria-pressed 标当前偏好；
            点击由 App 落盘并应用到 document（lib/theme）。分段钮 min-h-10
            保证 ≥40px 触控（对齐 WorkspacePicker chips 的触控约定）。 */}
        {onThemeChange && (
          <div
            role="group"
            aria-label="外观"
            className="mb-1.5 flex w-full items-center justify-between rounded-lg px-3 py-1 text-xs text-muted"
          >
            <span>外观</span>
            <div className="flex items-center gap-0.5">
              {THEME_OPTIONS.map((opt) => {
                const active = themePref === opt.value;
                return (
                  <button
                    key={opt.value}
                    type="button"
                    onClick={() => onThemeChange(opt.value)}
                    aria-pressed={active}
                    className={
                      "press min-h-10 shrink-0 rounded-full px-2.5 text-[11px] transition-colors " +
                      (active
                        ? "bg-emerald-600/15 text-ink"
                        : "text-ghost hover:bg-panel2 hover:text-ink")
                    }
                  >
                    {opt.label}
                  </button>
                );
              })}
            </div>
          </div>
        )}
        {/* v1.5 管理面板入口（仅会话模式——旧 relay 无会话层不显示） */}
        {onOpenAdmin && (
          <button
            type="button"
            onClick={onOpenAdmin}
            className="mb-1.5 w-full rounded-lg px-3 py-1.5 text-left text-xs text-muted transition-colors hover:bg-panel2 hover:text-ink"
          >
            管理
          </button>
        )}
        {/* v1.6：新会话入口改为开 App 级弹层（会话名 + 工作目录选择） */}
        <button
          type="button"
          onClick={onCreate}
          className="w-full rounded-lg border border-dashed border-line-strong px-3 py-2 text-sm text-muted transition-colors hover:border-emerald-500/60 hover:text-ink"
        >
          + 新会话
        </button>
      </div>
    </div>
  );
}
