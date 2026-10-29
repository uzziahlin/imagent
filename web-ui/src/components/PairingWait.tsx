import { countdownText, useNow } from "./useNow";

/** 配对等待码约定有效期（relay pending 5 分钟；pairing_pending 帧不带
 * expiresAt，按到达时刻 + 5 分钟本地近似倒计时）。 */
const PAIRING_TTL_MS = 5 * 60_000;

/**
 * 新设备配对等待屏（v1.5 登录页「我是新设备」入口）：hello{pairing} 发出后
 * relay 回 pairing_pending——本屏以大字展示 6 位等待码 + 倒计时，引导用户在
 * 任一已登录设备上核对同码批准。批准后 relay 在同一连接换发 hello_ack.
 * session，App 切入主界面（本屏自动卸载）；拒绝则连接被关闭回登录页。
 * code = null：hello 已发、等待码未回（中继不可达时由重连循环兜底）。
 */
export function PairingWait({
  device,
  code,
  codeAt,
  onCancel,
}: {
  /** 本机设备名（hello device 字段回显）。 */
  device: string;
  /** 6 位等待码；null = 尚未收到 pairing_pending。 */
  code: string | null;
  /** 等待码到达时刻（ms epoch；倒计时锚点）。 */
  codeAt: number | null;
  onCancel: () => void;
}) {
  const now = useNow();
  const remain = codeAt !== null ? codeAt + PAIRING_TTL_MS - now : null;
  const expired = remain !== null && remain <= 0;

  return (
    <div className="flex min-h-full items-center justify-center px-4">
      <div className="w-full max-w-sm">
        <div className="mb-8 text-center">
          <h1 className="text-xl font-semibold text-ink">新设备配对</h1>
          <p className="mt-1 text-sm text-muted">{device}</p>
        </div>
        <div className="rounded-2xl border border-line bg-panel p-5 shadow-xl">
          {code === null ? (
            <p className="py-8 text-center text-sm text-muted" role="status">
              正在连接中继…
            </p>
          ) : (
            <>
              <p className="mb-2 text-center text-xs leading-relaxed text-muted">
                在任一已登录设备上核对以下代码并批准，本设备将直接进入
              </p>
              <p
                className="select-all text-center font-mono text-5xl font-semibold tracking-[0.35em] text-ink"
                aria-label="配对等待码"
              >
                {code}
              </p>
              <p
                className={`mt-3 text-center text-xs ${expired ? "text-err" : "text-ghost"}`}
                aria-label="配对等待倒计时"
              >
                {expired ? "等待超时（代码已过期），请返回重试" : `有效期内等待批准 ${countdownText(remain ?? 0)}`}
              </p>
            </>
          )}
          <button
            type="button"
            onClick={onCancel}
            className="press mt-5 w-full rounded-xl border border-line-strong py-2 text-sm text-muted transition-colors hover:text-ink"
          >
            取消配对
          </button>
          <p className="mt-3 text-center text-xs leading-relaxed text-faint">
            配对批准需已登录设备通过通行钥匙验证；无已登录设备时请使用访问令牌登录。
          </p>
        </div>
      </div>
    </div>
  );
}
