#!/usr/bin/env bash
# 单邮箱测试包：release 程序、启动脚本、说明、构建信息与 SHA256。
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="0.1.0-p2-test2"
out="$root/dist/qingmail-$version"
archive="$out.tar.gz"

cd "$root"
if [[ -e "$out" || -e "$archive" ]]; then
  echo "测试包已存在，请先移走原包再重新构建：$out" >&2
  exit 1
fi
# 构建信息中的提交必须与 Rust 源码、依赖和构建配置一致。
git diff --quiet HEAD -- Cargo.toml Cargo.lock rust-toolchain.toml crates .cargo
cargo build --release -p mail-ui --locked
mkdir -p "$out"
cp target/release/mail-ui "$out/mail-ui"
strip --strip-unneeded "$out/mail-ui"
cp docs/test-build-README.md "$out/README-TEST.md"
cat > "$out/run-qingmail.sh" <<'RUN'
#!/bin/sh
DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
# 遵循当前 Wayland 会话和显式驱动设置；本机 Intel 驱动可用时才作为默认值。
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-1}"
if [ -z "${VK_DRIVER_FILES+x}" ] && [ -f /usr/share/vulkan/icd.d/intel_icd.json ]; then
  export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/intel_icd.json
fi
exec "$DIR/mail-ui" "$@"
RUN
chmod 755 "$out/run-qingmail.sh" "$out/mail-ui"
{
  echo "package=qingmail-$version"
  echo "source_commit=$(git rev-parse HEAD)"
  echo "built_at_utc=$(date -u +%FT%TZ)"
  echo "architecture=$(uname -m)"
  echo "rustc=$(rustc --version)"
  echo "profile=release; locked dependencies; stripped"
} > "$out/BUILD-INFO.txt"
(
  cd "$out"
  ./run-qingmail.sh --version
  sha256sum mail-ui README-TEST.md run-qingmail.sh BUILD-INFO.txt > SHA256SUMS
  sha256sum -c SHA256SUMS
)
tar -C "$root/dist" -czf "$archive" "qingmail-$version"
(
  cd "$root/dist"
  sha256sum "qingmail-$version.tar.gz" > "qingmail-$version.tar.gz.sha256"
)
echo "built: $out"
echo "tarball: $archive"
