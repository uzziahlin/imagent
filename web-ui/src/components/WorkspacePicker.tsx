import { useMemo, useState, type KeyboardEvent } from "react";
import { workspaceNameValid, type MachineWorkspaces } from "../lib/protocol";

/** localStorage 前缀：`imagent-lastdir:<machine>` 记录该机器上次选择的目录
 *  （发 setConvWorkdir / 创建会话绑定目录时写入；弹层打开时预选该值）。 */
const LASTDIR_PREFIX = "imagent-lastdir:";

export function loadLastDir(machine: string): string | null {
  return localStorage.getItem(LASTDIR_PREFIX + machine);
}

export function saveLastDir(machine: string, path: string): void {
  localStorage.setItem(LASTDIR_PREFIX + machine, path);
}

/** 路径末段（「正在使用」chip 短标签用；根路径给 "/"）。 */
function baseName(path: string): string {
  const t = path.replace(/\/+$/, "");
  const i = t.lastIndexOf("/");
  return i === -1 ? t : t.slice(i + 1) || "/";
}

/** 绝对路径判定（契约要求绝对路径；本批 Unix 形态）。 */
function isAbsolutePath(p: string): boolean {
  return p.startsWith("/");
}

/** chips 选中态样式（目录/引擎弹窗共用的单一形态）：active 高亮描边、非
 *  active 灰态（模块级纯函数——新会话弹窗的引擎 chips 复用，v1.36）。 */
export const chipCls = (active: boolean): string =>
  "press min-h-10 shrink-0 whitespace-nowrap rounded-full border px-4 py-2 text-sm transition-colors " +
  (active
    ? "border-emerald-500 bg-emerald-600/15 text-ink"
    : "border-line-strong bg-panel text-muted hover:border-emerald-500/60 hover:text-ink");

/**
 * 工作目录选择区（v1.6）：空间 chips（lastUsedAt 降序，无时间者排尾）+
 * 「正在使用」chips（快照 convs 值去重，排除与空间 path 重复项与 default）+
 * 「默认」chip + 「其它路径…」折叠输入框（绝对路径，回车/确认提交）。
 * 空间管理轻量内联：输入路径 + 名称 → onSave（workspace_save）；本批不做
 * 独立管理页。预选（selected）由调用方给定——App 在弹层打开时读 lastdir。
 * 纯展示组件：选择只上抛 onSelect，发包与 lastdir 写入由调用方承担。
 * 移动端：chips 容器横向滚动，触控目标 ≥40px（min-h-10）。
 */
export function WorkspacePicker({
  snapshot,
  selected,
  onSelect,
  onSave,
}: {
  /** 当前机器的工作空间快照（App 保证存在才渲染本组件）。 */
  snapshot: MachineWorkspaces;
  /** 当前选中目录（chip 高亮 / aria-pressed）。 */
  selected?: string;
  /** 选定目录（空间/正在使用/默认/其它路径提交）。 */
  onSelect: (path: string) => void;
  /** 保存当前输入为空间（轻量内联管理入口）。 */
  onSave: (name: string, path: string) => void;
}) {
  const [otherOpen, setOtherOpen] = useState(false);
  const [otherPath, setOtherPath] = useState("");
  const [spaceName, setSpaceName] = useState("");
  const [error, setError] = useState("");

  // 空间 chips：lastUsedAt 降序；无时间（可选字段）排尾（稳定排序保持快照原序）
  const spaces = useMemo(
    () => [...snapshot.items].sort((a, b) => (b.lastUsedAt ?? 0) - (a.lastUsedAt ?? 0)),
    [snapshot.items],
  );

  // 「正在使用」chips：convs 值去重，排除与空间 path 重复项、排除 default
  const inUse = useMemo(() => {
    const spacePaths = new Set(snapshot.items.map((w) => w.path));
    const seen = new Set<string>();
    const out: string[] = [];
    for (const p of Object.values(snapshot.convs)) {
      if (p === snapshot.default || spacePaths.has(p) || seen.has(p)) {
        continue;
      }
      seen.add(p);
      out.push(p);
    }
    return out;
  }, [snapshot]);

  const submitOther = () => {
    const p = otherPath.trim();
    if (!isAbsolutePath(p)) {
      setError("请输入以 / 开头的绝对路径");
      return;
    }
    setError("");
    onSelect(p);
  };

  const saveSpace = () => {
    const p = otherPath.trim();
    const n = spaceName.trim();
    if (!isAbsolutePath(p)) {
      setError("请输入以 / 开头的绝对路径");
      return;
    }
    if (!workspaceNameValid(n)) {
      setError("空间名称须为 1-32 位非空字符");
      return;
    }
    setError("");
    setSpaceName("");
    onSave(n, p);
  };

  // 折叠输入的回车提交（并阻断冒泡——不触发外层表单（新会话弹层）的创建）
  const enterSubmit = (e: KeyboardEvent<HTMLInputElement>, run: () => void) => {
    if (e.key === "Enter") {
      e.preventDefault();
      e.stopPropagation();
      run();
    }
  };

  return (
    <div className="space-y-3">
      <section aria-label="工作空间">
        <div className="mb-1 text-[11px] font-medium uppercase tracking-wider text-ghost">工作空间</div>
        <div className="flex gap-2 overflow-x-auto pb-1">
          {spaces.length === 0 && <span className="py-2 text-xs text-ghost">（尚未保存空间）</span>}
          {spaces.map((w) => (
            <button
              key={w.name}
              type="button"
              title={w.path}
              aria-pressed={selected === w.path}
              onClick={() => onSelect(w.path)}
              className={chipCls(selected === w.path)}
            >
              {w.name}
            </button>
          ))}
        </div>
      </section>

      {inUse.length > 0 && (
        <section aria-label="正在使用">
          <div className="mb-1 text-[11px] font-medium uppercase tracking-wider text-ghost">正在使用</div>
          <div className="flex gap-2 overflow-x-auto pb-1">
            {inUse.map((p) => (
              <button
                key={p}
                type="button"
                title={p}
                aria-pressed={selected === p}
                onClick={() => onSelect(p)}
                className={chipCls(selected === p)}
              >
                {baseName(p)}
              </button>
            ))}
          </div>
        </section>
      )}

      <section aria-label="默认目录">
        <button
          type="button"
          title={snapshot.default}
          aria-pressed={selected === snapshot.default}
          onClick={() => onSelect(snapshot.default)}
          className={chipCls(selected === snapshot.default)}
        >
          默认
        </button>
      </section>

      <section aria-label="其它路径">
        {otherOpen ? (
          <div className="space-y-1.5 rounded-xl border border-line bg-surface p-2.5">
            <div className="flex gap-1.5">
              <input
                value={otherPath}
                onChange={(e) => setOtherPath(e.target.value)}
                onKeyDown={(e) => enterSubmit(e, submitOther)}
                placeholder="/绝对路径"
                aria-label="其它路径输入"
                className="min-w-0 flex-1 rounded-lg border border-line-strong bg-surface px-2.5 py-2 text-base text-ink outline-none placeholder:text-ghost focus:border-emerald-500"
              />
              <button
                type="button"
                onClick={submitOther}
                className="press shrink-0 rounded-lg bg-emerald-600 px-3 py-2 text-xs font-semibold text-white transition-colors hover:bg-emerald-500"
              >
                使用
              </button>
            </div>
            {/* 轻量内联空间管理：输入路径 + 名称 → workspace_save（快照由网关回包刷新） */}
            <div className="flex gap-1.5">
              <input
                value={spaceName}
                onChange={(e) => setSpaceName(e.target.value)}
                onKeyDown={(e) => enterSubmit(e, saveSpace)}
                placeholder="空间名称（1-32 字符）"
                aria-label="空间名称"
                className="min-w-0 flex-1 rounded-lg border border-line-strong bg-surface px-2.5 py-1.5 text-sm text-ink outline-none placeholder:text-ghost focus:border-emerald-500"
              />
              <button
                type="button"
                onClick={saveSpace}
                disabled={!(otherPath.trim() && workspaceNameValid(spaceName))}
                className="press shrink-0 rounded-lg border border-line-strong px-3 py-1.5 text-xs text-muted transition-colors hover:text-ink disabled:opacity-40"
              >
                保存为空间
              </button>
            </div>
            {error && (
              <p role="alert" className="text-xs text-err">
                {error}
              </p>
            )}
          </div>
        ) : (
          <button
            type="button"
            onClick={() => setOtherOpen(true)}
            className="press min-h-10 rounded-full border border-dashed border-line-strong px-4 py-2 text-sm text-muted transition-colors hover:border-emerald-500/60 hover:text-ink"
          >
            + 其它路径…
          </button>
        )}
      </section>
    </div>
  );
}
