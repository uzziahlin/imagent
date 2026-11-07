import { useCallback, useEffect, useRef, useState } from "react";
import {
  TARGET_SAMPLE_RATE,
  concatFloat32,
  encodeWav16,
  resampleLinear,
  wavBytesToB64,
} from "../lib/wav";

/**
 * v1.40 语音转写 whisper 主路径的录音 hook（Web Audio API 纯前端编码）：
 * getUserMedia 采麦克风 → AudioContext（iOS 须在用户手势内 resume——start
 * 由 🎤 点击触发天然满足）→ AudioWorklet 采 PCM（主路径；jsdom/旧环境 fallback
 * ScriptProcessor）→ 环形缓冲累积（容量 = maxDurationMs，超限丢最旧）→
 * 停止时重采样到 16k（context 采样率非 16k 时线性插值——iOS 一般 48k）→
 * 16-bit PCM WAV 编码（lib/wav）→ b64 → onRecorded 交调用方发 asr_request。
 * 不走 MediaRecorder：容器/编码随平台漂移（iOS 出 mp4/aac），契约固定
 * WAV/PCM 免网关解码依赖——iOS 主屏 PWA 可用是本方案核心卖点。
 * 设计约定（组件薄、hook 可测，对齐 useSpeechRecognition 先例）：
 * - 状态机 idle / recording / transcribing / error：recording = 采集中；
 *   transcribing = 已停止并完成编码/提交，等待网关 asr_result（归位由调用方
 *   经 reset() 驱动——hook 不感知网关）；error = 失败态（下一次 start 或
 *   reset 清除；UI 文案走 onError 回调由调用方映射）。
 * - 依赖全部注入可测：getUserMedia / AudioContext / AudioWorkletNode 工厂
 *   （缺省从浏览器全局探测；测试传 fake 直连）+ maxDurationMs（60s 契约上限，
 *   测试缩短注入）。
 * - 60s 上限自动截停：到点自动执行停止流程（编码 + onRecorded 照常——前
 *   60s 内容仍提交转写；60s@16k16bit ≈ 1.92MB，恒在网关 8MB 限内）。
 * - 错误归一 onError(kind)：mic-denied = 权限拒绝；no-device = 设备缺失/
 *   被占用；unknown = 其它（含环境无 getUserMedia、编码失败）。
 * - unmount 清理：stream track stop + context close + 节点断链 + 截停定时器
 *   清除；异步流（getUserMedia/addModule 竞态 unmount/stop）经会话号失效，
 *   迟到的资源在续体里自行释放。
 * - 回调经 cbsRef 转发（监听器闭包固定引用最新回调，无陈旧闭包）。
 */

export type VoiceRecorderStatus = "idle" | "recording" | "transcribing" | "error";

export type VoiceRecorderErrorKind = "mic-denied" | "no-device" | "unknown";

/** 编码完成产物：audioB64 = 16k/mono/16bit WAV 的 base64；durationMs = 实际
 *  录音时长（毫秒，四舍五入）。 */
export type RecordedAudio = { audioB64: string; durationMs: number };

export interface UseVoiceRecorderDeps {
  /** getUserMedia 工厂（缺省 navigator.mediaDevices.getUserMedia；缺席 =
   *  环境不支持，start 即 onError("unknown")）。 */
  getUserMedia?: (constraints: MediaStreamConstraints) => Promise<MediaStream>;
  /** AudioContext 工厂（缺省 new AudioContext({sampleRate:16000})——iOS
   *  忽略该选项时建立 48k，由重采样兜底）。 */
  createAudioContext?: () => AudioContext;
  /** AudioWorkletNode 工厂（缺省全局 AudioWorkletNode 构造；addModule 或
   *  构造失败自动 fallback ScriptProcessor）。 */
  createWorkletNode?: (ctx: AudioContext) => AudioWorkletNode;
  /** 录音上限（毫秒；缺省 60_000 = 网关契约，超长自动截停）。 */
  maxDurationMs?: number;
  /** 编码完成回调（stop / 60s 截停各触发一次；空录音——0 采样——不触发）。 */
  onRecorded?: (audio: RecordedAudio) => void;
  /** 错误回调（状态机同时落 error 态；aborted 无此概念——主动 stop 非错误）。 */
  onError?: (kind: VoiceRecorderErrorKind) => void;
}

/** AudioWorklet 处理器名（Blob 内联模块注册名）。 */
export const PCM_CAPTURE_WORKLET_NAME = "pcm-capture";

/** AudioWorklet 采 PCM 模块源（内联 Blob URL——零构建配置侵入，vite 无需
 *  worklet 特殊处理；每输入帧 postMessage 一份拷贝的 Float32 单声道数据，
 *  返回 true 保持处理器存活）。 */
const PCM_CAPTURE_WORKLET_SRC = `
class PcmCaptureWorklet extends AudioWorkletProcessor {
  process(inputs) {
    const ch = inputs[0] && inputs[0][0];
    if (ch && ch.length > 0) {
      this.port.postMessage(ch.slice(0));
    }
    return true;
  }
}
registerProcessor(${JSON.stringify(PCM_CAPTURE_WORKLET_NAME)}, PcmCaptureWorklet);
`;

/** ScriptProcessor 缓冲大小（fallback 路径；4096 ≈ 85ms@48k，粒度足够）。 */
const SCRIPT_PROCESSOR_BUFFER_SIZE = 4096;

/** 缺省 getUserMedia 探测：环境缺席返回 null（jsdom / 极旧浏览器）。 */
function defaultGetUserMedia(): ((c: MediaStreamConstraints) => Promise<MediaStream>) | null {
  return typeof navigator.mediaDevices?.getUserMedia === "function"
    ? (c) => navigator.mediaDevices.getUserMedia(c)
    : null;
}

/** 缺省 AudioContext 工厂：优先 16k 建立（省一次重采样）；个别实现（旧 iOS
 *  Safari）对非硬件采样率抛 NotSupportedError——回落默认采样率建 context，
 *  由停止时的线性插值重采样兜底到 16k（iOS 一般 48k）。 */
function defaultCreateAudioContext(): AudioContext {
  try {
    return new AudioContext({ sampleRate: TARGET_SAMPLE_RATE });
  } catch {
    return new AudioContext();
  }
}

/** getUserMedia 异常 → 归一错误类。NotAllowedError/SecurityError = 权限拒绝；
 *  NotFoundError（无设备）/ NotReadableError（被占用）/ OverconstrainedError
 *  （约束无满足）= 设备侧问题；其余 unknown。 */
function classifyGumError(e: unknown): VoiceRecorderErrorKind {
  const name = (e as { name?: string } | null)?.name ?? "";
  if (name === "NotAllowedError" || name === "SecurityError") {
    return "mic-denied";
  }
  if (name === "NotFoundError" || name === "NotReadableError" || name === "OverconstrainedError") {
    return "no-device";
  }
  return "unknown";
}

/** 录音图句柄（清理共用：断链 + track stop + context close）。 */
interface RecorderGraph {
  stream: MediaStream;
  ctx: AudioContext;
  source: MediaStreamAudioSourceNode;
  node: AudioNode;
}

export function useVoiceRecorder(deps: UseVoiceRecorderDeps = {}) {
  const [status, setStatus] = useState<VoiceRecorderStatus>("idle");
  /** status 的同步镜像：回调/异步续体里的即时判定（state 异步批处理不可靠）。 */
  const statusRef = useRef<VoiceRecorderStatus>("idle");

  const setVoiceStatus = useCallback((s: VoiceRecorderStatus) => {
    statusRef.current = s;
    setStatus(s);
  }, []);

  // 回调与依赖经 ref 转发：start/stop/reset 的 useCallback 闭包恒取最新值
  //（调用方传入的函数身份变化不触发重绑，无陈旧闭包——对齐 useSpeechRecognition）。
  const depsRef = useRef(deps);
  depsRef.current = deps;

  /** 会话号：每次 start 递增；unmount/stop 也递增失效在途异步续体。 */
  const sessionRef = useRef(0);
  /** 当前录音图（null = 无活动录音；清理共用）。 */
  const graphRef = useRef<RecorderGraph | null>(null);
  /** 60s 截停定时器。 */
  const maxTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** PCM 分块累积（环形：总量超容量时从头丢弃最旧块）。 */
  const chunksRef = useRef<Float32Array[]>([]);
  const totalSamplesRef = useRef(0);
  /** 源采样率（start 时从 AudioContext 记录，编码/时长换算用）。 */
  const rateRef = useRef(TARGET_SAMPLE_RATE);

  const clearMaxTimer = useCallback(() => {
    if (maxTimerRef.current !== null) {
      clearTimeout(maxTimerRef.current);
      maxTimerRef.current = null;
    }
  }, []);

  /** 释放录音图：节点断链 + 全部 track stop + context close（异步，吞错——
   *  close 对已关 context 抛错无害）。幂等（graphRef 置空后 no-op）。 */
  const releaseGraph = useCallback((graph: RecorderGraph | null) => {
    if (!graph) {
      return;
    }
    try {
      graph.source.disconnect();
      graph.node.disconnect();
    } catch {
      // 个别实现对已断链节点再 disconnect 抛错——收尾路径吞掉
    }
    for (const track of graph.stream.getTracks()) {
      try {
        track.stop();
      } catch {
        // 同上，收尾路径吞掉
      }
    }
    void graph.ctx.close().catch(() => {
      // 已关闭/挂起中的 context close 失败无害
    });
  }, []);

  /** PCM 块入环形缓冲：容量 = 源采样率 × maxDurationMs（start 时设定），
   *  超限从头丢弃最旧块（60s 截停定时器正常先到，环形仅兜底极端节流）。 */
  const pushChunk = useCallback((chunk: Float32Array) => {
    if (chunk.length === 0) {
      return;
    }
    chunksRef.current.push(chunk);
    totalSamplesRef.current += chunk.length;
    const maxSamples = rateRef.current * ((depsRef.current.maxDurationMs ?? 60_000) / 1000);
    while (
      totalSamplesRef.current > maxSamples &&
      chunksRef.current.length > 1
    ) {
      const dropped = chunksRef.current.shift();
      if (dropped) {
        totalSamplesRef.current -= dropped.length;
      }
    }
  }, []);

  const fail = useCallback(
    (kind: VoiceRecorderErrorKind) => {
      setVoiceStatus("error");
      depsRef.current.onError?.(kind);
    },
    [setVoiceStatus],
  );

  /** 停止流程（手动 stop 与 60s 截停共用）：断图失效异步流 → transcribing →
   *  同步编码（重采样 + WAV + b64）→ onRecorded。空录音（0 采样）静默归位
   *  ——提交空 WAV 只会换回网关 asr_invalid，白耗一次转写。编码失败落
   *  error 态（onError unknown）。 */
  const finishRecording = useCallback(() => {
    clearMaxTimer();
    const graph = graphRef.current;
    graphRef.current = null;
    sessionRef.current += 1; // 失效在途异步续体（迟到资源在续体里自释放）
    const chunks = chunksRef.current;
    const total = totalSamplesRef.current;
    const srcRate = rateRef.current;
    chunksRef.current = [];
    totalSamplesRef.current = 0;
    releaseGraph(graph);
    if (total === 0) {
      setVoiceStatus("idle");
      return;
    }
    setVoiceStatus("transcribing");
    try {
      const raw = concatFloat32(chunks, total);
      const pcm =
        srcRate === TARGET_SAMPLE_RATE ? raw : resampleLinear(raw, srcRate, TARGET_SAMPLE_RATE);
      const audio: RecordedAudio = {
        audioB64: wavBytesToB64(encodeWav16(pcm, TARGET_SAMPLE_RATE)),
        durationMs: Math.round((total / srcRate) * 1000),
      };
      depsRef.current.onRecorded?.(audio);
    } catch {
      setVoiceStatus("error");
      depsRef.current.onError?.("unknown");
    }
  }, [clearMaxTimer, releaseGraph, setVoiceStatus]);

  const start = useCallback(() => {
    if (statusRef.current === "recording" || statusRef.current === "transcribing") {
      return; // recording 重复 start 幂等；transcribing 期间由调用方禁点
    }
    const session = ++sessionRef.current;
    chunksRef.current = [];
    totalSamplesRef.current = 0;
    const gum = depsRef.current.getUserMedia ?? defaultGetUserMedia();
    if (!gum) {
      fail("unknown"); // 环境无 getUserMedia（极旧浏览器）
      return;
    }
    setVoiceStatus("recording"); // 权限弹窗期间即视为录音态（失败回滚 error）
    const run = async () => {
      let opened: RecorderGraph | null = null;
      try {
        const stream = await gum({ audio: true });
        if (sessionRef.current !== session) {
          // 竞态：gum 等待期间 unmount/stop 已失效本会话——迟到资源自释放
          for (const track of stream.getTracks()) {
            track.stop();
          }
          return;
        }
        const createCtx = depsRef.current.createAudioContext ?? defaultCreateAudioContext;
        const ctx = createCtx();
        // iOS 须在用户手势内 resume（start 由 🎤 点击触发天然满足；部分
        // 环境初建即 suspended——弹窗耗时可能失焦，resume 兜底）
        if (ctx.state === "suspended") {
          await ctx.resume();
        }
        if (sessionRef.current !== session) {
          // 竞态：resume 等待期间会话已失效——迟到资源自释放
          for (const track of stream.getTracks()) {
            track.stop();
          }
          void ctx.close().catch(() => {});
          return;
        }
        rateRef.current = ctx.sampleRate;
        const source = ctx.createMediaStreamSource(stream);
        let node: AudioNode;
        let isScriptProcessor = false;
        try {
          // 主路径：AudioWorklet（现代浏览器含 iOS Safari 14.1+）
          const url = URL.createObjectURL(
            new Blob([PCM_CAPTURE_WORKLET_SRC], { type: "application/javascript" }),
          );
          try {
            await ctx.audioWorklet.addModule(url);
            node = (depsRef.current.createWorkletNode ?? ((c: AudioContext) =>
              new AudioWorkletNode(c, PCM_CAPTURE_WORKLET_NAME)))(ctx);
            (node as AudioWorkletNode).port.onmessage = (ev: MessageEvent) => {
              pushChunk(ev.data as Float32Array);
            };
          } finally {
            URL.revokeObjectURL(url); // addModule resolve/reject 后模块已加载/失败，URL 可回收
          }
        } catch {
          // fallback：ScriptProcessor（jsdom / 旧环境；须连 destination 才被
          // 音频线程拉取，输出不写 buffer = 静音不出声）
          const sp = ctx.createScriptProcessor(SCRIPT_PROCESSOR_BUFFER_SIZE, 1, 1);
          sp.onaudioprocess = (ev: AudioProcessingEvent) => {
            pushChunk(ev.inputBuffer.getChannelData(0));
          };
          node = sp;
          isScriptProcessor = true;
        }
        opened = { stream, ctx, source, node };
        graphRef.current = opened;
        source.connect(node);
        if (isScriptProcessor) {
          node.connect(ctx.destination);
        }
        // 60s 上限自动截停（契约）：到点执行停止流程，前 60s 内容照常提交
        maxTimerRef.current = setTimeout(() => {
          maxTimerRef.current = null;
          if (statusRef.current === "recording") {
            finishRecording();
          }
        }, depsRef.current.maxDurationMs ?? 60_000);
      } catch (e) {
        // 失败清理：已建资源就地释放（opened 已入 graphRef 的场景不会到这里
        // ——入 ref 前抛错；releaseGraph 幂等双保险）
        releaseGraph(opened);
        if (sessionRef.current !== session) {
          return; // 会话已失效（unmount 已收尾，不再回调）
        }
        fail(classifyGumError(e));
      }
    };
    void run();
  }, [fail, finishRecording, pushChunk, releaseGraph, setVoiceStatus]);

  /** 用户主动停止（60s 截停自动调用 finishRecording 同路径）。 */
  const stop = useCallback(() => {
    if (statusRef.current !== "recording") {
      return;
    }
    finishRecording();
  }, [finishRecording]);

  /** 调用方驱动归位：transcribing（asr_result/error/超时到达）与 error 态
   *  → idle；recording/idle 不动（reset 无权打断录音）。 */
  const reset = useCallback(() => {
    if (statusRef.current === "transcribing" || statusRef.current === "error") {
      setVoiceStatus("idle");
    }
  }, [setVoiceStatus]);

  // unmount 清理：定时器清除 + 会话号失效 + 录音图释放（track stop +
  // context close + 节点断链）。不回调 onError/onRecorded（卸载后无意义）。
  useEffect(
    () => () => {
      sessionRef.current += 1;
      if (maxTimerRef.current !== null) {
        clearTimeout(maxTimerRef.current);
        maxTimerRef.current = null;
      }
      releaseGraph(graphRef.current);
      graphRef.current = null;
    },
    [releaseGraph],
  );

  return { status, start, stop, reset };
}
