# 字体子集化派生脚本（SIZE-1，2026-11-15）。
#
# 用途：从 translations/*.json 九语言词条并集 ∪ ASCII ∪ 通用标点 ∪ Latin-1 ∪
# Latin Extended-A 派生 unicodes，用 fontTools.subset（pyftsubset）对
# assets/fonts/*.ttf 做保守子集化后原位替换。
#
# 纪律：码点清单绝不硬编码入仓——每次运行时从 translations 派生，后续词条
# 新增字符会自动被收进子集，不会静默混排退 fallback。脚本可重复执行。
#
# 行为差（已裁决接受，SIZE-1）：西里尔/希腊等超出派生集的码点字形被删除，
# 相关曲名/歌词从 Inter 退到系统 fallback——与 CJK 现状同机制
# （87b90867ef 先例）。回退方式：git checkout -- assets/fonts/。
#
# 依赖：python -m pip install fonttools
# 用法：
#   python tools/subset_bundled_fonts.py          # 重新生成并原位替换字体
#   python tools/subset_bundled_fonts.py --check  # 门禁检查当前字体是否与词条对齐

import hashlib
import json
import os
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
TRANSLATIONS = ROOT / "translations"
FONTS = ROOT / "assets" / "fonts"

# 固定 Unicode 区块（拉丁语系 UI 与常见元数据的字形底盘）：
#   ASCII 可打印 / Latin-1 Supplement 打印区（跳过 C1 控制区，无字形）/
#   Latin Extended-A（中欧、土耳其语变音符）/ General Punctuation
#   （省略号、弯引号、破折号、零宽空格等）。区块之外的词条字符（如越南语
#   的 Latin Extended Additional）由 translations 并集兜住。
FIXED_RANGES = [
    (0x0020, 0x007E),  # ASCII 可打印
    (0x00A0, 0x00FF),  # Latin-1 Supplement 打印区
    (0x0100, 0x017F),  # Latin Extended-A
    (0x2000, 0x206F),  # General Punctuation
]

# 保守参数：保留全部 OpenType layout 特性与全部 name 记录，收紧留后。
SUBSET_ARGS = [
    "--layout-features=*",
    "--name-IDs=*",
]


def collect_translation_chars() -> set[int]:
    """递归收集 translations/*.json（排除 meta.json——它是行号引用元数据）
    中所有键与字符串值的码点。"""
    chars: set[int] = set()

    def walk(obj) -> None:
        if isinstance(obj, str):
            chars.update(ord(ch) for ch in obj)
        elif isinstance(obj, dict):
            for key, value in obj.items():
                chars.update(ord(ch) for ch in key)  # 键兜底收进（均为 ASCII）
                walk(value)
        elif isinstance(obj, list):
            for item in obj:
                walk(item)

    for path in sorted(TRANSLATIONS.glob("*.json")):
        if path.name == "meta.json":
            continue
        walk(json.loads(path.read_text(encoding="utf-8")))
    return chars


def derive_codepoints() -> list[int]:
    """派生最终码点集：词条并集 ∪ 固定区块，过滤控制字符，升序返回。"""
    chars = collect_translation_chars()
    for start, end in FIXED_RANGES:
        chars.update(range(start, end + 1))
    return sorted(cp for cp in chars if cp >= 0x0020 and not (0x007F <= cp < 0x00A0))


def compact_ranges(cps: list[int]) -> list[tuple[int, int]]:
    """把码点序列压缩为闭区间列表（pyftsubset --unicodes 的紧凑形式）。"""
    ranges: list[tuple[int, int]] = []
    for cp in cps:
        if ranges and cp == ranges[-1][1] + 1:
            ranges[-1] = (ranges[-1][0], cp)
        else:
            ranges.append((cp, cp))
    return ranges


def subset_font(path: pathlib.Path, unicodes: str, tmp_out: pathlib.Path) -> None:
    """等价 pyftsubset：经 python -m fontTools.subset 调用，参数与 CLI 同源。
    先写临时文件，由调用方决定是原子替换原文件还是仅作门禁哈希比对。"""
    subprocess.run(
        [
            sys.executable,
            "-m",
            "fontTools.subset",
            str(path),
            f"--unicodes={unicodes}",
            f"--output-file={tmp_out}",
            *SUBSET_ARGS,
        ],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def main() -> None:
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")

    check_only = "--check" in sys.argv
    codepoints = derive_codepoints()
    ranges = compact_ranges(codepoints)
    unicodes = ",".join(f"U+{a:04X}-{b:04X}" if b > a else f"U+{a:04X}" for a, b in ranges)
    print(f"派生码点 {len(codepoints)} 个 / {len(ranges)} 个区间 (模式: {'检查' if check_only else '刷新'})")

    stale_fonts = []
    for path in sorted(FONTS.glob("*.ttf")):
        before = path.stat().st_size
        before_hash = hashlib.sha256(path.read_bytes()).hexdigest()
        tmp = path.with_suffix(".ttf.subset.tmp")
        try:
            subset_font(path, unicodes, tmp)
            after_hash = hashlib.sha256(tmp.read_bytes()).hexdigest()
            after = tmp.stat().st_size
            if check_only:
                if before_hash != after_hash:
                    stale_fonts.append(path.name)
                    print(f"❌ {path.name:26} 字体与当前词条未对齐 (当前 {before} B -> 派生应为 {after} B)")
                else:
                    print(f"✓ {path.name:26} 字体与词条完全对齐 ({after} B)")
                if tmp.exists():
                    tmp.unlink()
            else:
                os.replace(tmp, path)
                print(f"{path.name:26} {before:>9} -> {after:>7} B  ({before - after:+8d})")
        finally:
            if check_only and tmp.exists():
                tmp.unlink()

    if check_only and stale_fonts:
        print(f"\n错误: 以下字体文件未同步最新词条派生集: {', '.join(stale_fonts)}", file=sys.stderr)
        print("请运行 'python tools/subset_bundled_fonts.py' 刷新字体后提交！", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
