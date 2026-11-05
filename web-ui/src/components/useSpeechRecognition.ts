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
 * - unmount 清理：abort + 事件解绑（teardown 幂等，StrictMode 双挂载安全）。
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
 * v1.39 收尾：iOS 主屏（standalone）模式检测——真机实证 Web Speech API 在
 * iOS PWA standalone 下不工作（WebKit 限制：识别服务起不来，start 后静默
 * 失败——无 error、无 result 即 end），Safari 浏览器内可用。命中时由调用方
 * 在 🎤 入口拦截（不发起识别 + 可见提示）。两条件与：
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
}

export function useSpeechRecognition({
  ctor,
  lang = "zh-CN",
  onFinal,
  onError,
}: UseSpeechRecognitionOptions) {
  const [status, setStatus] = useState<SpeechStatus>("idle");
  /** interim 预览文本（null = 无预览；final 到达或结束时清空）。 */
  const [interim, setInterim] = useState<string | null>(null);
  const recRef = useRef<SpeechRecognitionLike | null>(null);
  // 回调经 ref 转发：事件监听器闭包固定引用最新回调（实例生命周期内调用方
  // 传入的函数身份变化不触发重绑，无陈旧闭包）。
  const cbsRef = useRef({ onFinal, onError });
  cbsRef.current = { onFinal, onError };

  /** 收尾：事件解绑 + abort + UI 态复位（幂等；end/error 复位/unmount 共用）。 */
  const teardown = useCallback(() => {
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
    const rec = new ctor();
    rec.lang = lang;
    rec.interimResults = true;
    rec.continuous = false; // 单句：静音自动断（end 自然归位）
    rec.onresult = (ev) => {
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
      const kind = normalizeErrorKind(ev.error);
      if (kind !== "aborted") {
        cbsRef.current.onError?.(kind);
      }
      // UI 态先行复位兜底（规范 error 后必跟 end，teardown 幂等）；实例不
      // 在此 abort/解绑——stop() 优雅路径的收尾 result 事件不能丢
      setStatus("idle");
      setInterim(null);
    };
    // end 静默失败（listening 期间无 result 无 error 即结束，iOS standalone
    // 的 WebKit 特征）不在 hook 层报错：iOS standalone 已由调用方在入口拦
    // 截（isIosStandalone 分支，根本不发起）；其余环境静默结束归位即可——
    // 真实无语音场景走 no-speech error 路径提示，此处不误报。
    rec.onend = () => teardown();
    recRef.current = rec;
    setInterim(null);
    setStatus("listening");
    try {
      rec.start();
    } catch {
      // start 同步抛（如 InvalidStateError）——复位回 idle
      teardown();
    }
  }, [ctor, lang, teardown]);

  /** 用户主动停止：stop() 优雅结束（实现会再派最后一段 final result + end），
   *  不立即 teardown 以免丢收尾识别结果。 */
  const stop = useCallback(() => {
    const rec = recRef.current;
    if (!rec) {
      return;
    }
    try {
      rec.stop();
    } catch {
      teardown();
    }
  }, [teardown]);

  /** 点击切换：listening → stop（再点停止）；idle → start。以 ref 判定（比
   *  status state 更即时，error 复位与 end 收尾间的窗口期也安全）。 */
  const toggle = useCallback(() => {
    if (recRef.current) {
      stop();
    } else {
      start();
    }
  }, [start, stop]);

  return { status, interim, supported: ctor !== null, toggle, stop };
}
