#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
manifest="$root/tools/ptc-guest/Cargo.toml"
asset="$root/crates/moc-core/assets/ptc-guest.wasm"
for binary in rustc cargo clang sha256sum; do
  command -v "$binary" >/dev/null || { printf '缺少构建工具: %s\n' "$binary" >&2; exit 1; }
done
rustup target list --installed | rg -q '^wasm32-unknown-unknown$' || { printf '缺少 rustup target: wasm32-unknown-unknown\n' >&2; exit 1; }
clang -target wasm32-unknown-unknown -x c -c -o /dev/null /dev/null || { printf 'clang 不支持 wasm32\n' >&2; exit 1; }
if command -v llvm-ar >/dev/null; then
  ar="$(command -v llvm-ar)"
else
  host="$(rustc -vV | rg '^host:' | cut -d ' ' -f 2)"
  ar="$(rustc --print sysroot)/lib/rustlib/$host/bin/llvm-ar"
  test -x "$ar" || { printf '缺少 llvm-ar\n' >&2; exit 1; }
fi
export CC_wasm32_unknown_unknown=clang
export AR_wasm32_unknown_unknown="$ar"
cargo build --manifest-path "$manifest" --target wasm32-unknown-unknown --release --locked
mkdir -p "$(dirname "$asset")"
cp "$root/tools/ptc-guest/target/wasm32-unknown-unknown/release/moc_ptc_guest.wasm" "$asset"
{
  printf 'ABI=1\n'
  rustc -Vv
  clang --version
  "$ar" --version
  (cd "$root" && sha256sum tools/ptc-guest/Cargo.toml tools/ptc-guest/Cargo.lock tools/ptc-guest/src/lib.rs tools/ptc-guest/src/prelude.js crates/moc-core/src/tools/ptc/wire.rs crates/moc-core/assets/ptc-guest.wasm)
} > "$(dirname "$asset")/ptc-guest-build.txt"
