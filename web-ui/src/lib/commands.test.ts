// v1.7 命令表（lib/commands）：表完整性（无重名、字段非空、name 带 / 前缀、
// 覆盖任务规定的全部命令）与过滤函数（名称前缀 / 描述包含）。
// v1.8 模型菜单动态化：modelChips 三层回退 + currentModelText 行尾文本。
import { describe, expect, it } from "vitest";
import type { ModelsInfo } from "./protocol";
import {
  BUILTIN_MODEL_CHIPS,
  COMMANDS,
  currentModelText,
  filterCommands,
  modelChips,
} from "./commands";

/** 任务清单要求的最小命令集（对齐 crates/core/src/dispatch/commands/mod.rs
 *  注册表核对过的命令名）。 */
const REQUIRED = [
  "/stop",
  "/tasks",
  "/resume",
  "/retry",
  "/queue",
  "/status",
  "/export",
  "/cd",
  "/ws",
  "/model",
  "/perm",
  "/timeout",
  "/compact",
  "/cron",
  "/delegate",
  "/help",
];

describe("命令表完整性", () => {
  it("无重名；name 带 / 前缀；desc/group 非空；args 出现即非空", () => {
    const names = COMMANDS.map((c) => c.name);
    expect(new Set(names).size).toBe(names.length);
    for (const c of COMMANDS) {
      expect(c.name.startsWith("/")).toBe(true);
      expect(c.desc.trim().length).toBeGreaterThan(0);
      expect(c.group.trim().length).toBeGreaterThan(0);
      if (c.args !== undefined) {
        expect(c.args.trim().length).toBeGreaterThan(0);
      }
    }
  });

  it("覆盖任务规定的全部命令（注册表核对过的 16 个）", () => {
    const names = COMMANDS.map((c) => c.name);
    for (const name of REQUIRED) {
      expect(names, `缺少命令 ${name}`).toContain(name);
    }
  });

  it("/model 描述附常用模型；内置兜底 chips 与契约一致（default/sonnet/opus/haiku）", () => {
    const model = COMMANDS.find((c) => c.name === "/model");
    expect(model?.desc).toContain("sonnet");
    expect(BUILTIN_MODEL_CHIPS).toEqual(["default", "sonnet", "opus", "haiku"]);
  });
});

describe("modelChips 三层回退（v1.8）", () => {
  const mapped: ModelsInfo = {
    current: "glm-4.6",
    cliDefault: "glm-4.6",
    options: [
      { label: "sonnet", target: "glm-4.6" },
      { label: "haiku", target: "glm-4.5-air" },
    ],
  };
  const official: ModelsInfo = {
    current: null,
    cliDefault: null,
    options: [
      { label: "sonnet", target: null },
      { label: "opus", target: null },
      { label: "haiku", target: null },
    ],
  };

  it("①快照 options 有项：label 前置 default（仅 2 条映射 → 3 chips）", () => {
    expect(modelChips(mapped)).toEqual(["default", "sonnet", "haiku"]);
    expect(modelChips(official)).toEqual(["default", "sonnet", "opus", "haiku"]);
  });

  it("②快照无 models / options 空 → ③内置兜底（与网关缺省同源）", () => {
    expect(modelChips()).toEqual(["default", "sonnet", "opus", "haiku"]);
    expect(modelChips(undefined)).toEqual([...BUILTIN_MODEL_CHIPS]);
    expect(modelChips({ current: null, cliDefault: null, options: [] })).toEqual([
      "default",
      "sonnet",
      "opus",
      "haiku",
    ]);
  });

  it("返回新数组（调用方渲染安全，不泄漏内置常量引用）", () => {
    const a = modelChips();
    expect(a).not.toBe(BUILTIN_MODEL_CHIPS);
    expect(a).toEqual(modelChips());
  });
});

describe("currentModelText 行尾文本（v1.8）", () => {
  it("current 非空显示 current；空且 cliDefault 非空显示「默认 · x」；都空「默认」", () => {
    expect(
      currentModelText({ current: "sonnet", cliDefault: "glm-4.6", options: [] }),
    ).toBe("sonnet");
    expect(
      currentModelText({ current: null, cliDefault: "glm-4.6", options: [] }),
    ).toBe("默认 · glm-4.6");
    expect(currentModelText({ current: null, cliDefault: null, options: [] })).toBe("默认");
    // 空串视为空（网关按可空字符串序列化，防御 null/"" 双形态）。
    expect(currentModelText({ current: "", cliDefault: "", options: [] })).toBe("默认");
  });

  it("models 未下发（旧网关）→「默认」", () => {
    expect(currentModelText()).toBe("默认");
    expect(currentModelText(undefined)).toBe("默认");
  });
});

describe("filterCommands 过滤", () => {
  it("仅 /：全量（= 命令表原序）", () => {
    expect(filterCommands("/")).toEqual([...COMMANDS]);
  });

  it("名称前缀匹配：/mo → /model；/t → /tasks /timeout", () => {
    expect(filterCommands("/mo").map((c) => c.name)).toEqual(["/model"]);
    expect(filterCommands("/t").map((c) => c.name)).toEqual(["/tasks", "/timeout"]);
  });

  it("描述关键词匹配（剥 / 后中文包含）：模型 → /model", () => {
    expect(filterCommands("/模型").map((c) => c.name)).toContain("/model");
  });

  it("无命中返回空数组", () => {
    expect(filterCommands("/zzz")).toEqual([]);
  });
});
