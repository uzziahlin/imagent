// v1.40 useVoiceRecorder 状态机单测：依赖全部注入（getUserMedia /
// AudioContext / AudioWorkletNode 工厂 fake 直连），覆盖 worklet 主路径采
// PCM → 停止编码（48k→16k 重采样 + WAV b64）、60s 截停注入、ScriptProcessor
// fallback、权限拒绝/设备缺失归一、空录音静默、unmount 清理与迟到竞态。
import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { WAV_HEADER_BYTES } from "../lib/wav";
import { useVoiceRecorder } from "./useVoiceRecorder";

// jsdom 无 URL.createObjectURL/revokeObjectURL（worklet 模块经 Blob URL 加载
// 的主路径必需）——本文件顶层注入 stub（进程独享，无需还原）。
Object.defineProperty(URL, "createObjectURL", {
  value: () => "blob:fake",
  configurable: true,
  writable: true,
});
Object.defineProperty(URL, "revokeObjectURL", {
  value: () => {},
  configurable: true,
  writable: true,
});

/** fake MediaStream（单 track，stop 可断言）。 */
function makeStream() {
  const track = { stop: vi.fn() };
  return {
    stream: { getTracks: () => [track] } as unknown as MediaStream,
    track,
  };
}

let workletNodes: FakeWorkletNode[] = [];

/** fake AudioWorkletNode：port.onmessage 由 hook 设置，测试经它投 PCM 块。 */
class FakeWorkletNode {
  port = {
    postMessage: vi.fn(),
    onmessage: null as ((ev: { data: Float32Array }) => void) | null,
  };
  connect = vi.fn();
  disconnect = vi.fn();
  constructor(_ctx: unknown, _name: string) {
    workletNodes.push(this);
  }
}

/** fake AudioContext：sampleRate 可配（默认 48k——iOS 真实形态），全部方法
 *  可断言；createScriptProcessor 默认抛错（worklet 主路径不该触达）。 */
class FakeCtx {
  sampleRate: number;
  state = "running";
  destination = { connect: vi.fn(), disconnect: vi.fn() };
  audioWorklet = { addModule: vi.fn(async () => {}) };
  resume = vi.fn(async () => {});
  close = vi.fn(async () => {});
  createMediaStreamSource = vi.fn(() => ({ connect: vi.fn(), disconnect: vi.fn() }));
  createScriptProcessor = vi.fn(() => {
    throw new Error("ScriptProcessor 不应在 worklet 主路径被调用");
  });
  constructor(opts?: { sampleRate?: number }) {
    this.sampleRate = opts?.sampleRate ?? 48000;
  }
}

afterEach(() => {
  workletNodes = [];
});

interface Harness {
  result: { current: ReturnType<typeof useVoiceRecorder> };
  unmount: () => void;
  gum: ReturnType<typeof vi.fn>;
  ctx: FakeCtx;
  onRecorded: ReturnType<typeof vi.fn>;
  onError: ReturnType<typeof vi.fn>;
  track: { stop: ReturnType<typeof vi.fn> };
}

/** 标准挂载：worklet 主路径 + 可注入 sampleRate / maxDurationMs / addModule
 *  行为。start 后须 await act(async () => {}) flush 异步链（gum/addModule
 *  立即 resolve，无真实 timer 参与）。 */
function mount(opts?: {
  sampleRate?: number;
  maxDurationMs?: number;
  addModule?: ReturnType<typeof vi.fn>;
  gum?: ReturnType<typeof vi.fn>;
}): Harness {
  const { stream, track } = makeStream();
  const gum = opts?.gum ?? vi.fn(async () => stream);
  const ctx = new FakeCtx({ sampleRate: opts?.sampleRate });
  if (opts?.addModule) {
    ctx.audioWorklet.addModule = opts.addModule;
  }
  const onRecorded = vi.fn();
  const onError = vi.fn();
  const { result, unmount } = renderHook(() =>
    useVoiceRecorder({
      getUserMedia: gum as unknown as (c: MediaStreamConstraints) => Promise<MediaStream>,
      createAudioContext: () => ctx as unknown as AudioContext,
      createWorkletNode: (c) => new FakeWorkletNode(c, "pcm-capture") as unknown as AudioWorkletNode,
      maxDurationMs: opts?.maxDurationMs,
      onRecorded,
      onError,
    }),
  );
  return { result, unmount, gum, ctx, onRecorded, onError, track };
}

/** flush start() 的异步链（microtask 全 resolve，无需推进 timer）。 */
const flush = () => act(async () => {});

describe("useVoiceRecorder 状态机（v1.40）", () => {
  it("worklet 主路径：idle→recording→投块→stop→transcribing + onRecorded（48k→16k 重采样，durationMs 按源率换算）", async () => {
    const h = mount({ sampleRate: 48000 });
    expect(h.result.current.status).toBe("idle");
    act(() => h.result.current.start());
    expect(h.result.current.status).toBe("recording");
    await flush();
    expect(h.gum).toHaveBeenCalledTimes(1);
    expect(h.ctx.audioWorklet.addModule).toHaveBeenCalledTimes(1);
    // 投 3 块 × 1600 样本（48k 下共 3000ms→100ms）：
    // 4800 样本 @48k = 100ms；重采样后 1600 样本 @16k = 100ms
    const node = workletNodes[0];
    for (const v of [0.1, 0.2, -0.3]) {
      act(() => node.port.onmessage?.({ data: new Float32Array(1600).fill(v) }));
    }
    act(() => h.result.current.stop());
    expect(h.result.current.status).toBe("transcribing");
    expect(h.onRecorded).toHaveBeenCalledTimes(1);
    const audio = h.onRecorded.mock.calls[0][0] as { audioB64: string; durationMs: number };
    expect(audio.durationMs).toBe(100); // total/srcRate：4800/48000
    // WAV 头验证：16k/mono/16bit、data 段 1600×2 字节
    const bin = atob(audio.audioB64);
    expect(bin.length).toBe(WAV_HEADER_BYTES + 3200);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) {
      bytes[i] = bin.charCodeAt(i);
    }
    const v = new DataView(bytes.buffer);
    expect(String.fromCharCode(...bytes.subarray(0, 4))).toBe("RIFF");
    expect(v.getUint32(24, true)).toBe(16000);
    expect(v.getUint16(22, true)).toBe(1);
    expect(v.getUint16(34, true)).toBe(16);
    expect(v.getUint32(40, true)).toBe(3200);
    // 停止清理：track stop + ctx close + 节点断链；worklet 不连 destination
    expect(h.track.stop).toHaveBeenCalledTimes(1);
    expect(h.ctx.close).toHaveBeenCalledTimes(1);
    expect(node.disconnect).toHaveBeenCalledTimes(1);
    expect(node.connect).not.toHaveBeenCalled();
    // reset 归位（调用方在 asr_result 到达后驱动）
    act(() => h.result.current.reset());
    expect(h.result.current.status).toBe("idle");
  });

  it("suspended context 在 start 中 resume（iOS 用户手势内恢复）", async () => {
    const h = mount({ sampleRate: 16000 });
    h.ctx.state = "suspended";
    act(() => h.result.current.start());
    await flush();
    expect(h.ctx.resume).toHaveBeenCalledTimes(1);
    act(() => workletNodes[0].port.onmessage?.({ data: new Float32Array(1600).fill(0.1) }));
    act(() => h.result.current.stop());
    expect(h.onRecorded).toHaveBeenCalledTimes(1);
  });

  it("16k context 直通（无重采样）：1600 采样 → durationMs 100ms、data 1600×2 字节", async () => {
    const h = mount({ sampleRate: 16000 });
    act(() => h.result.current.start());
    await flush();
    act(() => workletNodes[0].port.onmessage?.({ data: new Float32Array(1600).fill(0.5) }));
    act(() => h.result.current.stop());
    const audio = h.onRecorded.mock.calls[0][0] as { audioB64: string; durationMs: number };
    expect(audio.durationMs).toBe(100);
    expect(atob(audio.audioB64).length).toBe(WAV_HEADER_BYTES + 3200);
  });

  it("60s 上限自动截停（maxDurationMs 注入）：到点自动执行停止流程，内容照常提交", async () => {
    vi.useFakeTimers();
    try {
      const h = mount({ sampleRate: 16000, maxDurationMs: 500 });
      act(() => h.result.current.start());
      await flush();
      act(() => workletNodes[0].port.onmessage?.({ data: new Float32Array(1600).fill(0.1) }));
      act(() => {
        vi.advanceTimersByTime(500);
      });
      expect(h.result.current.status).toBe("transcribing");
      expect(h.onRecorded).toHaveBeenCalledTimes(1);
      expect((h.onRecorded.mock.calls[0][0] as { durationMs: number }).durationMs).toBe(100);
    } finally {
      vi.useRealTimers();
    }
  });

  it("addModule 失败 fallback ScriptProcessor：onaudioprocess 采 PCM + connect destination", async () => {
    const h = mount({ sampleRate: 16000, addModule: vi.fn(async () => { throw new Error("no worklet"); }) });
    h.ctx.createScriptProcessor = vi.fn(() => ({
      connect: vi.fn(),
      disconnect: vi.fn(),
      onaudioprocess: null as ((ev: AudioProcessingEvent) => void) | null,
    })) as unknown as FakeCtx["createScriptProcessor"];
    act(() => h.result.current.start());
    await flush();
    expect(h.ctx.createScriptProcessor).toHaveBeenCalledWith(4096, 1, 1);
    const sp = (h.ctx.createScriptProcessor as ReturnType<typeof vi.fn>).mock.results[0]
      .value as { connect: (d: unknown) => void; disconnect: () => void; onaudioprocess: ((ev: AudioProcessingEvent) => void) | null };
    expect(sp.connect).toHaveBeenCalledWith(h.ctx.destination); // 拉取前提
    expect(workletNodes).toHaveLength(0); // worklet 未创建
    act(() =>
      sp.onaudioprocess?.({
        inputBuffer: { getChannelData: () => new Float32Array(1600).fill(0.5) },
      } as unknown as AudioProcessingEvent),
    );
    act(() => h.result.current.stop());
    expect(h.onRecorded).toHaveBeenCalledTimes(1);
    expect((h.onRecorded.mock.calls[0][0] as { durationMs: number }).durationMs).toBe(100);
  });

  it("getUserMedia NotAllowedError → error 态 + onError(mic-denied)；reset 归位后可再 start", async () => {
    const { stream } = makeStream();
    const gum = vi.fn<() => Promise<MediaStream>>()
      .mockRejectedValueOnce(Object.assign(new Error("denied"), { name: "NotAllowedError" }))
      .mockResolvedValue(stream);
    const h = mount({ gum: gum as unknown as Harness["gum"] });
    act(() => h.result.current.start());
    expect(h.result.current.status).toBe("recording"); // 权限弹窗期间即录音态
    await flush();
    expect(h.result.current.status).toBe("error");
    expect(h.onError).toHaveBeenCalledWith("mic-denied");
    act(() => h.result.current.reset());
    expect(h.result.current.status).toBe("idle");
    // 归位后可再开一轮（第二次 gum 成功）
    act(() => h.result.current.start());
    await flush();
    expect(h.result.current.status).toBe("recording");
    act(() => workletNodes[0].port.onmessage?.({ data: new Float32Array(1600).fill(0.1) }));
    act(() => h.result.current.stop());
    expect(h.onRecorded).toHaveBeenCalledTimes(1);
  });

  it("getUserMedia NotFoundError → onError(no-device)；未知错误 → unknown", async () => {
    const gum = vi.fn<() => Promise<MediaStream>>()
      .mockRejectedValueOnce(Object.assign(new Error("none"), { name: "NotFoundError" }))
      .mockRejectedValueOnce(new Error("boom"));
    const h = mount({ gum: gum as unknown as Harness["gum"] });
    act(() => h.result.current.start());
    await flush();
    expect(h.onError).toHaveBeenLastCalledWith("no-device");
    act(() => h.result.current.reset());
    act(() => h.result.current.start());
    await flush();
    expect(h.onError).toHaveBeenLastCalledWith("unknown");
    expect(h.result.current.status).toBe("error");
  });

  it("0 采样静默归位（不提交空 WAV）：stop 即 idle，onRecorded 不触发", async () => {
    const h = mount({ sampleRate: 16000 });
    act(() => h.result.current.start());
    await flush();
    act(() => h.result.current.stop());
    expect(h.result.current.status).toBe("idle");
    expect(h.onRecorded).not.toHaveBeenCalled();
    expect(h.onError).not.toHaveBeenCalled();
  });

  it("recording 中重复 start 幂等（gum 只调一次）；transcribing 期间 start 被忽略", async () => {
    const h = mount({ sampleRate: 16000 });
    act(() => h.result.current.start());
    await flush();
    act(() => h.result.current.start());
    expect(h.gum).toHaveBeenCalledTimes(1);
    act(() => workletNodes[0].port.onmessage?.({ data: new Float32Array(1600).fill(0.1) }));
    act(() => h.result.current.stop()); // 有内容 → transcribing
    act(() => h.result.current.start()); // transcribing：调用方禁点，hook 防御
    expect(h.gum).toHaveBeenCalledTimes(1);
    expect(h.result.current.status).toBe("transcribing");
  });

  it("unmount 清理：track stop + ctx close + 节点断链；截停定时器失效不触发 onRecorded", async () => {
    vi.useFakeTimers();
    try {
      const h = mount({ sampleRate: 16000, maxDurationMs: 200 });
      act(() => h.result.current.start());
      await flush();
      const node = workletNodes[0];
      act(() => workletNodes[0].port.onmessage?.({ data: new Float32Array(1600).fill(0.1) }));
      h.unmount();
      expect(h.track.stop).toHaveBeenCalledTimes(1);
      expect(h.ctx.close).toHaveBeenCalledTimes(1);
      expect(node.disconnect).toHaveBeenCalledTimes(1);
      // 截停定时器已清除：推进过截停点不触发 onRecorded（卸载后回调无意义）
      act(() => {
        vi.advanceTimersByTime(200);
      });
      expect(h.onRecorded).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("unmount 迟到竞态：gum 在卸载后才 resolve，迟到 stream 由续体自释放（track stop）", async () => {
    let resolveGum!: (s: MediaStream) => void;
    const { stream, track } = makeStream();
    const gum = vi.fn(
      () =>
        new Promise<MediaStream>((res) => {
          resolveGum = res;
        }),
    );
    const h = mount({ gum: gum as unknown as Harness["gum"] });
    act(() => h.result.current.start());
    h.unmount(); // gum 还在挂起
    resolveGum(stream); // 迟到 resolve
    await flush();
    expect(track.stop).toHaveBeenCalledTimes(1); // 竞态自释放
    expect(h.onRecorded).not.toHaveBeenCalled();
    expect(h.onError).not.toHaveBeenCalled();
  });
});
