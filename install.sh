#!/bin/sh
# imagent 安装脚本 —— 从 GitHub Releases 下载预编译二进制并校验安装。
#
# 用法（curl | sh）：
#   curl -fsSL https://raw.githubusercontent.com/uzziahlin/imagent/main/install.sh | sh
# 指定版本 / 安装路径：
#   curl -fsSL .../install.sh | sh -s -- --version v1.0.0 --bin /usr/local/bin
#   curl -fsSL .../install.sh | sh -s -- --bin "$HOME/.local/bin"
# 环境变量等价：IMAGENT_VERSION=v1.0.0 INSTALL_DIR=... sh install.sh
#
# 校验：下载二进制 + .sha256，本地校验通过后才安装（防供应链篡改）。
# 注意：脚本内所有 shell 变量一律用 ${var} 显式包裹，避免紧跟多字节字符
# （如中文标点）时被误解析为变量名延续。

set -eu

OWNER=uzziahlin
REPO=imagent
VERSION="${IMAGENT_VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --bin) INSTALL_DIR="$2"; shift 2 ;;
    --help|-h)
      echo "usage: install.sh [--version <tag|latest>] [--bin <dir>]"; exit 0 ;;
    *) echo "unknown arg: $1 (--help for usage)" >&2; exit 1 ;;
  esac
done

# --- 平台检测 → release asset 名（须与 .github/workflows/release.yml matrix 一致）---
OS=$(uname -s)
ARCH=$(uname -m)
case "$OS" in
  Darwin) os=darwin ;;
  Linux)  os=linux ;;
  *) echo "X unsupported OS: ${OS} (imagent supports macOS / Linux only)" >&2; exit 1 ;;
esac
case "$ARCH" in
  arm64|aarch64) arch=arm64 ;;
  x86_64|amd64)  arch=x86_64 ;;
  *) echo "X unsupported arch: ${ARCH}" >&2; exit 1 ;;
esac
case "${os}-${arch}" in
  darwin-arm64)  asset=imagent-darwin-arm64 ;;
  darwin-x86_64) asset=imagent-darwin-x86_64 ;;
  linux-x86_64)  asset=imagent-linux-x86_64 ;;
  *) echo "X no prebuilt binary for ${os}-${arch}; build from source: cargo build --release" >&2; exit 1 ;;
esac

# --- 下载基址 ---
if [ "${VERSION}" = "latest" ]; then
  base="https://github.com/${OWNER}/${REPO}/releases/latest/download"
else
  base="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}"
fi

# --- 依赖检查 ---
need() { command -v "$1" >/dev/null 2>&1 || { echo "X missing dependency: $1" >&2; exit 1; }; }
need curl
need uname

tmpdir=$(mktemp -d 2>/dev/null || mktemp -d -t imagent)
trap 'rm -rf "${tmpdir}"' EXIT INT TERM

echo "-> ${os}-${arch} | ${VERSION} | ${INSTALL_DIR}"
echo "-> downloading ${asset} ..."
curl -fsSL "${base}/${asset}"        -o "${tmpdir}/${asset}"
curl -fsSL "${base}/${asset}.sha256" -o "${tmpdir}/${asset}.sha256"

# --- sha256 校验（macOS 用 shasum，Linux 用 sha256sum；二者 -c 格式兼容）---
echo "-> verifying sha256 ..."
if command -v sha256sum >/dev/null 2>&1; then
  (cd "${tmpdir}" && sha256sum -c "${asset}.sha256")
elif command -v shasum >/dev/null 2>&1; then
  (cd "${tmpdir}" && shasum -a 256 -c "${asset}.sha256")
else
  echo "! no sha256sum/shasum; skipping integrity check" >&2
fi

# --- 安装 ---
if [ ! -d "${INSTALL_DIR}" ]; then
  mkdir -p "${INSTALL_DIR}" 2>/dev/null || {
    echo "X cannot create ${INSTALL_DIR}; pass --bin <writable-dir> (e.g. \$HOME/.local/bin)" >&2; exit 1; }
fi

target="${INSTALL_DIR}/imagent"
if [ -w "${INSTALL_DIR}" ]; then
  mv "${tmpdir}/${asset}" "${target}"
else
  echo "-> ${INSTALL_DIR} needs elevated rights, using sudo ..."
  sudo mv "${tmpdir}/${asset}" "${target}"
fi
chmod +x "${target}"

echo "OK installed: ${target}"
if command -v imagent >/dev/null 2>&1; then
  imagent --version 2>/dev/null || echo "  (run 'imagent --version' to check)"
else
  echo "  add ${INSTALL_DIR} to PATH to use 'imagent' (may need a new shell)"
fi
