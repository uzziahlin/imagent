// v1.40 语音转写 whisper 主路径的音频编码纯函数（无 DOM 依赖、可单测）：
// 累积的 PCM Float32 采样 → 重采样到 16kHz（线性插值）→ 16-bit PCM WAV
// 编码（44 字节头 + data，纯 JS）→ base64（分块转换防 apply 栈溢出，参照
// e2e.ts bytesToB64 既有先例）。
//
// 为什么纯前端编码（不走 MediaRecorder）：MediaRecorder 产出的容器/编码随平
// 台漂移（iOS 出 mp4/aac），网关侧需要对应解码器；契约固定 16k/mono/16bit
// WAV（PCM 无压缩），whisper 直接吃，网关零解码依赖。iOS 主屏 PWA 下
// getUserMedia/AudioContext 可用（Web Speech 的 SpeechRecognition 才受限），
// 这是本方案的核心卖点。

/** 网关契约目标采样率：16kHz 单声道（whisper 常用输入规格）。 */
export const TARGET_SAMPLE_RATE = 16_000;

/** WAV 头固定长度（44 字节：RIFF 12 + fmt chunk 24 + data 头 8）。 */
export const WAV_HEADER_BYTES = 44;

/**
 * 线性插值重采样（降采样）：输出第 i 点落在源序列 i*ratio 位置，取相邻两
 * 点按小数部分插值。ratio = fromRate / toRate（仅支持 ratio ≥ 1 的降采样
 * 形态——本场景源恒 ≥ 16k：iOS 一般 48k，Android 44.1k/48k；升采样无用途）。
 * 边界：末点 i0+1 越界取最后一点（夹逼，防 undefined）。
 */
export function resampleLinear(data: Float32Array, fromRate: number, toRate: number): Float32Array {
  if (fromRate === toRate || data.length === 0) {
    return data;
  }
  const ratio = fromRate / toRate;
  const outLen = Math.floor(data.length / ratio);
  const out = new Float32Array(outLen);
  for (let i = 0; i < outLen; i++) {
    const pos = i * ratio;
    const i0 = Math.floor(pos);
    const s0 = data[i0];
    const s1 = data[Math.min(i0 + 1, data.length - 1)];
    out[i] = s0 + (s1 - s0) * (pos - i0);
  }
  return out;
}

/**
 * 16-bit PCM WAV 编码：44 字节标准头（RIFF/WAVE/fmt/data，little-endian；
 * PCM 格式码 1、单声道、位深 16）+ 逐样本量化。Float32 [-1,1] → Int16：
 * 负向 ×0x8000、正向 ×0x7fff（非对称映射是波形编解码惯例，满幅不失真），
 * 越界 clamp。sampleRate 调用方传实际值（本场景恒 16k）。
 */
export function encodeWav16(samples: Float32Array, sampleRate: number): Uint8Array {
  const dataLen = samples.length * 2;
  const buf = new ArrayBuffer(WAV_HEADER_BYTES + dataLen);
  const view = new DataView(buf);
  const writeStr = (off: number, s: string) => {
    for (let i = 0; i < s.length; i++) {
      view.setUint8(off + i, s.charCodeAt(i));
    }
  };
  writeStr(0, "RIFF");
  view.setUint32(4, WAV_HEADER_BYTES - 8 + dataLen, true); // RIFF 块长（余下全部）
  writeStr(8, "WAVE");
  writeStr(12, "fmt ");
  view.setUint32(16, 16, true); // fmt chunk 长度（PCM 固定 16）
  view.setUint16(20, 1, true); // 格式码 1 = PCM
  view.setUint16(22, 1, true); // 声道数 1 = mono
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, sampleRate * 2, true); // 字节率 = rate × blockAlign
  view.setUint16(32, 2, true); // 块对齐 = 声道 × 位深/8
  view.setUint16(34, 16, true); // 位深
  writeStr(36, "data");
  view.setUint32(40, dataLen, true);
  let off = WAV_HEADER_BYTES;
  for (let i = 0; i < samples.length; i++) {
    const s = samples[i];
    const clamped = s > 1 ? 1 : s < -1 ? -1 : s;
    view.setInt16(off, (clamped < 0 ? clamped * 0x8000 : clamped * 0x7fff) | 0, true);
    off += 2;
  }
  return new Uint8Array(buf);
}

/**
 * WAV 字节 → base64：分块 String.fromCharCode 避免 apply 栈溢出（60s 录音
 * ≈ 1.92MB，一次 spread 必爆栈；分块先例 = e2e.ts bytesToB64）。
 */
export function wavBytesToB64(bytes: Uint8Array): string {
  let bin = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(bin);
}

/** 分块累积的 Float32 拼接（重采样/编码前合并；total 由调用方按 chunk 累计）。 */
export function concatFloat32(chunks: Float32Array[], total: number): Float32Array {
  const out = new Float32Array(total);
  let off = 0;
  for (const c of chunks) {
    out.set(c, off);
    off += c.length;
  }
  return out;
}
