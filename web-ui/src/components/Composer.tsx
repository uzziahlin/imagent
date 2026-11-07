import { useEffect, useLayoutEffect, useRef, useState, type ClipboardEvent, type KeyboardEvent } from "react";
import type { ConnStatus } from "../lib/ws";
import type { MediaPayload, ModelsInfo } from "../lib/protocol";
import { compressImage } from "../lib/imageCompress";
import { currentModelText, filterCommands, modelChips, type SlashCommand } from "../lib/commands";
import {
  getSpeechRecognitionCtor,
  isIosStandalone,
  useSpeechRecognition,
  type SpeechErrorKind,
} from "./useSpeechRecognition";

/**
 * 输入区自适应高度上限：6 行（text-base 行高 24px × 6 + 上下 padding 20px =
 * 164px）。text-base（16px）：字号 <16px 时 iOS Safari 聚焦会自动放大整页。
 */
const MAX_TEXTAREA_HEIGHT = 164;

/** 占位文案按连接态分流：ready 可用；connecting 过渡态；reconnecting/superseded
 *  明确告知断线（superseded 引导点上方「重新连接」）；e2eBlocked 引导看横幅。 */
const PLACEHOLDER: Record<ConnStatus, string> = {
  ready: "发消息给 Mac…（Enter 发送，/ 唤起命令）",
  connecting: "连接中…（恢复后可发送）",
  reconnecting: "连接中断，恢复后可发送",
  superseded: "已在其它窗口连接，重新连接后可发送",
};

const E2E_BLOCKED_PLACEHOLDER = "加密连接未确认，暂不能发送（见上方提示）";

/** v1.39 语音错误码 → 人话文案（行内 alert 展示；aborted 属主动终止语义，
 *  hook 层静默不回调，此表仅占位全键）。 */
const SPEECH_ERROR_TEXT: Record<SpeechErrorKind, string> = {
  "not-allowed": "麦克风权限被拒，请在系统设置允许后重试",
  "service-not-allowed": "麦克风权限被拒，请在系统设置允许后重试",
  "no-speech": "没有听到说话，请再试一次",
  network: "语音服务网络异常，请检查网络后重试",
  "audio-capture": "未检测到麦克风设备",
  aborted: "",
  unknown: "语音识别出错，请重试",
};

/** v1.39.2：iOS 主屏（standalone）语音受限提示——真机实证主屏模式并非绝对
 *  不支持（首次识别可用，授权后出字），后续会话因 WebKit 连续会话 bug 静默
 *  失败。v1.39 的入口抢先拦截已撤（实证推翻前提）：点 🎤 照常发起识别，仅
 *  静默重试耗尽（v1.39.1 自动重试仍无果）后经 onSilentFailExhausted 呈现
 *  行内 alert（复用既有错误行 + 4s 消退）；title 同步提示。措辞从「不支持」
 *  改「受限」。未命中环境静默耗尽不提示（真实无语音不误报）。 */
const IOS_STANDALONE_HINT = "主屏模式下语音受 iOS 系统限制可能不稳定，建议用 Safari 打开使用";

/**
 * composer：多行输入，Enter 发送 / Shift+Enter 换行（中文输入法组词中的
 * Enter 不发送）；「停止」发 control stop。发送即本地回显（上层 send_echo）。
 * 输入区高度随内容自适应（1-6 行，超出滚动）；底部安全区 padding（刘海屏
 * home bar 场景，移动端优先）。
 * 断线置灰：非 ready 全量禁用（输入/发送/停止）+ 状态化占位文案 + 灰底。
 * v1.4 e2eBlocked：当前机器处于 fail-closed 态（未确认指纹/不支持加密且
 * 未放行/身份告警）→ 同样全量禁用，占位引导看上方横幅。
 * v1.7 媒体上行：📷 选图 → canvas 压缩（lib/imageCompress）→ 输入框上方
 * 缩略预览（可移除）→ 随消息一起发（onSend 第二参 media；纯图消息 text
 * 为空串，发送按钮不因此禁用）。
 * v1.7 命令面板：输入以 / 开头（且未进入参数位——含空白即不弹）→ 输入框
 * 上方浮出命令面板（lib/commands 表按名称前缀/描述过滤）：↑↓ 选中、Tab/
 * Enter 填充（`/name ` 光标留参数位）、点击同效；Esc 或清掉 / 关闭；/model
 * 行附模型快捷 chips（v1.8 动态化：优先当前机器快照 models 段的 options
 * label 前置 default，无快照回退内置 default/sonnet/opus/haiku）+ 行尾
 * 当前模型文本（current / 默认 · cliDefault / 默认）+ 有映射 target 的 chip
 * 加 title 提示。
 * 条目 ≥40px 触控高度、面板 max-h 滚动（移动端友好）。
 * v1.39 语音转文字（P1.5）：Web Speech API（原生，零服务端改动）。支持环境
 * （Chrome / iOS Safari 16.4+）在 📷 旁渲染 🎤 入口，点击开始（zh-CN 单句
 * 模式）：interim 实时小字灰色预览（不进输入框），final 前置空格追加到当前
 * 输入内容——只填入框绝不自动发送；再点停止（stop 优雅收尾保留最后一段）或
 * 静音自动断。错误（权限/没说话/网络）行内 alert 人话提示 + 4s 消退 + 状态
 * 复位。不支持环境（Firefox/旧 iOS）不渲染按钮。iOS 主屏（standalone）模式
 * 照常发起识别（v1.39.2 事实修正：真机实证并非绝对不支持，撤除 v1.39 抢先
 * 拦截）——静默失败由 v1.39.1 自动重试兜底，重试耗尽（WebKit 连续会话 bug
 * 仍无果）经 onSilentFailExhausted 呈现受限提示（仅 iOS 主屏命中，其余环境
 * 静默不误报；title 同步）。逻辑在 useSpeechRecognition（构造器注入，可测）。
 */
export function Composer({
  status,
  e2eBlocked,
  models,
  onSend,
  onStop,
}: {
  status: ConnStatus;
  /** v1.4：当前机器 e2e 发送被阻（状态化占位 + 全量禁用）。 */
  e2eBlocked?: boolean;
  /** v1.8：当前机器快照的模型目录（/model chips 与当前模型文本；缺省内置兜底）。 */
  models?: ModelsInfo;
  /** v1.7：可选 media 随 message 帧发送；text 空串 + media 合法（纯图消息）。 */
  onSend: (text: string, media?: MediaPayload) => boolean;
  onStop: () => void;
}) {
  const disabled = status !== "ready" || e2eBlocked === true;
  const [text, setText] = useState("");
  /** 待发图片（压缩产物；null = 无附件）。 */
  const [media, setMedia] = useState<MediaPayload | null>(null);
  /** 附件处理失败提示（选下一张/发送后清除）。 */
  const [attachError, setAttachError] = useState<string | null>(null);
  /** 命令面板：Esc 显式关闭标记（清掉 / 或重新到空输入时复位）。 */
  const [dismissed, setDismissed] = useState(false);
  /** 面板键盘选中序号（过滤串变化经 onChange 归零）。 */
  const [sel, setSel] = useState(0);
  const taRef = useRef<HTMLTextAreaElement>(null);
  const fileRef = useRef<HTMLInputElement>(null);

  // ---- v1.39 语音转文字（P1.5）：Web Speech API，能力探测决定入口渲染 ----
  /** 构造器惰性探测一次（mount 期间环境不变；测试经 window 注入 stub）。 */
  const [speechCtor] = useState(getSpeechRecognitionCtor);
  /** iOS 主屏（standalone）检测：mount 时探测一次（环境运行期不变；测试经
   *  matchMedia/navigator.standalone/UA 注入）。v1.39.2 不再拦入口——仅作
   *  静默重试耗尽后兜底提示的条件（title 同步受限提示）。 */
  const [iosStandalone] = useState(isIosStandalone);
  /** 语音错误提示（人话文案；4s 自动消退，对齐 App toast 节奏）。 */
  const [speechError, setSpeechError] = useState<string | null>(null);
  const speech = useSpeechRecognition({
    ctor: speechCtor,
    // final 只追加输入框（前置空格拼接，保留已输入内容）——绝不自动发送，
    // 由用户确认后手动发；追加后焦点回输入框便于确认/修改（同 fill 先例）。
    onFinal: (t) => {
      setText((prev) => (prev ? `${prev} ${t}` : t));
      setSel(0);
      setDismissed(false);
      taRef.current?.focus();
    },
    onError: (kind) => setSpeechError(SPEECH_ERROR_TEXT[kind] || SPEECH_ERROR_TEXT.unknown),
    // v1.39.2 静默重试耗尽兜底：iOS 主屏命中 → 受限提示（复用行内 alert +
    // 4s 消退管道）；未命中环境静默——真实无语音等场景不误报。
    onSilentFailExhausted: () => {
      if (iosStandalone) {
        setSpeechError(IOS_STANDALONE_HINT);
      }
    },
  });
  // 语音错误 4s 自动消退（对齐 error/notice toast 节奏）
  useEffect(() => {
    if (!speechError) {
      return;
    }
    const t = setTimeout(() => setSpeechError(null), 4000);
    return () => clearTimeout(t);
  }, [speechError]);

  // 面板查询串：以 / 开头且不含空白（含空白 = 已在参数输入态，不弹）
  const query = text.startsWith("/") && !/\s/.test(text) ? text : null;
  const matches = query !== null && !dismissed ? filterCommands(query) : [];
  const paletteOpen = matches.length > 0;
  const selIdx = Math.min(sel, matches.length - 1);

  // 内容变化即重算高度（发送清空后回缩到 1 行）
  useLayoutEffect(() => {
    const el = taRef.current;
    if (!el) {
      return;
    }
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, MAX_TEXTAREA_HEIGHT)}px`;
  }, [text]);

  const submit = () => {
    const t = text.trim();
    if (!t && !media) {
      return;
    }
    if (onSend(t, media ?? undefined)) {
      setText("");
      setMedia(null);
      setAttachError(null);
      setDismissed(false);
      setSel(0);
    }
  };

  /** 填充命令（`/name ` 尾随空格留参数位）/ 模型 chip（`/model <名> `）。 */
  const fill = (filled: string) => {
    setText(filled);
    setDismissed(false);
    setSel(0);
    taRef.current?.focus();
  };

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (paletteOpen) {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        const n = matches.length;
        setSel(e.key === "ArrowDown" ? (selIdx + 1) % n : (selIdx - 1 + n) % n);
        return;
      }
      if (e.key === "Tab" || (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing)) {
        e.preventDefault();
        fill(`${matches[selIdx].name} `);
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        setDismissed(true);
        return;
      }
    }
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      submit();
    }
  };

  /** 选图 → 压缩 → 待发预览；失败给行内提示（不动既有输入内容）。 */
  const pickImage = (file: File | undefined) => {
    if (!file) {
      return;
    }
    setAttachError(null);
    if (!file.type.startsWith("image/")) {
      setAttachError("仅支持图片文件");
      return;
    }
    compressImage(file)
      .then((m) => setMedia(m))
      .catch(() => setAttachError("图片处理失败，换一张试试"));
  };

  /** v1.37 粘贴图片直进上行管道：剪贴板含图片文件时 preventDefault 并送入
   *  既有选图管道（pickImage：压缩→预览→随帧上行，不复制逻辑）；非图片
   *  文件/纯文本忽略（保持默认粘贴行为）。既有管道为单附件（media 单值），
   *  多文件粘贴取第一张（跟随现有能力边界）。 */
  const onPaste = (e: ClipboardEvent<HTMLTextAreaElement>) => {
    const files = Array.from(e.clipboardData?.files ?? []);
    const image = files.find((f) => f.type.startsWith("image/"));
    if (!image) {
      return;
    }
    e.preventDefault();
    pickImage(image);
  };

  return (
    <div
      className="border-t border-line bg-panel px-3 pt-2 backdrop-blur"
      style={{ paddingBottom: "max(env(safe-area-inset-bottom), 0.5rem)" }}
    >
      <div className="relative mx-auto w-full max-w-3xl">
        {/* v1.7 命令面板：输入框上方浮出（absolute 锚 bottom-full，不挤压
            消息区），max-h 滚动；条目 ≥40px 触控高度 */}
        {paletteOpen && (
          <div
            role="listbox"
            aria-label="命令面板"
            className="absolute bottom-full left-0 right-0 z-40 mb-2 max-h-72 overflow-y-auto overscroll-contain rounded-xl border border-line bg-panel shadow-2xl"
          >
            {matches.map((c, i) => (
              <CommandRow
                key={c.name}
                cmd={c}
                selected={i === selIdx}
                models={models}
                onSelect={() => fill(`${c.name} `)}
                onChip={(m) => fill(`/model ${m} `)}
              />
            ))}
          </div>
        )}

        {/* v1.7 待发图片预览：48px 缩略 + 文件名 + 移除 */}
        {(media || attachError) && (
          <div className="mb-2 flex items-center gap-2 rounded-xl border border-line bg-surface p-2">
            {media && (
              <>
                <img
                  src={`data:${media.mime};base64,${media.b64}`}
                  alt={media.name}
                  className="h-12 w-12 shrink-0 rounded-lg border border-line object-cover"
                />
                <div className="min-w-0 flex-1">
                  <p className="truncate text-xs font-medium text-ink">{media.name}</p>
                  <p className="text-[11px] text-ghost">已压缩，随消息一起发送</p>
                </div>
                <button
                  type="button"
                  onClick={() => setMedia(null)}
                  aria-label="移除图片"
                  className="press shrink-0 rounded-lg p-1.5 text-muted transition-colors hover:bg-panel2 hover:text-err"
                >
                  <svg viewBox="0 0 20 20" className="h-4 w-4" fill="none" stroke="currentColor" strokeWidth="1.8" aria-hidden="true">
                    <line x1="5" y1="5" x2="15" y2="15" strokeLinecap="round" />
                    <line x1="15" y1="5" x2="5" y2="15" strokeLinecap="round" />
                  </svg>
                </button>
              </>
            )}
            {!media && attachError && (
              <p role="alert" className="px-1 text-xs text-err">
                {attachError}
              </p>
            )}
          </div>
        )}

        <div className="flex items-end gap-2">
          {/* v1.7 图片附件入口（accept=image/*；隐藏 file input 由按钮触发） */}
          <input
            ref={fileRef}
            type="file"
            accept="image/*"
            aria-label="选择图片"
            className="hidden"
            onChange={(e) => {
              pickImage(e.target.files?.[0]);
              e.target.value = ""; // 同文件可重复选择
            }}
          />
          <button
            type="button"
            onClick={() => fileRef.current?.click()}
            disabled={disabled}
            title="添加图片（自动压缩后随消息发送）"
            aria-label="添加图片"
            className="press h-11 shrink-0 rounded-xl border border-line-strong px-3 text-lg leading-none transition-colors hover:border-emerald-500/60 disabled:opacity-40 dark:border-slate-700"
          >
            📷
          </button>
          {/* v1.39 语音输入入口：仅 Web Speech API 可用环境渲染（Firefox/旧
              iOS 无按钮）；断线/e2eBlocked 与输入框同规则禁用；录音态切呼吸
              点（与侧栏运行中同款视觉语汇）+ 强调色 */}
          {speech.supported && (
            <button
              type="button"
              onClick={() => speech.toggle()}
              disabled={disabled}
              title={
                iosStandalone
                  ? IOS_STANDALONE_HINT
                  : speech.status === "listening"
                    ? "停止语音输入"
                    : "语音输入（说完自动停）"
              }
              aria-label={speech.status === "listening" ? "停止语音输入" : "开始语音输入"}
              aria-pressed={speech.status === "listening"}
              className="press relative h-11 shrink-0 rounded-xl border border-line-strong px-3 text-lg leading-none transition-colors hover:border-emerald-500/60 disabled:opacity-40 dark:border-slate-700"
            >
              <span aria-hidden="true" className={speech.status === "listening" ? "text-emerald-600 dark:text-emerald-400" : undefined}>
                🎤
              </span>
              {speech.status === "listening" && (
                <span
                  aria-hidden="true"
                  className="absolute right-1 top-1 h-2 w-2 rounded-full bg-emerald-400 animate-pulse"
                />
              )}
            </button>
          )}
          <textarea
            ref={taRef}
            value={text}
            onChange={(e) => {
              const v = e.target.value;
              setText(v);
              setSel(0);
              // 清掉 / 前缀（或清空输入）即解除 Esc 关闭态，下次输入 / 重新弹
              if (!v.startsWith("/")) {
                setDismissed(false);
              }
            }}
            onKeyDown={onKeyDown}
            onPaste={onPaste}
            rows={1}
            placeholder={e2eBlocked ? E2E_BLOCKED_PLACEHOLDER : PLACEHOLDER[status]}
            disabled={disabled}
            aria-label="消息输入框"
            className="max-h-[164px] min-h-[2.75rem] flex-1 resize-none overflow-y-auto rounded-xl border border-line-strong bg-surface px-3 py-2.5 text-base leading-6 text-ink outline-none transition-colors placeholder:text-ghost focus:border-emerald-500 disabled:cursor-not-allowed disabled:border-line disabled:bg-panel2 disabled:opacity-60"
          />
          <button
            onClick={submit}
            disabled={disabled || (!text.trim() && !media)}
            className="press h-11 shrink-0 rounded-xl bg-emerald-600 px-3 text-sm font-semibold text-white transition-colors hover:bg-emerald-500 active:bg-emerald-700 disabled:opacity-40 md:px-4"
          >
            发送
          </button>
          <button
            onClick={onStop}
            disabled={disabled}
            title="停止当前任务（control stop）"
            className="press h-11 shrink-0 rounded-xl border border-line-strong px-3 text-sm text-muted transition-colors hover:border-rose-400/60 hover:text-err disabled:opacity-40 dark:border-slate-700"
          >
            停止
          </button>
        </div>
        {/* v1.39 语音 interim 预览：小字灰色暂显（不进输入框，final 追加后
            清除）；错误行内 alert（人话文案，4s 消退）——两者互斥（错误
            复位时 interim 已清），独立渲染互不纠缠 */}
        {speech.interim && (
          <p className="mt-1.5 truncate px-1 text-xs text-ghost" aria-live="polite">
            🎙 {speech.interim}…
          </p>
        )}
        {speechError && (
          <p role="alert" className="mt-1.5 px-1 text-xs text-err">
            {speechError}
          </p>
        )}
      </div>
    </div>
  );
}

/** 面板单行：命令名 + 参数提示 + 描述；/model 行额外附模型快捷 chips
 *  （v1.8 动态化：modelChips(models) 三层来源，点击直接填 `/model <名> `，
 *  不占用行本身的选择语义；有映射 target 的 chip 加 title 提示）+ 行尾
 *  当前模型文本（currentModelText：current / 默认 · cliDefault / 默认）。 */
function CommandRow({
  cmd,
  selected,
  models,
  onSelect,
  onChip,
}: {
  cmd: SlashCommand;
  selected: boolean;
  models?: ModelsInfo;
  onSelect: () => void;
  onChip: (model: string) => void;
}) {
  return (
    <div
      role="option"
      aria-selected={selected}
      className={`cursor-pointer border-b border-line/60 last:border-b-0 ${
        selected ? "bg-emerald-500/10" : "hover:bg-panel2"
      }`}
      onClick={onSelect}
    >
      <div className="flex min-h-[40px] items-center gap-2 px-3 py-1.5">
        <span className="shrink-0 font-mono text-sm text-ink">
          {cmd.name}
          {cmd.args ? <span className="text-muted"> {cmd.args}</span> : null}
        </span>
        <span className="min-w-0 flex-1 truncate text-right text-xs text-muted">{cmd.desc}</span>
      </div>
      {cmd.name === "/model" && (
        <div className="flex flex-wrap items-center gap-1.5 px-3 pb-2">
          {modelChips(models).map((m) => {
            const target = models?.options.find((o) => o.label === m)?.target;
            return (
              <button
                key={m}
                type="button"
                onClick={(e) => {
                  e.stopPropagation();
                  onChip(m);
                }}
                title={target ? `映射 → ${target}` : undefined}
                className="press rounded-lg border border-line-strong bg-surface px-2 py-1 font-mono text-[11px] text-muted transition-colors hover:border-emerald-500/60 hover:text-ink"
              >
                {m}
              </button>
            );
          })}
          <span className="ml-auto shrink-0 text-[11px] text-ghost">
            当前 {currentModelText(models)}
          </span>
        </div>
      )}
    </div>
  );
}
