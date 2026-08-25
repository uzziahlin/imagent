#!/usr/bin/env bash
# imagent 一键安装 + 首次配置 + MCP 挂载（macOS / Linux）。
#
# 用法：
#   bash <(curl -fsSL https://raw.githubusercontent.com/uzziahlin/imagent/main/install.sh)
#   bash install.sh [选项]
#
# 选项：
#   --version <tag>   安装指定 release（默认 latest）
#   --bin <dir>       安装目录（默认 /usr/local/bin，不可写回退 ~/.local/bin）
#   --workdir <path>  agent 工作目录（默认 当前目录；交互模式会问）
#   --app-id <cli_x>  飞书 App ID
#   --secret <s>      飞书 App Secret（写 shell rc，不写 config）
#   --mcp-only        跳过安装与配置，只做 MCP 挂载（二进制须已在 PATH）
#   --yes             全部用默认值/已给参数，不交互提问
#
# 环境变量：IMAGENT_VERSION / INSTALL_DIR 与 --version / --bin 等价；
#   IMAGENT_NO_BUILD=1 禁用「release 缺 mcp-ask 时源码构建」兜底。
#
# 行为要点：
#   - 二进制 + .sha256 一起下载，校验通过才安装（防供应链篡改）；
#   - 最新 release 若尚未包含 mcp-ask 子命令（ask_via_im 需 v1.3.0+），且本机
#     有 cargo/git，自动 clone 源码构建兜底；
#   - ~/.imagent/config.toml 已存在则保持不动（绝不覆盖）；secret 经 grep
#     防重复地写入 shell rc。
#   - MCP：有 claude CLI 直接 `claude mcp add`（user 级）；否则打印 mcpServers
#     JSON 供 ZCode / Cursor 等手动粘贴。
#
# 注意：所有 shell 变量一律 ${var} 显式包裹，避免紧跟多字节字符时被误解析。

set -euo pipefail

OWNER=uzziahlin
REPO=imagent
VERSION="${IMAGENT_VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
IMAGENT_HOME_DIR="${IMAGENT_HOME:-${HOME}/.imagent}"
INTERACTIVE=1
MCP_ONLY=0
OPT_WORKDIR="" OPT_APP_ID="" OPT_SECRET=""
NO_BUILD="${IMAGENT_NO_BUILD:-0}"

say()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m! %s\033[0m\n' "$*"; }
die()  { printf '\033[1;31mX %s\033[0m\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --bin) INSTALL_DIR="$2"; shift 2 ;;
    --workdir) OPT_WORKDIR="$2"; shift 2 ;;
    --app-id) OPT_APP_ID="$2"; shift 2 ;;
    --secret) OPT_SECRET="$2"; shift 2 ;;
    --mcp-only) MCP_ONLY=1; shift ;;
    --yes) INTERACTIVE=0; shift ;;
    --help|-h) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown arg: ${1} (--help for usage)" ;;
  esac
done

# ask <提示> <默认值>：交互时提问（可回车取默认），非交互/管道直接回默认值。
ask() {
  local prompt="$1" default="$2" ans=""
  if [ "${INTERACTIVE}" -ne 1 ] || [ ! -t 0 ]; then echo "${default}"; return; fi
  printf '\033[1;36m%s\033[0m [%s]: ' "${prompt}" "${default}" >&2
  read -r ans || true
  echo "${ans:-${default}}"
}

# --- 平台检测 → release asset 名（须与 .github/workflows/release.yml matrix 一致）---
OS=$(uname -s); ARCH=$(uname -m)
case "$OS" in Darwin) os=darwin ;; Linux) os=linux ;; *) die "unsupported OS: ${OS}" ;; esac
case "$ARCH" in arm64|aarch64) arch=arm64 ;; x86_64|amd64) arch=x86_64 ;; *) die "unsupported arch: ${ARCH}" ;; esac
case "${os}-${arch}" in
  darwin-arm64)  asset=imagent-darwin-arm64 ;;
  darwin-x86_64) asset=imagent-darwin-x86_64 ;;
  linux-x86_64)  asset=imagent-linux-x86_64 ;;
  *) die "no prebuilt binary for ${os}-${arch}; build from source: cargo build --release" ;;
esac

need() { command -v "$1" >/dev/null 2>&1 || die "missing dependency: ${1}" ; }
need curl; need uname

tmpdir=$(mktemp -d 2>/dev/null || mktemp -d -t imagent)
trap 'rm -rf "${tmpdir}"' EXIT INT TERM

# ===========================================================================
# 1. 安装二进制
# ===========================================================================
BIN=""
if [ "${MCP_ONLY}" -eq 1 ]; then
  BIN=$(command -v imagent 2>/dev/null || true)
  [ -n "${BIN}" ] || die "--mcp-only 但 PATH 里没有 imagent"
  say "跳过安装，使用 ${BIN}"
else
  if [ "${VERSION}" = "latest" ]; then
    base="https://github.com/${OWNER}/${REPO}/releases/latest/download"
  else
    base="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}"
  fi
  if [ ! -d "${INSTALL_DIR}" ]; then
    mkdir -p "${INSTALL_DIR}" 2>/dev/null || \
      die "cannot create ${INSTALL_DIR}; pass --bin <writable-dir> (e.g. \$HOME/.local/bin)"
  fi
  BIN="${INSTALL_DIR}/imagent"
  had_old=0; [ -x "${BIN}" ] && had_old=1

  say "下载 ${asset}（${VERSION}）..."
  need_build=0
  if curl -fsSL "${base}/${asset}" -o "${tmpdir}/${asset}"; then
    say "校验 sha256 ..."
    if curl -fsSL "${base}/${asset}.sha256" -o "${tmpdir}/${asset}.sha256"; then
      if command -v sha256sum >/dev/null 2>&1; then
        (cd "${tmpdir}" && sha256sum -c "${asset}.sha256") || die "sha256 校验失败（疑似篡改/损坏），中止"
      elif command -v shasum >/dev/null 2>&1; then
        (cd "${tmpdir}" && shasum -a 256 -c "${asset}.sha256") || die "sha256 校验失败（疑似篡改/损坏），中止"
      else
        warn "无 sha256sum/shasum，跳过完整性校验"
      fi
    else
      warn ".sha256 不可得，跳过校验"
    fi
    chmod +x "${tmpdir}/${asset}"
    if [ -w "${INSTALL_DIR}" ]; then
      mv "${tmpdir}/${asset}" "${BIN}"
    else
      echo "-> ${INSTALL_DIR} needs elevated rights, using sudo ..."
      sudo mv "${tmpdir}/${asset}" "${BIN}"
    fi
    if [ "${had_old}" -eq 1 ]; then
      say "已升级 ${BIN}（覆盖旧版本；config 与凭据不受影响）"
      warn "在跑的进程仍用旧二进制，需重启生效：前台 Ctrl-C 重跑；后台服务 imagent service install（重装即重启）"
    else
      say "已安装 ${BIN}"
    fi
    # release 早于 ask_via_im 合入（缺 mcp-ask）→ 源码构建兜底。
    if ! "${BIN}" mcp-ask --print-config >/dev/null 2>&1; then
      warn "该 release 不含 mcp-ask 子命令（ask_via_im 需 v1.3.0+）"
      need_build=1
    fi
  else
    warn "release 下载失败"
    need_build=1
  fi

  if [ "${need_build}" -eq 1 ]; then
    if [ "${NO_BUILD}" = "1" ]; then
      warn "IMAGENT_NO_BUILD=1，跳过源码构建（mcp-ask 不可用，其余功能不受影响）"
    else
      command -v cargo >/dev/null 2>&1 || die "源码构建需要 Rust（https://rustup.rs 装后重跑）"
      command -v git  >/dev/null 2>&1 || die "源码构建需要 git"
      say "clone + cargo build --release（数分钟）..."
      git clone --depth 1 "https://github.com/${OWNER}/${REPO}.git" "${tmpdir}/src"
      (cd "${tmpdir}/src" && cargo build --release)
      if [ -w "${INSTALL_DIR}" ]; then
        install -m 755 "${tmpdir}/src/target/release/imagent" "${BIN}"
      else
        sudo install -m 755 "${tmpdir}/src/target/release/imagent" "${BIN}"
      fi
      say "已从源码构建并安装 ${BIN}"
    fi
  fi
fi

# ===========================================================================
# 2. 首次配置（~/.imagent/config.toml 已存在则跳过，绝不覆盖）
# ===========================================================================
if [ "${MCP_ONLY}" -eq 0 ] && [ ! -f "${IMAGENT_HOME_DIR}/config.toml" ]; then
  say "生成 ${IMAGENT_HOME_DIR}/config.toml"
  WORKDIR="${OPT_WORKDIR:-$(ask "agent 工作目录（绝对路径）" "${PWD}")}"
  APP_ID="${OPT_APP_ID:-$(ask "飞书 App ID（回车跳过，之后手填）" "")}"
  SECRET="${OPT_SECRET:-$(ask "飞书 App Secret（回车跳过；不会写进 config）" "")}"
  mkdir -p "${IMAGENT_HOME_DIR}"; chmod 700 "${IMAGENT_HOME_DIR}"
  {
    echo "# imagent 配置（install.sh 生成；完整说明见项目 README）"
    echo "default_workdir = \"${WORKDIR}\""
    echo "allowed_senders = []        # 留空 = 发现模式：给 bot 发条消息，日志拿你的 id 后 imagent allow <id>"
    echo "allowed_tools = [\"Read\", \"Edit\"]"
    echo "agent = \"claude-cli\""
    echo "platform = \"feishu\""
    echo "permission_mode = \"ask\""
    echo "# ask_via_im_conv = \"feishu:ou_xxx\"   # 终端 agent 的 ask_via_im 提问投递会话（/whoami 可查）"
    if [ -n "${APP_ID}" ]; then
      echo "feishu_app_id = \"${APP_ID}\""
    else
      echo "# feishu_app_id = \"cli_xxx\"   # 飞书后台「凭证与基础信息」"
    fi
  } > "${IMAGENT_HOME_DIR}/config.toml"
  chmod 600 "${IMAGENT_HOME_DIR}/config.toml"
  if [ -n "${SECRET}" ]; then
    rc_file="${HOME}/.zshrc"
    case "$(basename "${SHELL:-bash}")" in bash) rc_file="${HOME}/.bashrc" ;; esac
    if ! grep -q "IMAGENT_FEISHU_APP_SECRET" "${rc_file}" 2>/dev/null; then
      printf 'export IMAGENT_FEISHU_APP_SECRET="%s"  # imagent\n' "${SECRET}" >> "${rc_file}"
      say "secret 已写入 ${rc_file}（新开终端或 source 后生效）"
    fi
  fi
elif [ "${MCP_ONLY}" -eq 0 ]; then
  say "已有 config（${IMAGENT_HOME_DIR}/config.toml），保持不动"
fi

# ===========================================================================
# 3. MCP 挂载（ask_via_im）
# ===========================================================================
MCP_JSON=$("${BIN}" mcp-ask --print-config 2>/dev/null || true)
if [ -z "${MCP_JSON}" ]; then
  warn "当前二进制不支持 mcp-ask（旧版本）——升级后运行：imagent mcp-ask --print-config"
elif command -v claude >/dev/null 2>&1; then
  claude mcp remove imagent -s user >/dev/null 2>&1 || true
  claude mcp add -s user imagent -- "${BIN}" mcp-ask >/dev/null
  say "已挂载到 Claude Code（user 级，claude mcp list 可查）"
else
  say "未检测到 claude CLI。把下面的 JSON 并入你的 MCP client（ZCode / Cursor 等）："
  echo "    ${MCP_JSON}"
fi

echo
say "完成。后续步骤："
echo "  1. 检查 config（${IMAGENT_HOME_DIR}/config.toml）：feishu_app_id / secret / 工作目录"
echo "  2. 启动：${BIN} start（缺省读 config 的 platform；imagent service install 可装后台服务）"
echo "  3. 给 bot 发条消息拿你的 sender id，授权：${BIN} allow <ou_xxx>"
echo "  4. config 里设 ask_via_im_conv = \"feishu:ou_xxx\" 启用终端 agent 提问转发"
