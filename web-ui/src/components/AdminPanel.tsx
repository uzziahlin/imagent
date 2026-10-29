import { useState, type FormEvent } from "react";
import {
  machineNameValid,
  machinePubkeyValid,
  normalizeExpiryMs,
  type DeviceInfo,
  type MachineCfg,
  type PasskeyInfo,
} from "../lib/protocol";
import { fingerprint, type E2ePin } from "../lib/e2e";
import { useExitMounted } from "./useExitMounted";
import { CopyButton } from "./CopyButton";

/** 相对时间（v2 设备列表「最后活跃」/ 机器「添加于」）：刚刚 / N 分钟前 /
 *  N 小时前 / N 天前。ts 容错秒级 epoch（与 expiresAt 同一归一）；缺席给占位。 */
export function relativeTime(ts: number | undefined, now = Date.now()): string {
  if (ts === undefined) {
    return "—";
  }
  const diff = now - normalizeExpiryMs(ts);
  if (diff < 60_000) {
    return "刚刚";
  }
  if (diff < 3_600_000) {
    return `${Math.floor(diff / 60_000)} 分钟前`;
  }
  if (diff < 86_400_000) {
    return `${Math.floor(diff / 3_600_000)} 小时前`;
  }
  return `${Math.floor(diff / 86_400_000)} 天前`;
}

/**
 * 管理面板（v1.5：设备管理弹层升级为「管理」大弹层，五区）：
 * 1. 机器管理：machines_cfg 列表（名称/公钥指纹/添加时间）+「添加机器」
 *    （名称 + 64hex 公钥，客户端预校验与 relay 同规）+「移除」（二次确认，
 *    明示将断开该机器网关连接）。机器列表/增删均带 adminToken（App 侧经
 *    ensureAdminToken 自动 re-auth，本组件只递交意图）。
 * 2. 访问令牌：状态说明 +「轮换」（二次确认警告旧令牌立即失效）→ 结果由
 *    App 层 RotatedTokenModal 一次性展示新值。
 * 3. 已登录设备（v1.2 原设备区整块并入）。
 * 4. 通行钥匙（v1.3 原钥匙区并入）。
 * 5. 已信任机器指纹（v1.4 TOFU pin 区并入）。
 *
 * adminBusy = re-auth ceremony 进行中（管理操作入口统一禁用）；adminError =
 * re-auth 失败/过期引导文案；adminActive = adminToken 当前有效（提示免验剩余
 * 窗口）。进出过渡与原设备弹层同构（150ms + useExitMounted 延迟卸载）。
 */
export function AdminPanel({
  open,
  machines,
  adminBusy,
  adminError,
  adminActive,
  webauthnAvailable,
  onRefreshMachines,
  onAddMachine,
  onRemoveMachine,
  onRotateToken,
  devices,
  passkeys,
  passkeyBusy,
  passkeyMessage,
  selfId,
  onRevoke,
  onAddPasskey,
  onRevokePasskey,
  pins,
  onResetPin,
  onClose,
}: {  /** 弹层开关（App 恒挂载本组件，open 只切显隐）。 */
  open: boolean;
  /** null = 尚未加载（re-auth 未完成或 machines_cfg 未回）。 */
  machines: MachineCfg[] | null;
  /** 管理验证（passkey re-auth）进行中。 */
  adminBusy: boolean;
  /** 管理验证失败/过期引导文案；null 不显示。 */
  adminError: string | null;
  /** adminToken 当前有效（5 分钟免再次验证提示）。 */
  adminActive: boolean;
  /** 本机浏览器是否支持 WebAuthn（不支持则管理区给指引）。 */
  webauthnAvailable: boolean;
  /** 重拉机器列表（App 侧 ensureAdminToken → machine_list）。 */
  onRefreshMachines: () => void;
  /** 添加机器（App 侧 ensureAdminToken → machine_add；已过客户端校验）。 */
  onAddMachine: (name: string, pubkey: string) => void;
  /** 移除机器（App 侧 ensureAdminToken → machine_remove）。 */
  onRemoveMachine: (name: string) => void;
  /** 轮换 USER_TOKEN（App 侧 ensureAdminToken → user_token_rotate）。 */
  onRotateToken: () => void;
  /** null = 加载中（打开面板即发 device_list，relay 回 devices 帧更新）。 */
  devices: DeviceInfo[] | null;
  /** null = 加载中（打开面板即发 cred_list，relay 回 creds 帧更新）。 */
  passkeys: PasskeyInfo[] | null;
  /** 注册 ceremony 进行中（「添加通行钥匙」禁用 + 等待系统弹窗）。 */
  passkeyBusy: boolean;
  /** 注册/删除的结果提示（成功/失败一行文案；null 不显示）。 */
  passkeyMessage: string | null;
  /** 本机会话 id（hello_ack.session.id）；与列表项 id 匹配标「本机」。 */
  selfId?: string;
  onRevoke: (device: DeviceInfo) => void;
  /** 「添加通行钥匙」：App 侧走 lib/webauthn 注册流程。 */
  onAddPasskey: () => void;
  onRevokePasskey: (passkey: PasskeyInfo) => void;
  /** v1.4 已信任机器指纹（TOFU pin 表快照；空表 = 尚无信任机器）。 */
  pins: E2ePin[];
  /** v1.4 重置机器 pin（重置后该机器重新走 TOFU 指纹确认）。 */
  onResetPin: (machine: string) => void;
  onClose: () => void;
}) {
  const mounted = useExitMounted(open, 150);
  // 添加机器表单（客户端预校验与 relay 同规：名称 1..=32 [a-zA-Z0-9_-]、
  // 公钥 64 hex——服务端仍有终判，错误经 error{machine_invalid} 回显）
  const [newName, setNewName] = useState("");
  const [newPubkey, setNewPubkey] = useState("");
  const [formError, setFormError] = useState("");
  // 移除机器的二次确认行（name；null = 无确认中）
  const [removing, setRemoving] = useState<string | null>(null);
  // 令牌轮换二次确认
  const [rotating, setRotating] = useState(false);

  if (!open && !mounted) {
    return null;
  }

  const submitAdd = (e: FormEvent) => {
    e.preventDefault();
    const name = newName.trim();
    const pubkey = newPubkey.trim().toLowerCase();
    if (!machineNameValid(name)) {
      setFormError("机器名须为 1-32 位字母/数字/_/-");
      return;
    }
    if (!machinePubkeyValid(pubkey)) {
      setFormError("公钥须为 64 位 hex（ed25519 公钥）");
      return;
    }
    setFormError("");
    setNewName("");
    setNewPubkey("");
    onAddMachine(name, pubkey);
  };

  return (
    <div
      className="fixed inset-0 z-50"
      role={open ? "dialog" : undefined}
      aria-modal={open ? "true" : undefined}
      aria-label={open ? "管理" : undefined}
    >
      <div
        className={`absolute inset-0 bg-black/50 transition-opacity duration-150 ease-out ${
          open ? "opacity-100" : "opacity-0"
        }`}
        onClick={onClose}
        aria-hidden="true"
      />
      <div
        className={`absolute left-1/2 top-1/2 flex max-h-[85vh] w-[calc(100%-2rem)] max-w-lg -translate-x-1/2 -translate-y-1/2 flex-col rounded-2xl border border-line bg-panel shadow-2xl transition-[opacity,transform,visibility] duration-150 ease-out ${
          open ? "visible scale-100 opacity-100" : "invisible scale-95 opacity-0"
        }`}
      >
        <div className="flex shrink-0 items-center px-4 pb-2 pt-3.5">
          <h2 className="text-sm font-semibold text-ink">管理</h2>
          <span className="ml-2 text-[11px] text-ghost">
            {adminBusy
              ? "验证通行钥匙…"
              : adminActive
                ? "已通过管理验证（5 分钟内免再次验证）"
                : "管理操作需通行钥匙验证"}
          </span>
          <button
            type="button"
            onClick={onClose}
            aria-label="关闭管理面板"
            className="press ml-auto rounded-lg p-1.5 text-muted transition-colors hover:bg-panel2 hover:text-ink"
          >
            <svg viewBox="0 0 20 20" className="h-4 w-4" fill="none" stroke="currentColor" strokeWidth="1.8" aria-hidden="true">
              <line x1="5" y1="5" x2="15" y2="15" strokeLinecap="round" />
              <line x1="15" y1="5" x2="5" y2="15" strokeLinecap="round" />
            </svg>
          </button>
        </div>

        <div className="min-h-0 flex-1 space-y-4 overflow-y-auto overscroll-contain px-4 pb-4">
          {adminError && (
            <p
              role="alert"
              className="rounded-lg border border-rose-400/50 bg-rose-50 px-3 py-2 text-xs leading-relaxed text-rose-700 dark:border-rose-500/40 dark:bg-rose-950/60 dark:text-rose-200"
            >
              {adminError}
            </p>
          )}

          {/* 一、机器管理（v1.5）：列表 + 添加 + 移除（断线提示）。 */}
          <section aria-label="机器管理">
            <div className="mb-2 flex items-center">
              <h3 className="text-xs font-semibold uppercase tracking-wider text-ghost">机器管理</h3>
              <button
                type="button"
                onClick={onRefreshMachines}
                disabled={adminBusy}
                className="press ml-auto rounded-lg border border-line-strong px-2 py-1 text-xs text-muted transition-colors hover:border-emerald-500/60 hover:text-ink disabled:opacity-60"
              >
                刷新
              </button>
            </div>
            {!webauthnAvailable ? (
              <p className="py-1 text-[11px] leading-relaxed text-ghost">
                本浏览器不支持通行钥匙，无法进行管理验证（机器管理需在支持设备上操作）。
              </p>
            ) : machines === null ? (
              <p className="py-2 text-center text-xs text-ghost">
                {adminBusy ? "等待管理验证…" : "加载中…"}
              </p>
            ) : machines.length === 0 ? (
              <p className="py-1 text-[11px] leading-relaxed text-ghost">
                尚未配置机器。网关需在 relay 登记公钥后方可接入。
              </p>
            ) : (
              <ul className="max-h-52 space-y-1 overflow-y-auto overscroll-contain" aria-label="机器配置列表">
                {machines.map((m) => (
                  <li key={m.name} className="rounded-xl border border-line bg-surface px-3 py-2">
                    <div className="flex items-center gap-2">
                      <svg viewBox="0 0 20 20" className="h-4 w-4 shrink-0 text-emerald-500" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
                        <rect x="3" y="4" width="14" height="9" rx="1.5" />
                        <path d="M7 16h6" strokeLinecap="round" />
                      </svg>
                      <span className="min-w-0 flex-1 truncate font-mono text-sm text-ink">{m.name}</span>
                      <span className="shrink-0 text-[11px] text-ghost">添加于 {relativeTime(m.created)}</span>
                      {removing === m.name ? (
                        <span className="flex shrink-0 gap-1">
                          <button
                            type="button"
                            onClick={() => {
                              setRemoving(null);
                              onRemoveMachine(m.name);
                            }}
                            disabled={adminBusy}
                            className="press rounded-lg border border-rose-400/70 px-2 py-1 text-xs text-rose-600 transition-colors hover:bg-rose-500/10 disabled:opacity-60 dark:border-rose-500/50 dark:text-rose-300"
                          >
                            确认移除
                          </button>
                          <button
                            type="button"
                            onClick={() => setRemoving(null)}
                            className="press rounded-lg border border-line-strong px-2 py-1 text-xs text-muted transition-colors hover:text-ink"
                          >
                            取消
                          </button>
                        </span>
                      ) : (
                        <button
                          type="button"
                          onClick={() => setRemoving(m.name)}
                          disabled={adminBusy}
                          className="press shrink-0 rounded-lg border border-line-strong px-2 py-1 text-xs text-muted transition-colors hover:border-rose-400/60 hover:text-rose-600 disabled:opacity-60 dark:hover:border-rose-500/50 dark:hover:text-rose-300"
                        >
                          移除
                        </button>
                      )}
                    </div>
                    <div className="mt-1 font-mono text-[11px] tracking-wider text-ghost" aria-label={`${m.name} 公钥指纹`}>
                      {m.pubkey_fp}
                    </div>
                    {removing === m.name && (
                      <p className="mt-1 text-[11px] leading-relaxed text-amber-700 dark:text-amber-300">
                        移除后该机器的网关连接将被立即断开，且无法再接入（可重新添加）。
                      </p>
                    )}
                  </li>
                ))}
              </ul>
            )}
            <form onSubmit={submitAdd} className="mt-2 space-y-1.5" aria-label="添加机器">
              <div className="flex gap-1.5">
                <input
                  value={newName}
                  onChange={(e) => {
                    setNewName(e.target.value);
                    setFormError("");
                  }}
                  placeholder="机器名（a-z0-9_-）"
                  aria-label="机器名"
                  className="w-32 shrink-0 rounded-lg border border-line-strong bg-surface px-2.5 py-1.5 text-base text-ink outline-none placeholder:text-ghost focus:border-emerald-500"
                />
                <input
                  value={newPubkey}
                  onChange={(e) => {
                    setNewPubkey(e.target.value);
                    setFormError("");
                  }}
                  placeholder="网关公钥（64 位 hex）"
                  aria-label="机器公钥"
                  className="min-w-0 flex-1 rounded-lg border border-line-strong bg-surface px-2.5 py-1.5 font-mono text-base text-ink outline-none placeholder:text-ghost focus:border-emerald-500"
                />
                <button
                  type="submit"
                  disabled={adminBusy || webauthnAvailable === false}
                  className="press shrink-0 rounded-lg bg-emerald-600 px-2.5 py-1.5 text-xs font-semibold text-white transition-colors hover:bg-emerald-500 disabled:opacity-60"
                >
                  添加机器
                </button>
              </div>
              {formError && <p className="text-xs text-err">{formError}</p>}
            </form>
          </section>

          {/* 二、访问令牌（v1.5）：轮换（二次确认；新值一次性弹层展示）。 */}
          <section className="border-t border-line pt-3" aria-label="访问令牌">
            <h3 className="mb-2 text-xs font-semibold uppercase tracking-wider text-ghost">访问令牌</h3>
            <p className="text-[11px] leading-relaxed text-ghost">
              当前 USER_TOKEN 正常使用中（新设备首次登录 / 配对均依赖它）。轮换将立即使旧令牌失效。
            </p>
            {rotating ? (
              <div className="mt-2 rounded-xl border border-amber-400/60 bg-amber-50 px-3 py-2 text-[11px] leading-relaxed text-amber-800 dark:border-amber-500/40 dark:bg-amber-950/60 dark:text-amber-200">
                <p>轮换后旧访问令牌立即失效：尚未完成首次登录的设备将无法用旧令牌进入（已登录会话不受影响）。</p>
                <div className="mt-1.5 flex gap-1.5">
                  <button
                    type="button"
                    onClick={() => {
                      setRotating(false);
                      onRotateToken();
                    }}
                    disabled={adminBusy}
                    className="press rounded-lg bg-amber-600 px-2.5 py-1 text-xs font-semibold text-white transition-colors hover:bg-amber-500 disabled:opacity-60"
                  >
                    确认轮换
                  </button>
                  <button
                    type="button"
                    onClick={() => setRotating(false)}
                    className="press rounded-lg border border-line-strong px-2.5 py-1 text-xs text-muted transition-colors hover:text-ink"
                  >
                    取消
                  </button>
                </div>
              </div>
            ) : (
              <button
                type="button"
                onClick={() => setRotating(true)}
                disabled={adminBusy || !webauthnAvailable}
                className="press mt-2 rounded-lg border border-line-strong px-2.5 py-1 text-xs text-muted transition-colors hover:border-amber-500/60 hover:text-ink disabled:opacity-60"
              >
                轮换访问令牌
              </button>
            )}
          </section>

          {/* 三、已登录设备（v1.2 原设备区并入）。 */}
          <section className="border-t border-line pt-3" aria-label="已登录设备">
            <h3 className="mb-2 text-xs font-semibold uppercase tracking-wider text-ghost">已登录设备</h3>
            {devices === null ? (
              <p className="py-1 text-center text-xs text-ghost">加载中…</p>
            ) : devices.length === 0 ? (
              <p className="py-1 text-center text-xs text-ghost">暂无已登录设备</p>
            ) : (
              <ul className="max-h-44 space-y-1 overflow-y-auto overscroll-contain" aria-label="设备列表">
                {devices.map((d) => {
                  const self = d.id === selfId;
                  return (
                    <li
                      key={d.id}
                      className="flex items-center gap-2.5 rounded-xl border border-line bg-surface px-3 py-2"
                    >
                      <span
                        className={`h-2 w-2 shrink-0 rounded-full ${
                          d.connected ? "bg-emerald-400" : "bg-slate-500"
                        }`}
                        title={d.connected ? "在线" : "离线"}
                        aria-label={d.connected ? "在线" : "离线"}
                      />
                      <div className="min-w-0 flex-1">
                        <div className="flex items-center gap-1.5">
                          <span className="min-w-0 truncate text-sm text-ink">{d.name}</span>
                          {self && (
                            <span className="shrink-0 rounded bg-emerald-600/15 px-1 py-px text-[10px] font-medium text-emerald-700 dark:text-emerald-300">
                              本机
                            </span>
                          )}
                        </div>
                        <div className="text-[11px] text-ghost">最后活跃 {relativeTime(d.lastSeen)}</div>
                      </div>
                      <button
                        type="button"
                        onClick={() => onRevoke(d)}
                        className={
                          "press shrink-0 rounded-lg border px-2 py-1 text-xs transition-colors " +
                          (self
                            ? "border-rose-400/70 text-rose-600 hover:bg-rose-500/10 dark:border-rose-500/50 dark:text-rose-300 dark:hover:bg-rose-500/15"
                            : "border-line-strong text-muted hover:border-rose-400/60 hover:text-rose-600 dark:hover:border-rose-500/50 dark:hover:text-rose-300")
                        }
                      >
                        {self ? "吊销并退出" : "吊销"}
                      </button>
                    </li>
                  );
                })}
              </ul>
            )}
          </section>

          {/* 四、通行钥匙（v1.3 原钥匙区并入）。 */}
          <section className="border-t border-line pt-3" aria-label="通行钥匙">
            <div className="mb-2 flex items-center">
              <h3 className="text-xs font-semibold uppercase tracking-wider text-ghost">通行钥匙</h3>
              {webauthnAvailable && (
                <button
                  type="button"
                  onClick={onAddPasskey}
                  disabled={passkeyBusy}
                  className="press ml-auto rounded-lg border border-line-strong px-2 py-1 text-xs text-muted transition-colors hover:border-emerald-500/60 hover:text-ink disabled:opacity-60"
                >
                  {passkeyBusy ? "等待系统弹窗…" : "添加通行钥匙"}
                </button>
              )}
            </div>
            {!webauthnAvailable ? (
              <p className="py-1 text-[11px] leading-relaxed text-ghost">
                本浏览器不支持通行钥匙（或非安全上下文），登录请使用访问令牌。
              </p>
            ) : passkeys === null ? (
              <p className="py-1 text-center text-xs text-ghost">加载中…</p>
            ) : passkeys.length === 0 ? (
              <p className="py-1 text-[11px] leading-relaxed text-ghost">
                尚未注册通行钥匙。添加后可在登录页免令牌一步登录。
              </p>
            ) : (
              <ul className="max-h-40 space-y-1 overflow-y-auto overscroll-contain" aria-label="通行钥匙列表">
                {passkeys.map((p) => (
                  <li
                    key={p.id}
                    className="flex items-center gap-2.5 rounded-xl border border-line bg-surface px-3 py-2"
                  >
                    <svg viewBox="0 0 20 20" className="h-4 w-4 shrink-0 text-emerald-500" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
                      <circle cx="7" cy="10" r="3.2" />
                      <path d="M10.2 10h6.3M14 10v2.2M16.5 10v1.6" strokeLinecap="round" />
                    </svg>
                    <div className="min-w-0 flex-1">
                      <div className="min-w-0 truncate text-sm text-ink">{p.label}</div>
                      <div className="text-[11px] text-ghost">注册于 {relativeTime(p.created)}</div>
                    </div>
                    <button
                      type="button"
                      onClick={() => onRevokePasskey(p)}
                      className="press shrink-0 rounded-lg border border-line-strong px-2 py-1 text-xs text-muted transition-colors hover:border-rose-400/60 hover:text-rose-600 dark:hover:border-rose-500/50 dark:hover:text-rose-300"
                    >
                      删除
                    </button>
                  </li>
                ))}
              </ul>
            )}
            {passkeyMessage && <p className="mt-2 text-[11px] leading-relaxed text-faint">{passkeyMessage}</p>}
          </section>

          {/* 五、已信任机器指纹（v1.4 TOFU pin 区并入）。 */}
          <section className="border-t border-line pt-3" aria-label="已信任机器指纹">
            <h3 className="mb-2 text-xs font-semibold uppercase tracking-wider text-ghost">已信任机器指纹</h3>
            {pins.length === 0 ? (
              <p className="py-1 text-[11px] leading-relaxed text-ghost">
                尚未信任任何机器。首次加密连接时会请求确认机器指纹。
              </p>
            ) : (
              <ul className="max-h-44 space-y-1 overflow-y-auto overscroll-contain" aria-label="已信任机器指纹列表">
                {pins.map((p) => (
                  <li
                    key={p.machine}
                    className="rounded-xl border border-line bg-surface px-3 py-2"
                  >
                    <div className="flex items-center gap-2">
                      <svg viewBox="0 0 20 20" className="h-4 w-4 shrink-0 text-emerald-500" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
                        <rect x="4.5" y="8.5" width="11" height="7.5" rx="2" />
                        <path d="M7 8.5V6.5a3 3 0 0 1 6 0v2" strokeLinecap="round" />
                      </svg>
                      <span className="min-w-0 flex-1 truncate text-sm text-ink">{p.machine}</span>
                      <span className="shrink-0 text-[11px] text-ghost">{relativeTime(p.pinnedAt)}</span>
                      <button
                        type="button"
                        onClick={() => onResetPin(p.machine)}
                        className="press shrink-0 rounded-lg border border-line-strong px-2 py-1 text-xs text-muted transition-colors hover:border-rose-400/60 hover:text-rose-600 dark:hover:border-rose-500/50 dark:hover:text-rose-300"
                      >
                        重置
                      </button>
                    </div>
                    <div className="mt-1 space-y-0.5 font-mono text-[11px] tracking-wider text-ghost">
                      <div aria-label={`${p.machine} 网关密钥指纹`}>X25519 {fingerprint(p.gk)}</div>
                      <div aria-label={`${p.machine} 身份密钥指纹`}>Ed25519 {fingerprint(p.ik)}</div>
                    </div>
                  </li>
                ))}
              </ul>
            )}
          </section>

          <p className="text-[11px] leading-relaxed text-faint">
            管理操作（机器增删 / 令牌轮换 / 配对批准）均需通行钥匙再验证，5 分钟内免重复验证（仅本页内存，刷新即失效）。吊销本机设备将退出当前登录。
          </p>
        </div>
      </div>
    </div>
  );
}

/**
 * USER_TOKEN 轮换结果弹层（v1.5）：user_token_rotated 的新值**仅此一次**返回
 * 明文——本弹层大字展示 + 复制按钮，明示存入 Mac 的 ~/.imagent/web-relay.env。
 * 关闭即作废（SPA 不落任何存储）；alertdialog 语义（不可点遮罩关闭，防误失）。
 */
export function RotatedTokenModal({ token, onDone }: { token: string; onDone: () => void }) {
  return (
    <div
      className="fixed inset-0 z-[60] bg-black/60"
      role="alertdialog"
      aria-modal="true"
      aria-label="访问令牌已轮换"
    >
      <div className="absolute left-1/2 top-1/2 w-[calc(100%-2rem)] max-w-md -translate-x-1/2 -translate-y-1/2 rounded-2xl border border-amber-400/60 bg-panel p-4 shadow-2xl">
        <div className="mb-2 flex items-center gap-2">
          <svg viewBox="0 0 20 20" className="h-5 w-5 shrink-0 text-amber-500" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
            <path d="M10 3.5 17 6v4.5c0 3.4-2.9 5.4-7 6.5-4.1-1.1-7-3.1-7-6.5V6z" />
          </svg>
          <h2 className="text-sm font-semibold text-ink">访问令牌已轮换</h2>
        </div>
        <p className="mb-3 text-xs leading-relaxed text-muted">
          新令牌<strong className="text-amber-600 dark:text-amber-400">仅显示这一次</strong>
          ，关闭后无法再查看。请立即复制并存入 Mac 的{" "}
          <code className="rounded bg-panel2 px-1 py-px text-[11px]">~/.imagent/web-relay.env</code>{" "}
          （USER_TOKEN），旧令牌已失效。
        </p>
        <div className="flex items-center gap-2 rounded-xl border border-line bg-surface px-3 py-2.5">
          <code className="min-w-0 flex-1 break-all font-mono text-xs leading-relaxed text-ink" aria-label="新访问令牌">
            {token}
          </code>
          <CopyButton getText={() => token} label="复制令牌" copiedLabel="已复制" />
        </div>
        <button
          type="button"
          onClick={onDone}
          className="press mt-3 w-full rounded-xl bg-amber-600 py-2 text-sm font-semibold text-white transition-colors hover:bg-amber-500"
        >
          我已保存，关闭
        </button>
      </div>
    </div>
  );
}
