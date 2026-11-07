// v1.39 useSpeechRecognition 状态机单测：构造器注入 stub，直驱
// result/error/end 事件。Composer 集成侧（追加输入框/预览/文案/入口渲染）
// 在 Composer.test.tsx，此处专注 hook 内部行为：增量语义、聚合回调、
// stop vs abort 语义、aborted 静默、清理幂等、静默失败自动重试（v1.39.1，
// fake timers 直驱定时器）、静默重试耗尽兜底回调（v1.39.2）。
import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  isIosStandalone,
  useSpeechRecognition,
  type SpeechRecognitionErrorEventLike,
  type SpeechRecognitionLike,
  type SpeechRecognitionResultEventLike,
} from "./useSpeechRecognition";

afterEach(cleanup);

/** 测试 stub：实现最小类型面，保留实例引用以直驱事件。 */
class StubRecognition implements SpeechRecognitionLike {
  lang = "";
  continuous = false;
  interimResults = false;
  onresult: ((ev: SpeechRecognitionResultEventLike) => void) | null = null;
  onerror: ((ev: SpeechRecognitionErrorEventLike) => void) | null = null;
  onend: (() => void) | null = null;
  start = vi.fn();
  stop = vi.fn();
  abort = vi.fn();
}

/** 构造 results 类数组（resultIndex 起的增量段 + 总长度）。 */
const emitResult = (
  rec: StubRecognition,
  resultIndex: number,
  parts: Array<[string, boolean]>,
) => {
  const results = { length: resultIndex + parts.length } as SpeechRecognitionResultEventLike["results"];
  parts.forEach(([t, isFinal], i) => {
    results[resultIndex + i] = { isFinal, length: 1, 0: { transcript: t } };
  });
  act(() => {
    rec.onresult?.({ resultIndex, results });
  });
};

const emitError = (rec: StubRecognition, code: string) =>
  act(() => {
    rec.onerror?.({ error: code });
  });

const emitEnd = (rec: StubRecognition) =>
  act(() => {
    rec.onend?.();
  });

/** 挂载并启动一轮识别（返回 hook result 与首实例）。 */
function startListening(opts?: {
  lang?: string;
  throwOnStart?: boolean;
  onSilentFailExhausted?: () => void;
}) {
  const onFinal = vi.fn();
  const onError = vi.fn();
  const instances: StubRecognition[] = [];
  class ThrowingStub extends StubRecognition {
    start = vi.fn(() => {
      if (opts?.throwOnStart) {
        throw new DOMException("InvalidStateError", "InvalidStateError");
      }
      instances.push(this as StubRecognition);
    });
  }
  // start 抛错时实例不入引用集——单独经 ref 捕获
  const { result } = renderHook(() =>
    useSpeechRecognition({
      ctor: ThrowingStub as unknown as new () => SpeechRecognitionLike,
      lang: opts?.lang,
      onFinal,
      onError,
      onSilentFailExhausted: opts?.onSilentFailExhausted,
    }),
  );
  act(() => {
    result.current.toggle();
  });
  return { result, onFinal, onError, instances };
}

describe("useSpeechRecognition", () => {
  it("不支持环境（ctor null）：toggle/stop no-op，状态保持 idle", () => {
    const { result } = renderHook(() => useSpeechRecognition({ ctor: null }));
    expect(result.current.supported).toBe(false);
    act(() => {
      result.current.toggle();
      result.current.stop();
    });
    expect(result.current.status).toBe("idle");
  });

  it("start 配置：zh-CN 缺省 + interimResults + 单句模式；listening 态", () => {
    const { result, instances } = startListening();
    expect(result.current.status).toBe("listening");
    expect(result.current.supported).toBe(true);
    expect(instances[0].lang).toBe("zh-CN");
    expect(instances[0].interimResults).toBe(true);
    expect(instances[0].continuous).toBe(false);
    expect(instances[0].start).toHaveBeenCalledTimes(1);
  });

  it("lang 参数透传", () => {
    const { instances } = startListening({ lang: "en-US" });
    expect(instances[0].lang).toBe("en-US");
  });

  it("再点停止走 stop() 优雅收尾（不 abort，保留收尾 final）；end 归位后可再开", () => {
    const { result, instances } = startListening();
    act(() => {
      result.current.toggle(); // listening → stop
    });
    expect(instances[0].stop).toHaveBeenCalledTimes(1);
    expect(instances[0].abort).not.toHaveBeenCalled();
    // stop 后收尾 final 仍可到达（实例未解绑）
    emitResult(instances[0], 0, [["收尾一段", true]]);
    expect(result.current.status).toBe("listening"); // 等 end 才归位
    emitEnd(instances[0]);
    expect(result.current.status).toBe("idle");
    // 归位后可再次开始（新实例）
    act(() => {
      result.current.toggle();
    });
    expect(instances.length).toBe(2);
  });

  it("interim 只进预览态；final 经 onFinal（trim）+ 预览清除", () => {
    const { result, instances, onFinal } = startListening();
    emitResult(instances[0], 0, [["正在说的", false]]);
    expect(result.current.interim).toBe("正在说的");
    emitResult(instances[0], 0, [["完整的话", true]]);
    expect(onFinal).toHaveBeenCalledTimes(1);
    expect(onFinal).toHaveBeenCalledWith("完整的话");
    expect(result.current.interim).toBeNull();
  });

  it("result 增量语义：resultIndex 起遍历，旧段不重复进 onFinal/预览", () => {
    const { result, instances, onFinal } = startListening();
    emitResult(instances[0], 0, [["第一段", true]]);
    expect(onFinal).toHaveBeenCalledTimes(1);
    // 第二次事件从 resultIndex=1 起（新增 interim）
    emitResult(instances[0], 1, [["补充", false]]);
    expect(onFinal).toHaveBeenCalledTimes(1); // 第一段未重复回调
    expect(result.current.interim).toBe("补充");
  });

  it("单次事件多 final 段聚合一次回调；全空白 final 不回调", () => {
    const { instances, onFinal } = startListening();
    emitResult(instances[0], 0, [
      ["你好，", true],
      ["再见", true],
    ]);
    expect(onFinal).toHaveBeenCalledTimes(1);
    expect(onFinal).toHaveBeenCalledWith("你好，再见");
    emitResult(instances[0], 2, [["   ", true]]);
    expect(onFinal).toHaveBeenCalledTimes(1); // trim 后空串不回调
  });

  it("error：归一化 kind 回调 + 状态/预览复位；未知码并入 unknown", () => {
    const { result, instances, onError } = startListening();
    emitResult(instances[0], 0, [["没说完", false]]);
    emitError(instances[0], "not-allowed");
    expect(onError).toHaveBeenCalledWith("not-allowed");
    expect(result.current.status).toBe("idle");
    expect(result.current.interim).toBeNull();

    // 未知码并入 unknown（error 复位仅复位 UI 态，监听器仍挂——end 才解绑）
    emitError(instances[0], "banana-code");
    expect(onError).toHaveBeenLastCalledWith("unknown");

    emitEnd(instances[0]); // 规范 error 后跟 end——teardown 幂等
    expect(result.current.status).toBe("idle");
  });

  it("aborted 静默：onError 不回调（主动终止语义），状态仍复位", () => {
    const { result, instances, onError } = startListening();
    emitError(instances[0], "aborted");
    expect(onError).not.toHaveBeenCalled();
    expect(result.current.status).toBe("idle");
  });

  it("no-speech / network / audio-capture 归一化透传", () => {
    const { instances, onError } = startListening();
    for (const code of ["no-speech", "network", "audio-capture", "service-not-allowed"]) {
      emitError(instances[0], code);
      expect(onError).toHaveBeenLastCalledWith(code);
      // 复位后重开（error 已复位但实例引用仍在——直接再发即可验证透传）
    }
  });

  it("unmount 清理：abort + 三事件解绑", () => {
    const { instances } = startListening();
    const rec = instances[0];
    cleanup();
    expect(rec.abort).toHaveBeenCalledTimes(1);
    expect(rec.onresult).toBeNull();
    expect(rec.onerror).toBeNull();
    expect(rec.onend).toBeNull();
  });

  it("start 同步抛错：teardown 归位 idle + onError(\"unknown\") 反馈一次（v1.39.1 消灭无反馈形态）", () => {
    // startListening 助手内部已完成 toggle（抛错实例不入 instances）
    const { result, onError } = startListening({ throwOnStart: true });
    expect(result.current.status).toBe("idle");
    expect(onError).toHaveBeenCalledTimes(1);
    expect(onError).toHaveBeenCalledWith("unknown");
  });
});

// ---------------------------------------------------------------------------
// v1.39.1 静默失败自动重试（WebKit 确认 bug：iOS Safari 同页后续会话 start
// 静默失败——无 error 无 result 即 end）。fake timers 直驱 end/定时器，验证
// 重试状态机：三条件与（非手动 stop + 无 result + 无 error）+ 本轮额度未用
// → 400ms 后换新实例重试一次；窗口竞态（unmount/停止点击）定时器必须清除。
// ---------------------------------------------------------------------------

describe("useSpeechRecognition 静默失败自动重试（v1.39.1）", () => {
  it("静默 end（无 result 无 error）：期间 status 保持 listening，400ms 后新实例自动重试一次", () => {
    vi.useFakeTimers();
    try {
      const { result, instances, onError } = startListening();
      emitEnd(instances[0]); // WebKit 静默失败特征：无 error 无 result 即 end
      // 重试延迟窗口：呼吸点不断（UI 无感），且不误报错误
      expect(result.current.status).toBe("listening");
      expect(onError).not.toHaveBeenCalled();
      expect(instances.length).toBe(1);
      act(() => {
        vi.advanceTimersByTime(400);
      });
      // 换新实例重试一次（start 全量逻辑：zh-CN 配置齐全）
      expect(instances.length).toBe(2);
      expect(instances[1].start).toHaveBeenCalledTimes(1);
      expect(instances[1].lang).toBe("zh-CN");
      expect(result.current.status).toBe("listening");
    } finally {
      vi.useRealTimers();
    }
  });

  it("重试会话再静默 end：无第二次重试，归位 idle（防循环）", () => {
    vi.useFakeTimers();
    try {
      const { result, instances } = startListening();
      emitEnd(instances[0]);
      act(() => {
        vi.advanceTimersByTime(400);
      });
      expect(instances.length).toBe(2);
      emitEnd(instances[1]); // 重试会话再静默失败
      expect(result.current.status).toBe("idle");
      act(() => {
        vi.advanceTimersByTime(1000);
      });
      expect(instances.length).toBe(2); // 不再重试
      expect(result.current.status).toBe("idle");
    } finally {
      vi.useRealTimers();
    }
  });

  it("手动 stop（未说话）不重试；end 后 manualStop 清除——再开的新会话静默失败仍可重试", () => {
    vi.useFakeTimers();
    try {
      const { result, instances } = startListening();
      act(() => {
        result.current.stop(); // 还没说话就停
      });
      emitEnd(instances[0]);
      expect(result.current.status).toBe("idle");
      act(() => {
        vi.advanceTimersByTime(1000);
      });
      expect(instances.length).toBe(1); // 主动停止绝不自动重启
      // manualStop 已随 end 清除：下一轮用户点击是全新意图，重试额度恢复
      act(() => {
        result.current.toggle();
      });
      expect(instances.length).toBe(2);
      emitEnd(instances[1]);
      act(() => {
        vi.advanceTimersByTime(400);
      });
      expect(instances.length).toBe(3); // 新会话静默失败 → 正常重试
    } finally {
      vi.useRealTimers();
    }
  });

  it("有 result 后 end：正常会话结束，不重试", () => {
    vi.useFakeTimers();
    try {
      const { result, instances, onFinal } = startListening();
      emitResult(instances[0], 0, [["说过话", true]]);
      expect(onFinal).toHaveBeenCalledTimes(1);
      emitEnd(instances[0]); // 单句静音自动断（说过话的 end）
      expect(result.current.status).toBe("idle");
      act(() => {
        vi.advanceTimersByTime(1000);
      });
      expect(instances.length).toBe(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("error 会话 end：不重试（错误自有文案，不叠加重试；aborted 同样命中）", () => {
    vi.useFakeTimers();
    try {
      const { result, instances, onError } = startListening();
      emitError(instances[0], "no-speech");
      expect(onError).toHaveBeenCalledWith("no-speech");
      emitEnd(instances[0]); // 规范 error 后必跟 end
      expect(result.current.status).toBe("idle");
      act(() => {
        vi.advanceTimersByTime(1000);
      });
      expect(instances.length).toBe(1);
      // aborted 同样置 errored（OS 中断等场景不自动重启，静默不回调）
      act(() => {
        result.current.toggle();
      });
      emitError(instances[1], "aborted");
      expect(onError).not.toHaveBeenCalledWith("aborted");
      emitEnd(instances[1]);
      act(() => {
        vi.advanceTimersByTime(400);
      });
      expect(instances.length).toBe(2);
      expect(result.current.status).toBe("idle");
    } finally {
      vi.useRealTimers();
    }
  });

  it("重试延迟窗口内 unmount：定时器清除，无迟到 start", () => {
    vi.useFakeTimers();
    try {
      const { instances } = startListening();
      emitEnd(instances[0]); // 进入重试窗口
      cleanup(); // 窗口内卸载
      act(() => {
        vi.advanceTimersByTime(400);
      });
      expect(instances.length).toBe(1); // 定时器已清，无重试 start
    } finally {
      vi.useRealTimers();
    }
  });

  it("重试延迟窗口内点击（UI 仍录音态）= 停止意图：取消重试归位 idle", () => {
    vi.useFakeTimers();
    try {
      const { result, instances } = startListening();
      emitEnd(instances[0]);
      expect(result.current.status).toBe("listening"); // 窗口内仍是录音态
      act(() => {
        result.current.toggle(); // 用户点停止
      });
      expect(result.current.status).toBe("idle");
      act(() => {
        vi.advanceTimersByTime(400);
      });
      expect(instances.length).toBe(1); // 待发重试已取消
    } finally {
      vi.useRealTimers();
    }
  });
});

// ---------------------------------------------------------------------------
// v1.39.2 静默重试耗尽兜底回调 onSilentFailExhausted：重试会话仍静默 end
// （retried 已置位的 silentFail 路径，直接归位 idle）时恰回调一次——是否
// 可见提示由调用方决定。非静默路径（有 result / 有 error / 手动 stop）不
// 回调，真实无语音/错误场景不误报。
// ---------------------------------------------------------------------------

describe("useSpeechRecognition onSilentFailExhausted 耗尽兜底（v1.39.2）", () => {
  it("重试会话仍静默 end：回调恰一次，状态归位 idle；首会话静默失败（未耗尽）不回调", () => {
    vi.useFakeTimers();
    try {
      const onSilentFailExhausted = vi.fn();
      const { result, instances } = startListening({ onSilentFailExhausted });
      emitEnd(instances[0]); // 首会话静默失败 → 进入重试窗口（未耗尽）
      expect(result.current.status).toBe("listening");
      expect(onSilentFailExhausted).not.toHaveBeenCalled();
      act(() => {
        vi.advanceTimersByTime(400);
      });
      expect(instances.length).toBe(2);
      emitEnd(instances[1]); // 重试会话仍静默失败 → 耗尽
      expect(result.current.status).toBe("idle");
      expect(onSilentFailExhausted).toHaveBeenCalledTimes(1);
      act(() => {
        vi.advanceTimersByTime(1000);
      });
      expect(instances.length).toBe(2); // 不再重试
      expect(onSilentFailExhausted).toHaveBeenCalledTimes(1); // 无后续误触
    } finally {
      vi.useRealTimers();
    }
  });

  it("非静默路径不回调：有 result / 有 error / 手动 stop / 耗尽会话内 error 均不触发", () => {
    vi.useFakeTimers();
    try {
      const onSilentFailExhausted = vi.fn();
      const { result, instances } = startListening({ onSilentFailExhausted });

      // 有 result 的正常 end（单句静音自动断）
      emitResult(instances[0], 0, [["说过话", true]]);
      emitEnd(instances[0]);
      expect(result.current.status).toBe("idle");
      expect(onSilentFailExhausted).not.toHaveBeenCalled();

      // error 后 end（错误自有文案，不叠加耗尽兜底）
      act(() => {
        result.current.toggle();
      });
      emitError(instances[1], "no-speech");
      emitEnd(instances[1]);
      expect(onSilentFailExhausted).not.toHaveBeenCalled();

      // 手动 stop 的 end（未说话即停）
      act(() => {
        result.current.toggle();
      });
      act(() => {
        result.current.stop();
      });
      emitEnd(instances[2]);
      expect(onSilentFailExhausted).not.toHaveBeenCalled();

      // 已进重试（额度用尽）的会话内发生 error：走非静默路径，同样不回调
      act(() => {
        result.current.toggle();
      });
      emitEnd(instances[3]); // 首会话静默失败 → 自动重试
      act(() => {
        vi.advanceTimersByTime(400);
      });
      expect(instances.length).toBe(5);
      emitError(instances[4], "network");
      emitEnd(instances[4]);
      expect(result.current.status).toBe("idle");
      expect(onSilentFailExhausted).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });
});

// ---------------------------------------------------------------------------
// v1.39 收尾引入、v1.39.2 语义修正：isIosStandalone 检测纯函数（不再拦 🎤
// 入口——真机实证主屏模式首次可用；仅作静默重试耗尽后兜底提示的条件）。
// jsdom 无 matchMedia（typeof 为 undefined）——默认即「无标准信号」；标准
// display-mode 路径经 window.matchMedia stub 驱动，Safari 专有路径经
// navigator.standalone 注入；UA 经 navigator 实例 defineProperty 覆盖
// （delete 后还原 prototype getter）。
// ---------------------------------------------------------------------------

describe("isIosStandalone（v1.39 收尾）", () => {
  const IOS_UA =
    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";
  const ANDROID_UA =
    "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Mobile Safari/537.36";
  const MAC_UA =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15";

  const setUA = (ua: string) =>
    Object.defineProperty(window.navigator, "userAgent", { value: ua, configurable: true });

  /** matchMedia stub：仅 standalone 查询可配命中（jsdom 无此 API）。 */
  const stubMatchMedia = (standalone: boolean) => {
    Object.defineProperty(window, "matchMedia", {
      value: (query: string) => ({
        matches: standalone && query.includes("standalone"),
        media: query,
        onchange: null,
        addListener: vi.fn(),
        removeListener: vi.fn(),
        addEventListener: vi.fn(),
        removeEventListener: vi.fn(),
        dispatchEvent: vi.fn(),
      }),
      configurable: true,
      writable: true,
    });
  };

  afterEach(() => {
    delete (window.navigator as { userAgent?: string }).userAgent;
    delete (window.navigator as { standalone?: boolean }).standalone;
    delete (window as { matchMedia?: unknown }).matchMedia;
  });

  it("iOS Safari 浏览器内（两 standalone 信号皆无）：false", () => {
    setUA(IOS_UA);
    expect(isIosStandalone()).toBe(false);
  });

  it("iOS 主屏（navigator.standalone，Safari 专有信号）：true；matchMedia 缺席不炸", () => {
    setUA(IOS_UA);
    Object.defineProperty(window.navigator, "standalone", { value: true, configurable: true });
    // jsdom 无 matchMedia——专有字段路径独立命中（typeof 防御分支有效）
    expect(isIosStandalone()).toBe(true);
  });

  it("iOS 主屏（display-mode: standalone，标准信号）：true", () => {
    setUA(IOS_UA);
    stubMatchMedia(true);
    expect(isIosStandalone()).toBe(true);
  });

  it("Android UA：standalone 双信号全命中也不误报（iOS 判定不满足）", () => {
    setUA(ANDROID_UA);
    Object.defineProperty(window.navigator, "standalone", { value: true, configurable: true });
    stubMatchMedia(true);
    expect(isIosStandalone()).toBe(false);
  });

  it("iPadOS 13+ 桌面 UA（Macintosh）角落情况：不命中（按桌面处理，注释约定）", () => {
    setUA(MAC_UA);
    Object.defineProperty(window.navigator, "standalone", { value: true, configurable: true });
    expect(isIosStandalone()).toBe(false);
  });
});
