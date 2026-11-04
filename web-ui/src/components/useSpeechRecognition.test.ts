// v1.39 useSpeechRecognition 状态机单测：构造器注入 stub，直驱
// result/error/end 事件。Composer 集成侧（追加输入框/预览/文案/入口渲染）
// 在 Composer.test.tsx，此处专注 hook 内部行为：增量语义、聚合回调、
// stop vs abort 语义、aborted 静默、清理幂等。
import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
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
function startListening(opts?: { lang?: string; throwOnStart?: boolean }) {
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

  it("start 同步抛错：复位 idle（不悬挂 listening）", () => {
    const { result } = renderHook(() =>
      useSpeechRecognition({
        ctor: class extends StubRecognition {
          start = vi.fn(() => {
            throw new DOMException("InvalidStateError", "InvalidStateError");
          });
        } as unknown as new () => SpeechRecognitionLike,
      }),
    );
    act(() => {
      result.current.toggle();
    });
    expect(result.current.status).toBe("idle");
  });
});
