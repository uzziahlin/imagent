import { useCallback, useEffect, useRef, useState } from "react";

/**
 * v1.39 语音转文字（P1.5）：浏览器原生 Web Speech API 封装（零成本、零服务
 * 端改动；Chrome 内置 / iOS Safari 16.4+，Firefox 与旧 iOS 不支持——由调用方
 * 据 supported 不渲染入口）。设计约定（组件薄、hook 可测）：
 * - SpeechRecognition 构造器经参数注入（Composer 从 window 探测传入；测试
 *   stub 直接传类）；
 * - 单句模式（continuous=false）：静音自动断流，end 事件自然归位 idle；
 * - interim 文本只进预览 state（不进输入框），final 经 onFinal 交调用方决定
 *   去向（任务钉死：只填输入框，绝不自动发送）；
 * - 错误经 onError 抛归一化 kind，人话文案/提示形态由调用方映射（aborted
 *   属主动终止语义，静默不回调）；
 * - unmount 清理：abort + 事件解绑（teardown 幂等，StrictMode 双挂载安全）；
 * - v1.39.1 静默失败自动重试：会话 end 时若非手动 stop 且本会话无 result
 *   无 error（WebKit 静默失败特征），400ms 后换新实例自动重试一次——期间
 *   保持 listening，UI 无感；重试会话再静默失败不循环，归位 idle（每轮用户
 *   点击至多 1 次自动重试）。start 同步抛错经 onError("unknown") 显式反馈。
 * - v1.39.2 静默重试耗尽兜底回调 onSilentFailExhausted：重试会话仍静默 end
 *   （retried 已置位的 silentFail 路径，直接归位 idle）时回调一次——是否
 *   可见提示由调用方决定；非静默路径（有 result / 有 error / 手动 stop）
 *   不回调。真机实证 iOS 主屏并非绝对不支持（首次识别可用），入口不拦、
 *   失败后兜底。
 */

// ---------------------------------------------------------------------------
// 最小 Web Speech 类型面：TS 5.6 的 lib.dom 无 SpeechRecognition 声明，只
// 声明本功能用到的字段（命名对齐 W3C 规范）；带 Like 后缀避免未来 lib.dom
// 内置全局类型后的命名合并冲突。不涉 protocol.ts。
// ---------------------------------------------------------------------------

export interface SpeechRecognitionAlternativeLike {
  transcript: string;
}

export interface SpeechRecognitionResultLike {
  isFinal: boolean;
  length: number;
  [index: number]: SpeechRecognitionAlternativeLike;
}

export interface SpeechRecognitionResultListLike {
  length: number;
  [index: number]: SpeechRecognitionResultLike;
}

/** result 事件（只含用到字段）。 */
export interface SpeechRecognitionResultEventLike {
  /** 本事件从 results 的哪个下标开始是新增结果（增量语义）。 */
  resultIndex: number;
  results: SpeechRecognitionResultListLike;
}

/** error 事件（error 为 W3C 错误码：not-allowed / no-speech / network / ...）。 */
export interface SpeechRecognitionErrorEventLike {
  error: string;
}

export interface SpeechRecognitionLike {
  lang: string;
  continuous: boolean;
  interimResults: boolean;
  start(): void;
  stop(): void;
  abort(): void;
  onresult: ((ev: SpeechRecognitionResultEventLike) => void) | null;
  onerror: ((ev: SpeechRecognitionErrorEventLike) => void) | null;
  onend: (() => void) | null;
}

export type SpeechRecognitionCtor = new () => SpeechRecognitionLike;

/** 能力探测：window.SpeechRecognition || window.webkitSpeechRecognition；
 *  缺席 = 不支持环境（Firefox / 旧 iOS），调用方不渲染麦克风入口。 */
export function getSpeechRecognitionCtor(): SpeechRecognitionCtor | null {
  const w = window as unknown as {
    SpeechRecognition?: SpeechRecognitionCtor;
    webkitSpeechRecognition?: SpeechRecognitionCtor;
  };
  return w.SpeechRecognition ?? w.webkitSpeechRecognition ?? null;
}

/**
 * iOS 主屏（standalone）模式检测——v1.39.2 事实修正（用户真机实证推翻 v1.39
 * 抢先拦截的前提）：主屏模式并非绝对不支持，首次识别可用（授权后出字），
 * 后续会话才静默失败（WebKit 连续会话 bug，由重试机制处理）。检测保留但不
 * 再拦入口：仅作静默重试耗尽后兜底提示的条件（命中 → 提示受 iOS 系统限制，
 * 未命中 → 静默，真实无语音不误报）。两条件与：
 * - iOS：UA 含 iPhone/iPad/iPod。iPadOS 13+ 默认桌面 UA（Macintosh）的
 *   角落情况不追求——该形态按桌面环境处理（不提示），粒度与「不支持环境
 *   不渲染」一致；
 * - standalone：标准 display-mode: standalone（matchMedia）或 Safari 专有
 *   navigator.standalone === true（iOS 无 display-mode 支持时的唯一信号）
 *   ——两判或。
 * matchMedia 缺席（旧环境/jsdom）按 false 计不抛错——本函数在每次 Composer
 * 挂载时无条件求值，先于任何能力探测。
 */
export function isIosStandalone(): boolean {
  if (!/iP(hone|ad|od)/.test(window.navigator.userAgent)) {
    return false;
  }
  const standalone =
    (typeof window.matchMedia === "function" &&
      window.matchMedia("(display-mode: standalone)").matches) ||
    (window.navigator as Navigator & { standalone?: boolean }).standalone === true;
  return standalone;
}

export type SpeechStatus = "idle" | "listening";

/** error 码归一后的错误类（未知码并入 unknown；aborted = 主动终止语义）。 */
export type SpeechErrorKind =
  | "not-allowed"
  | "service-not-allowed"
  | "no-speech"
  | "network"
  | "audio-capture"
  | "aborted"
  | "unknown";

const normalizeErrorKind = (code: string): SpeechErrorKind =>
  code === "not-allowed" ||
  code === "service-not-allowed" ||
  code === "no-speech" ||
  code === "network" ||
  code === "audio-capture" ||
  code === "aborted"
    ? code
    : "unknown";

export interface UseSpeechRecognitionOptions {
  /** 识别构造器（null = 不支持；toggle/stop 成为 no-op）。 */
  ctor: SpeechRecognitionCtor | null;
  /** 识别语言（缺省 zh-CN——驾驶舱主语言）。 */
  lang?: string;
  /** final 文本回调（聚合单次 result 事件内全部 final 段后一次回调，trim 过）。 */
  onFinal?: (text: string) => void;
  /** 错误回调（aborted 静默不回调——主动终止/清理语义，不算故障）。 */
  onError?: (kind: SpeechErrorKind) => void;
  /** v1.39.2 静默重试耗尽兜底：重试会话仍静默 end（无 result 无 error，归
   *  位 idle）时回调一次；非静默路径（有 result / 有 error / 手动 stop）
   *  不回调。可见与否由调用方决定（Composer 仅 iOS 主屏命中时提示）。 */
  onSilentFailExhausted?: () => void;
}

export function useSpeechRecognition({
  ctor,
  lang = "zh-CN",
  onFinal,
  onError,
  onSilentFailExhausted,
}: UseSpeechRecognitionOptions) {
  const [status, setStatus] = useState<SpeechStatus>("idle");
  /** interim 预览文本（null = 无预览；final 到达或结束时清空）。 */
  const [interim, setInterim] = useState<string | null>(null);
  const recRef = useRef<SpeechRecognitionLike | null>(null);
  // 回调经 ref 转发：事件监听器闭包固定引用最新回调（实例生命周期内调用方
  // 传入的函数身份变化不触发重绑，无陈旧闭包）。
  const cbsRef = useRef({ onFinal, onError, onSilentFailExhausted });
  cbsRef.current = { onFinal, onError, onSilentFailExhausted };

  // ---- v1.39.1 静默失败自动重试（WebKit 确认 bug：旧音频会话未完全释放，
  // 紧接的 start 静默失败——无 error 无 result 即 end；社区解法 end 后延迟
  // 重启 + 失败重试一次）----
  /** 重试延迟定时器（400ms）：teardown/unmount 与窗口内停止点击均清除，防
   *  迟到 start 竞态。 */
  const retryTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // 会话级标志（每次 start 新会话重置）：
  /** 本会话是否收到过任何 result 事件（onresult 一到即 true）。 */
  const gotResultRef = useRef(false);
  /** 本会话是否发生过 error（含 aborted——错误会话一律不自动重试）。 */
  const erroredRef = useRef(false);
  /** 用户本会话是否手动 stop（end 处理后清除——主动停止绝不自动重启）。 */
  const manualStopRef = useRef(false);
  /** 本轮用户意图内已用掉的自动重试（至多 1 次防循环；toggle 新点击重置）。 */
  const retriedRef = useRef(false);

  /** 收尾：重试定时器清除 + 事件解绑 + abort + UI 态复位（幂等；end/error
   *  复位/unmount 共用）。 */
  const teardown = useCallback(() => {
    if (retryTimerRef.current !== null) {
      clearTimeout(retryTimerRef.current);
      retryTimerRef.current = null;
    }
    const rec = recRef.current;
    recRef.current = null;
    if (rec) {
      rec.onresult = null;
      rec.onerror = null;
      rec.onend = null;
      try {
        rec.abort();
      } catch {
        // 个别实现对已停止实例 abort 抛错——收尾路径吞掉
      }
    }
    setStatus("idle");
    setInterim(null);
  }, []);

  // unmount 清理（任务约束：abort + 事件解绑；返回 teardown 作 cleanup，
  // StrictMode 双挂载下 recRef 为 null 时为 no-op）。
  useEffect(() => teardown, [teardown]);

  const start = useCallback(() => {
    if (!ctor || recRef.current) {
      return;
    }
    // 防御清除残留重试定时器（现路径 toggle 窗口分支已兜住；防未来新增
    // start 调用点把「窗口内迟到 start」变成双会话竞态）
    if (retryTimerRef.current !== null) {
      clearTimeout(retryTimerRef.current);
      retryTimerRef.current = null;
    }
    // 新会话重置会话级标志（retried 例外：属用户意图级，仅在 toggle 新点击
    // 重置——重试经 start 直入，靠它记住「本轮已重试过」防无限循环）
    gotResultRef.current = false;
    erroredRef.current = false;
    manualStopRef.current = false;
    const rec = new ctor();
    rec.lang = lang;
    rec.interimResults = true;
    rec.continuous = false; // 单句：静音自动断（end 自然归位）
    rec.onresult = (ev) => {
      gotResultRef.current = true; // 任何 result 事件即证明本会话有效（重试判定用）
      let finalBuf = "";
      let interimBuf = "";
      // 增量遍历（resultIndex 起）；alternative 是互斥候选，只取第 0 候选
      for (let i = ev.resultIndex; i < ev.results.length; i++) {
        const r = ev.results[i];
        const t = r.length > 0 ? r[0].transcript : "";
        if (r.isFinal) {
          finalBuf += t;
        } else {
          interimBuf += t;
        }
      }
      const f = finalBuf.trim();
      if (f) {
        cbsRef.current.onFinal?.(f);
      }
      const it = interimBuf.trim();
      setInterim(it || null);
    };
    rec.onerror = (ev) => {
      erroredRef.current = true; // 错误会话一律不自动重试（含 aborted，错误自有文案）
      const kind = normalizeErrorKind(ev.error);
      if (kind !== "aborted") {
        cbsRef.current.onError?.(kind);
      }
      // UI 态先行复位兜底（规范 error 后必跟 end，teardown 幂等）；实例不
      // 在此 abort/解绑——stop() 优雅路径的收尾 result 事件不能丢
      setStatus("idle");
      setInterim(null);
    };
    // v1.39.1 end 判定（重试状态机）：三条件与=静默失败特征——非手动 stop +
    // 无 result + 无 error；叠加本轮重试额度未用（重试会话再静默失败不再重
    // 试，避免循环——用户再点一次 🎤 即新一轮）。静默失败不回调 onError（不
    // 误报：真实无语音等静默场景交给下方耗尽兜底回调，明确错误走 no-speech
    // 等文案路径）。
    rec.onend = () => {
      const silentFail =
        !manualStopRef.current && !gotResultRef.current && !erroredRef.current;
      const allowRetry = silentFail && !retriedRef.current;
      manualStopRef.current = false; // end 处理后清除，不残留到下轮会话
      teardown(); // recRef 置空 → 重试直接走 start 新实例逻辑
      if (allowRetry) {
        retriedRef.current = true;
        // 重试延迟窗口 UI 无感：归位后立即拉回 listening（同一事件内 React
        // 批处理，无闪烁），呼吸点不断；窗口内用户点击/卸载经 toggle/
        // teardown 清定时器，防迟到 start 抢跑用户意图
        setStatus("listening");
        retryTimerRef.current = setTimeout(() => {
          retryTimerRef.current = null;
          start(); // 自引用安全：ctor/lang/teardown 每挂载稳定，触发时即最新实现
        }, 400);
      } else if (silentFail) {
        // v1.39.2 耗尽兜底挂点：retried 已置位的静默失败路径（重试会话仍
        // 静默 end，现直接归位 idle）——通知调用方；是否可见提示由调用方
        // 决定。有 result / 有 error / 手动 stop 的非静默路径不进此分支。
        cbsRef.current.onSilentFailExhausted?.();
      }
    };
    recRef.current = rec;
    setInterim(null);
    setStatus("listening");
    try {
      rec.start();
    } catch {
      // start 同步抛（如 InvalidStateError）——复位回 idle + onError("unknown")
      // 显式反馈（调用方映射「语音识别出错，请重试」，消灭同步失败的无反馈形态）
      teardown();
      cbsRef.current.onError?.("unknown");
    }
  }, [ctor, lang, teardown]);

  /** 用户主动停止：stop() 优雅结束（实现会再派最后一段 final result + end），
   *  不立即 teardown 以免丢收尾识别结果。manualStop 标记供 end 判定——主动
   *  停止（尤其还没说话就停）绝不自动重试。 */
  const stop = useCallback(() => {
    const rec = recRef.current;
    if (!rec) {
      return;
    }
    manualStopRef.current = true;
    try {
      rec.stop();
    } catch {
      teardown();
    }
  }, [teardown]);

  /** 点击切换：listening → stop（再点停止）；idle → start。以 ref 判定（比
   *  status state 更即时，error 复位与 end 收尾间的窗口期也安全）。重试延迟
   *  窗口（recRef 已空、status 仍 listening）内点击按停止意图处理：teardown
   *  取消待发重试并归位，防迟到 start 抢跑。新开会的点击重置本轮重试额度。 */
  const toggle = useCallback(() => {
    if (recRef.current) {
      stop();
    } else if (retryTimerRef.current !== null) {
      teardown(); // 清重试定时器 + 归位 idle
    } else {
      retriedRef.current = false;
      start();
    }
  }, [start, stop, teardown]);

  return { status, interim, supported: ctor !== null, toggle, stop };
}
