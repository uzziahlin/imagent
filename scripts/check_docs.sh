#!/usr/bin/env bash
# ============================================================================
# check_docs.sh —— 文档单一事实源断言（历史债务批 B2 · 任务 3）
#
# 历史上三次文档漂移：ARCHITECTURE 写 schema v9 实为更高、README 写旧版本、
# CLAUDE.md 指向旧 review 轮次。本脚本把「文档声称的当前值」与代码/目录里的
# 事实源对齐，漂移时失败信息直接给出 文件:行号 + 原句。
#
# 断言规则（事实源 → 被检文档）：
#   1. schema 版本：crates/store/src/schema.rs 的 SCHEMA_VERSION
#      ← README.md 与 docs/ARCHITECTURE.md 中的 `schema vN`
#      - 所有出现的 vN 必须 ≤ SCHEMA_VERSION（历史指称允许，如「v12 起」）；
#      - 「当前指称」必须 == SCHEMA_VERSION，取「当前指称」的机读形态为三类：
#        a) `v<n>→v<m>` 迁移区间终点 m（描述完整迁移链，终点=当前）；
#        b) `存储（schema vN`（§5 小节标题形态）；
#        c) 同行含 `schema vN` 且含「线性迁移 / 当前 / curr」。
#        取舍说明：自然语言里「当前」语义难以穷尽，以上三类覆盖本仓文档实际
#        使用过的全部「当前」写法；判不准的一律按「历史指称 ≤」放行，宁漏勿误杀。
#   2. review 轮次：docs/ 下最大编号的 CODE_REVIEW_vN.md（只看 docs/ 根，
#      docs/internal/ 是历史归档）
#      ← CLAUDE.md 同行含 CODE_REVIEW 且含「当前」的 vN；
#      ← README.md 同行含「最新一轮」的 CODE_REVIEW_vN。
#   3. 版本号：Cargo.toml [workspace.package] version
#      ← README.md「当前版本 / 当前 release / 当前稳定版」后的 vX.Y.Z。
#        取舍说明：本仓 README 的既定写法是「以 CHANGELOG.md 最新版本为准」
#        （不落具体数字，天然防漂移）——本规则只在 README 真写出数字时才生效。
#
# 依赖：bash + grep/sed/awk（macOS Bash 3.2 与 GNU 均可，无额外依赖）。
# CI：.github/workflows/ci.yml 的 docs job；本地：bash scripts/check_docs.sh
# ============================================================================
set -euo pipefail
cd "$(dirname "$0")/.."
# 字节语义统一（多字节中文字面量按字面字节匹配，跨 BSD/GNU 行为一致）。
export LC_ALL=C

fail=0
err() {
    printf '✗ %s\n' "$1" >&2
    fail=$((fail + 1))
}

# ---------------------------------------------------------------------------
# 事实源提取
# ---------------------------------------------------------------------------
SCHEMA=$(sed -n 's/^pub const SCHEMA_VERSION: i64 = \([0-9][0-9]*\);.*/\1/p' \
    crates/store/src/schema.rs | head -n 1)
if [ -z "$SCHEMA" ]; then
    err "crates/store/src/schema.rs: 提取不到 SCHEMA_VERSION（pub const SCHEMA_VERSION: i64 = N;）"
fi

VERSION=$(awk '
    /^\[workspace\.package\]$/ { in_ws = 1; next }
    in_ws && /^version[[:space:]]*=/ {
        sub(/^version[[:space:]]*=[[:space:]]*"/, "")
        sub(/".*$/, "")
        print
        exit
    }' Cargo.toml)
if [ -z "$VERSION" ]; then
    err "Cargo.toml: 提取不到 [workspace.package] 的 version"
fi

MAX_REVIEW=0
for p in docs/CODE_REVIEW_v*.md; do
    [ -e "$p" ] || continue
    b=${p##*/}
    n=${b#CODE_REVIEW_v}
    n=${n%.md}
    case $n in
    '' | *[!0-9]*) continue ;;
    esac
    if [ "$n" -gt "$MAX_REVIEW" ]; then MAX_REVIEW=$n; fi
done
if [ "$MAX_REVIEW" -eq 0 ]; then
    err "docs/: 找不到任何 CODE_REVIEW_vN.md（无法确定当前 review 轮次）"
fi

# ---------------------------------------------------------------------------
# 规则 1：schema vN（README.md + docs/ARCHITECTURE.md）
# ---------------------------------------------------------------------------
# 用法：check_line_le <文件> <行号> <原句> —— 句内每个 schema vN 都须 ≤ SCHEMA。
check_line_le() {
    f=$1 no=$2 line=$3
    # grep -o 一次枚举句内全部指称（免手工剥离循环——指称不一定在行尾）。
    # `|| true`：无匹配时 grep 退出 1，pipefail + set -e 会误杀脚本（这是合法的
    # 「本行无 schema 指称」）。
    nums=$(printf '%s' "$line" | grep -oE 'schema v[0-9]+' | sed -E 's/^schema v//' || true)
    for n in $nums; do
        if [ "$n" -gt "$SCHEMA" ]; then
            err "$f:$no 文档指称 schema v$n 超过实际 SCHEMA_VERSION=$SCHEMA：$line"
        fi
    done
}

check_schema_file() {
    f=$1
    # grep -n ''：全行编号（lineno:content），按首个冒号切分（内容可再含冒号）。
    while IFS= read -r numbered; do
        no=${numbered%%:*}
        line=${numbered#*:}

        # 历史指称：≤ 实际版本。
        check_line_le "$f" "$no" "$line"

        # 当前指称 a：v<n>→v<m> 迁移区间终点。
        if printf '%s' "$line" | grep -qE 'v[0-9]+→v[0-9]+'; then
            m=$(printf '%s' "$line" | sed -E 's/.*v[0-9]+→v([0-9]+).*/\1/')
            if [ "$m" != "$SCHEMA" ]; then
                err "$f:$no 迁移区间终点 v$m ≠ 实际 SCHEMA_VERSION=$SCHEMA：$line"
            fi
        fi

        # 当前指称 b：存储（schema vN）小节标题。
        if printf '%s' "$line" | grep -q '存储（schema v'; then
            n=$(printf '%s' "$line" | sed -E 's/.*存储（schema v([0-9]+).*/\1/')
            if [ "$n" != "$SCHEMA" ]; then
                err "$f:$no 存储小节标题 schema v$n ≠ 实际 SCHEMA_VERSION=$SCHEMA：$line"
            fi
        fi

        # 当前指称 c：同行含 schema vN 且含「线性迁移 / 当前 / curr」。
        # 排除区间形态 `schema v<n>→v<m>`——区间起点 v<n> 是历史值（终点已由
        # 规则 a 单独校验），否则会被本规则误判为「当前指称」。
        if printf '%s' "$line" | grep -qE 'schema v[0-9]+' &&
            ! printf '%s' "$line" | grep -qE 'schema v[0-9]+→' &&
            printf '%s' "$line" | grep -qE '线性迁移|当前|curr'; then
            n=$(printf '%s' "$line" | sed -E 's/.*schema v([0-9]+).*/\1/')
            if [ "$n" != "$SCHEMA" ]; then
                err "$f:$no 「当前 schema」指称 v$n ≠ 实际 SCHEMA_VERSION=$SCHEMA：$line"
            fi
        fi
    done < <(grep -n '' "$f")
}

check_schema_file README.md
check_schema_file docs/ARCHITECTURE.md

# ---------------------------------------------------------------------------
# 规则 2：review 轮次（CLAUDE.md「当前」+ README.md「最新一轮」）
# ---------------------------------------------------------------------------
while IFS= read -r numbered; do
    no=${numbered%%:*}
    line=${numbered#*:}
    if printf '%s' "$line" | grep -q 'CODE_REVIEW' &&
        printf '%s' "$line" | grep -q '当前'; then
        n=$(printf '%s' "$line" | sed -E 's/.*[^0-9]v([0-9]+).*/\1/')
        if [ "$n" != "$MAX_REVIEW" ]; then
            err "CLAUDE.md:$no 「当前」review 轮次 v$n ≠ docs/ 下最大编号 CODE_REVIEW_v$MAX_REVIEW.md：$line"
        fi
    fi
done < <(grep -n '' CLAUDE.md)

while IFS= read -r numbered; do
    no=${numbered%%:*}
    line=${numbered#*:}
    if printf '%s' "$line" | grep -q '最新一轮' &&
        printf '%s' "$line" | grep -qE 'CODE_REVIEW_v[0-9]+'; then
        n=$(printf '%s' "$line" | sed -E 's/.*CODE_REVIEW_v([0-9]+).*/\1/')
        if [ "$n" != "$MAX_REVIEW" ]; then
            err "README.md:$no 「最新一轮」指向 CODE_REVIEW_v$n ≠ docs/ 下最大编号 CODE_REVIEW_v$MAX_REVIEW.md：$line"
        fi
    fi
done < <(grep -n '' README.md)

# ---------------------------------------------------------------------------
# 规则 3：README「当前版本 vX.Y.Z」= Cargo.toml [workspace.package] version
# ---------------------------------------------------------------------------
while IFS= read -r numbered; do
    no=${numbered%%:*}
    line=${numbered#*:}
    if printf '%s' "$line" | grep -qE '当前(版本|release|稳定版)' &&
        printf '%s' "$line" | grep -qE 'v[0-9]+\.[0-9]+\.[0-9]+'; then
        v=$(printf '%s' "$line" | sed -E 's/.*v([0-9]+\.[0-9]+\.[0-9]+).*/\1/')
        if [ "$v" != "$VERSION" ]; then
            err "README.md:$no 「当前版本」写 v$v ≠ Cargo.toml workspace version $VERSION：$line"
        fi
    fi
done < <(grep -n '' README.md)

# ---------------------------------------------------------------------------
# 汇总
# ---------------------------------------------------------------------------
if [ "$fail" -gt 0 ]; then
    printf 'docs 单一事实源检查失败：%d 处漂移（修正文档或事实源后重跑）\n' "$fail" >&2
    exit 1
fi
printf '✓ docs 单一事实源检查通过（schema v%s / CODE_REVIEW v%s / workspace %s）\n' \
    "$SCHEMA" "$MAX_REVIEW" "$VERSION"
