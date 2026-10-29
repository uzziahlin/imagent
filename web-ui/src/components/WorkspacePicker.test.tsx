// v1.6 工作目录选择区（WorkspacePicker）：chips 渲染与排序 / 「正在使用」
// 去重排除 / 默认 chip / 其它路径提交（绝对路径校验）/ 保存为空间 / 预选
// lastdir（localStorage 助手）。
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { WorkspacePicker, loadLastDir, saveLastDir } from "./WorkspacePicker";
import type { MachineWorkspaces } from "../lib/protocol";

afterEach(() => {
  cleanup();
  localStorage.clear();
});

const SNAPSHOT: MachineWorkspaces = {
  default: "/home/u/def",
  items: [
    { name: "code", path: "/home/u/code", lastUsedAt: 100 },
    { name: "docs", path: "/home/u/docs", lastUsedAt: 200 },
    { name: "misc", path: "/home/u/misc" }, // 无 lastUsedAt → 排尾
  ],
  convs: {
    "web:default": "/home/u/code", // 与空间 path 重复 → 排除
    "web:a": "/home/u/tmp1",
    "web:b": "/home/u/tmp1", // 重复值 → 去重
    "web:c": "/home/u/def", // default → 排除
    "local:work": "/home/u/tmp2", // local: 会话目录也是「正在使用」来源
  },
};

function mountPicker(opts?: { selected?: string }) {
  const onSelect = vi.fn();
  const onSave = vi.fn();
  render(
    <WorkspacePicker snapshot={SNAPSHOT} selected={opts?.selected} onSelect={onSelect} onSave={onSave} />,
  );
  return { onSelect, onSave };
}

describe("WorkspacePicker chips（v1.6）", () => {
  it("空间 chips 按 lastUsedAt 降序（无时间者排尾）；「正在使用」去重并排除空间/default；默认 chip 在", () => {
    mountPicker();
    const spaces = within(screen.getByLabelText("工作空间")).getAllByRole("button");
    expect(spaces.map((b) => b.textContent)).toEqual(["docs", "code", "misc"]);
    expect(spaces[0].getAttribute("title")).toBe("/home/u/docs");

    const inUse = within(screen.getByLabelText("正在使用")).getAllByRole("button");
    // tmp1 去重后一次 + tmp2（local: 会话）；code/def 被排除
    expect(inUse.map((b) => b.textContent).sort()).toEqual(["tmp1", "tmp2"]);
    expect(inUse[0].getAttribute("title")).toMatch(/^\/home\/u\/tmp/);

    const def = within(screen.getByLabelText("默认目录")).getByRole("button", { name: "默认" });
    expect(def.getAttribute("title")).toBe("/home/u/def");
  });

  it("选择回调：空间 / 正在使用 / 默认 chip 点击各回传对应绝对路径", () => {
    const { onSelect } = mountPicker();
    fireEvent.click(within(screen.getByLabelText("工作空间")).getByRole("button", { name: "code" }));
    fireEvent.click(within(screen.getByLabelText("正在使用")).getByRole("button", { name: "tmp1" }));
    fireEvent.click(within(screen.getByLabelText("默认目录")).getByRole("button", { name: "默认" }));
    expect(onSelect.mock.calls).toEqual([
      ["/home/u/code"],
      ["/home/u/tmp1"],
      ["/home/u/def"],
    ]);
  });

  it("预选：selected 命中的 chip 高亮（aria-pressed）", () => {
    mountPicker({ selected: "/home/u/docs" });
    const spaces = within(screen.getByLabelText("工作空间")).getAllByRole("button");
    expect(spaces[0].getAttribute("aria-pressed")).toBe("true"); // docs
    expect(spaces[1].getAttribute("aria-pressed")).toBe("false"); // code
    expect(
      within(screen.getByLabelText("默认目录")).getByRole("button", { name: "默认" }).getAttribute("aria-pressed"),
    ).toBe("false");
  });
});

describe("WorkspacePicker 其它路径与保存空间（v1.6）", () => {
  function openOther() {
    fireEvent.click(screen.getByRole("button", { name: "+ 其它路径…" }));
    return screen.getByLabelText("其它路径输入");
  }

  it("其它路径：相对路径拒绝（行内提示、无回调）；绝对路径提交（点击/回车均触发）", () => {
    const { onSelect } = mountPicker();
    const input = openOther();
    fireEvent.change(input, { target: { value: "relative/path" } });
    fireEvent.click(screen.getByRole("button", { name: "使用" }));
    expect(onSelect).not.toHaveBeenCalled();
    expect(screen.getByRole("alert").textContent).toContain("绝对路径");

    fireEvent.change(input, { target: { value: " /home/u/new " } });
    fireEvent.click(screen.getByRole("button", { name: "使用" }));
    expect(onSelect).toHaveBeenCalledWith("/home/u/new"); // trim 后提交

    // 回车提交同样生效
    fireEvent.change(input, { target: { value: "/home/u/enter" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSelect).toHaveBeenCalledWith("/home/u/enter");
  });

  it("保存为空间：路径 + 名称（trim）→ onSave；名称非法时按钮禁用", () => {
    const { onSave } = mountPicker();
    const input = openOther();
    fireEvent.change(input, { target: { value: "/home/u/new" } });

    const name = screen.getByLabelText("空间名称");
    const saveBtn = screen.getByRole("button", { name: "保存为空间" });
    expect((saveBtn as HTMLButtonElement).disabled).toBe(true); // 无名称
    fireEvent.change(name, { target: { value: "  新空间  " } });
    expect((saveBtn as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(saveBtn);
    expect(onSave).toHaveBeenCalledWith("新空间", "/home/u/new");

    // 超长名称（33 字符）禁用
    fireEvent.change(name, { target: { value: "x".repeat(33) } });
    expect((screen.getByRole("button", { name: "保存为空间" }) as HTMLButtonElement).disabled).toBe(true);
  });
});

describe("lastdir localStorage 助手（v1.6）", () => {
  it("saveLastDir / loadLastDir 按 machine 分键往返", () => {
    expect(loadLastDir("home-mac")).toBeNull();
    saveLastDir("home-mac", "/home/u/code");
    saveLastDir("work-mac", "/work/u/docs");
    expect(loadLastDir("home-mac")).toBe("/home/u/code");
    expect(loadLastDir("work-mac")).toBe("/work/u/docs");
  });
});
