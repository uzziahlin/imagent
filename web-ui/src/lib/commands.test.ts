// v1.7 命令表（lib/commands）：表完整性（无重名、字段非空、name 带 / 前缀、
// 覆盖任务规定的全部命令）与过滤函数（名称前缀 / 描述包含）。
import { describe, expect, it } from "vitest";
import { COMMANDS, MODEL_CHIPS, filterCommands } from "./commands";

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

  it("/model 描述附常用模型；chips 与契约一致（default/sonnet/opus/haiku）", () => {
    const model = COMMANDS.find((c) => c.name === "/model");
    expect(model?.desc).toContain("sonnet");
    expect(MODEL_CHIPS).toEqual(["default", "sonnet", "opus", "haiku"]);
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
