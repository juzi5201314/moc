#!/usr/bin/env bash
# 生成 moc-tui 内嵌的图表字体子集。
#
# 图表里的文字（中文类别名、拉丁数值）必须与像素 1:1 落位才能保持锐利，
# 因此字体随 crate 内嵌、固定字号光栅化，不走系统字体（系统字体在各机器上
# 的字形与度量不同，会让同一张图在不同机器上宽度漂移）。内嵌字体只覆盖
# 常用字面，缺字时渲染层才懒回退系统字体。
#
# 子集来源：Noto Sans Mono CJK SC Regular（SIL OFL 1.1，无 Reserved Font Name，
# 允许子集化后再分发）。原字体 16 MB，子集后约 3~5 MB。
#
# 用法：./scripts/subset-font.sh
# 更新子集规则后必须重新运行并提交产物：
#   crates/moc-tui/assets/fonts/NotoSansMonoCJKsc-Regular.subset.otf
set -euo pipefail

cd "$(dirname "$0")/.."

SOURCE_URL="https://raw.githubusercontent.com/notofonts/noto-cjk/main/Sans/Mono/NotoSansMonoCJKsc-Regular.otf"
LICENSE_URL="https://raw.githubusercontent.com/notofonts/noto-cjk/main/Sans/LICENSE"
ASSET_DIR="crates/moc-tui/assets/fonts"
OUTPUT="$ASSET_DIR/NotoSansMonoCJKsc-Regular.subset.otf"
SOURCE="/tmp/NotoSansMonoCJKsc-Regular.otf"
HANZI="/tmp/moc-chart-hanzi.txt"

mkdir -p "$ASSET_DIR"

# 汉字表：GB2312 一二级汉字区（0xB0A1-0xF7FE），共 6763 字，覆盖现代汉语常用字。
# 用 Python 自带的 gb2312 编解码枚举，避免额外依赖与手工维护的字表。
python3 - "$HANZI" <<'PY'
import sys

out = []
for high in range(0xB0, 0xF8):
    for low in range(0xA1, 0xFF):
        try:
            out.append(bytes((high, low)).decode("gb2312"))
        except UnicodeDecodeError:
            continue
with open(sys.argv[1], "w", encoding="utf-8") as fh:
    fh.write("".join(out))
print(f"汉字表 {len(out)} 字")
PY

curl -fsSL "$SOURCE_URL" -o "$SOURCE"
curl -fsSL "$LICENSE_URL" -o "$ASSET_DIR/OFL.txt"

# 子集：ASCII、拉丁补充、常用标点/箭头/数学/制表/几何/杂项符号 + GB2312 汉字。
# 这些区段就是图表里会出现的字符集（数值、单位、图例箭头、状态记号）。
uvx --from fonttools pyftsubset "$SOURCE" \
    --output-file="$OUTPUT" \
    --unicodes="U+0020-007E,U+00A0-00FF,U+2010-2027,U+2030-205E,U+20A0-20BF,U+2190-21FF,U+2200-22FF,U+2500-257F,U+25A0-25FF,U+2600-26FF" \
    --text-file="$HANZI" \
    --layout-features='' \
    --no-hinting \
    --desubroutinize \
    --drop-tables+=DSIG

printf '子集产物 %s（%s 字节）\n' "$OUTPUT" "$(wc -c <"$OUTPUT")"
