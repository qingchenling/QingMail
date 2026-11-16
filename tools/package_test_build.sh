#!/usr/bin/env bash
# 生成 P2 测试包：release 二进制 + 验收说明 + 运行脚本 + 校验和/压缩包。
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="0.1.0-p2-test1"
out="$root/dist/qingmail-$version"

cd "$root"
cargo build --release -p mail-ui --locked
rm -rf "$out"
mkdir -p "$out"
cp target/release/mail-ui "$out/mail-ui"
strip --strip-unneeded "$out/mail-ui"
cp docs/test-build-README.md "$out/README-TEST.md"
cat > "$out/run-qingmail.sh" <<'RUN'
#!/bin/sh
DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec env \
  WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-1}" \
  VK_DRIVER_FILES="${VK_DRIVER_FILES:-/usr/share/vulkan/icd.d/intel_icd.json}" \
  "$DIR/mail-ui" "$@"
RUN
chmod 755 "$out/run-qingmail.sh" "$out/mail-ui"
(
  cd "$out"
  sha256sum mail-ui README-TEST.md run-qingmail.sh > SHA256SUMS
)
tar -C "$root/dist" -czf "$root/dist/qingmail-$version.tar.gz" "qingmail-$version"
echo "built: $out"
echo "tarball: $root/dist/qingmail-$version.tar.gz"
