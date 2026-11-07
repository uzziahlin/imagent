// v1.40 语音转写音频编码纯函数单测：WAV 头字节逐字段（对齐网关 16k/mono/
// 16bit PCM 契约）、量化边界、线性插值重采样点数与插值值、b64 分块（超
// 0x8000 大数组不爆栈且编解码往返一致）。
import { describe, expect, it } from "vitest";
import {
  TARGET_SAMPLE_RATE,
  WAV_HEADER_BYTES,
  concatFloat32,
  encodeWav16,
  resampleLinear,
  wavBytesToB64,
} from "./wav";

/** DataView 视角读回 WAV 结构（小端）。 */
function wavView(wav: Uint8Array): DataView {
  return new DataView(wav.buffer, wav.byteOffset, wav.byteLength);
}
const magic = (wav: Uint8Array, off: number, len: number) =>
  String.fromCharCode(...wav.subarray(off, off + len));

describe("encodeWav16", () => {
  it("44 字节标准头逐字段：RIFF/WAVE/fmt /data 魔数 + PCM/mono/16k/字节率/块对齐/位深", () => {
    const wav = encodeWav16(new Float32Array(100), TARGET_SAMPLE_RATE);
    expect(wav.length).toBe(WAV_HEADER_BYTES + 200);
    expect(magic(wav, 0, 4)).toBe("RIFF");
    expect(magic(wav, 8, 4)).toBe("WAVE");
    expect(magic(wav, 12, 4)).toBe("fmt ");
    expect(magic(wav, 36, 4)).toBe("data");
    const v = wavView(wav);
    expect(v.getUint32(4, true)).toBe(WAV_HEADER_BYTES - 8 + 200); // RIFF 块长
    expect(v.getUint32(16, true)).toBe(16); // fmt chunk 长度（PCM 固定）
    expect(v.getUint16(20, true)).toBe(1); // PCM
    expect(v.getUint16(22, true)).toBe(1); // mono（网关契约）
    expect(v.getUint32(24, true)).toBe(16000);
    expect(v.getUint32(28, true)).toBe(32000); // 字节率 = rate × blockAlign
    expect(v.getUint16(32, true)).toBe(2); // 块对齐
    expect(v.getUint16(34, true)).toBe(16); // 位深
    expect(v.getUint32(40, true)).toBe(200); // data 块长
  });

  it("量化：满幅 ±1 → 32767/-32768（非对称映射）、零 → 0、越界 clamp", () => {
    const wav = encodeWav16(new Float32Array([1, -1, 0, 0.5, -0.5, 2, -2]), 16000);
    const v = wavView(wav);
    expect(v.getInt16(WAV_HEADER_BYTES + 0, true)).toBe(32767);
    expect(v.getInt16(WAV_HEADER_BYTES + 2, true)).toBe(-32768);
    expect(v.getInt16(WAV_HEADER_BYTES + 4, true)).toBe(0);
    expect(v.getInt16(WAV_HEADER_BYTES + 6, true)).toBe(0.5 * 0x7fff | 0); // 16383
    expect(v.getInt16(WAV_HEADER_BYTES + 8, true)).toBe(-0.5 * 0x8000); // -16384
    expect(v.getInt16(WAV_HEADER_BYTES + 10, true)).toBe(32767); // clamp 上
    expect(v.getInt16(WAV_HEADER_BYTES + 12, true)).toBe(-32768); // clamp 下
  });

  it("空采样：仅 44 字节头（data 长度 0）", () => {
    const wav = encodeWav16(new Float32Array(0), 16000);
    expect(wav.length).toBe(WAV_HEADER_BYTES);
    expect(wavView(wav).getUint32(40, true)).toBe(0);
  });
});

describe("resampleLinear", () => {
  it("同采样率原引用返回（零拷贝直通）；空数据直通", () => {
    const data = new Float32Array([1, 2, 3]);
    expect(resampleLinear(data, 16000, 16000)).toBe(data);
    const empty = new Float32Array(0);
    expect(resampleLinear(empty, 48000, 16000)).toBe(empty);
  });

  it("48k→16k（3:1）整数比：输出点数 floor(n/3)；等差数据线性插值精确复现源值", () => {
    // 等差序列 data[i]=i：第 i 点落在 pos=3i（整数，frac=0）→ 精确取源值 3i
    const data = new Float32Array(9);
    for (let i = 0; i < data.length; i++) {
      data[i] = i;
    }
    const out = resampleLinear(data, 48000, 16000);
    expect(out.length).toBe(3);
    expect(Array.from(out)).toEqual([0, 3, 6]);
  });

  it("非整数比（44.1k→16k）线性插值：落点小数部分参与加权", () => {
    const data = new Float32Array(11);
    for (let i = 0; i < data.length; i++) {
      data[i] = i; // 等差 1
    }
    const out = resampleLinear(data, 44100, 16000); // ratio = 2.75625
    expect(out.length).toBe(Math.floor(11 / (44100 / 16000)));
    // i=1 → pos=2.75625：s0=2、s1=3、frac=0.75625 → 2.75625
    expect(out[1]).toBeCloseTo(2 + (3 - 2) * (2.75625 - 2), 5);
  });

  it("降采样输出点数 floor(n/ratio) 截断；ratio>1 时末点窗口数学上不越界（夹逼仅作防御）", () => {
    const out = resampleLinear(new Float32Array([1, 2, 3, 4, 5, 6, 7]), 48000, 16000);
    expect(out.length).toBe(2); // floor(7/3)
    expect(Array.from(out)).toEqual([1, 4]); // pos=0/3 处精确取源值
  });
});

describe("wavBytesToB64", () => {
  it("小数据与 btoa 基准一致", () => {
    const bytes = new Uint8Array([1, 2, 3, 0xfb, 0x00]);
    let bin = "";
    for (const b of bytes) {
      bin += String.fromCharCode(b);
    }
    expect(wavBytesToB64(bytes)).toBe(btoa(bin));
  });

  it("大数组（>0x8000 分块阈值）不爆栈：长度正确且 atob 往返逐字节一致", () => {
    const bytes = new Uint8Array(200_000);
    for (let i = 0; i < bytes.length; i++) {
      bytes[i] = (i * 7 + 13) & 0xff;
    }
    const b64 = wavBytesToB64(bytes);
    expect(b64.length).toBe(Math.ceil(bytes.length / 3) * 4);
    const back = atob(b64);
    expect(back.length).toBe(bytes.length);
    for (let i = 0; i < bytes.length; i++) {
      if (back.charCodeAt(i) !== bytes[i]) {
        throw new Error(`b64 往返不一致 @${i}`);
      }
    }
  });
});

describe("concatFloat32", () => {
  it("分块按序拼接（total 由调用方累计）", () => {
    expect(Array.from(concatFloat32([new Float32Array([1, 2]), new Float32Array([3])], 3))).toEqual([1, 2, 3]);
    expect(Array.from(concatFloat32([], 0))).toEqual([]);
  });
});
