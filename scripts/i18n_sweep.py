# -*- coding: utf-8 -*-
"""i18n 文案清扫工具（NFR-L10N-01，M5）。

把 ui/*.slint 与 session.rs / view_model.rs 中的中文文案替换为
Texts.<id>（Slint，见 ui/texts.slint）与 crate::text::tr(...)（Rust，
见 gt-ui/src/text.rs），并生成这两份资源文件。

用法：python scripts/i18n_sweep.py [--check]
  --check  只校验（退出码非 0 表示存在游离文案），不写文件。
幂等：替换后再次运行应报告"无游离文案"。
"""

import io
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from i18n_maps import SLINT_MAP, RUST_MAP  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
UI = os.path.join(ROOT, "crates", "gt-ui", "ui")
SRC = os.path.join(ROOT, "crates", "gt-ui", "src")

CJK = re.compile(r"[\u4e00-\u9fff]")
LIT = re.compile(r'"((?:[^"\\]|\\.)*)"')

SLINT_FILES = ["main-window.slint", "dialogs.slint", "rows.slint", "widgets.slint"]
RUST_FILES = ["session.rs", "view_model.rs"]


def iter_code_lines(text):
    """逐行产出 (行号, 行文本)，跳过注释行；Rust 侧跳过 #[cfg(test)] 模块。"""
    in_test = False
    for lineno, line in enumerate(io.StringIO(text), 1):
        if "#[cfg(test)]" in line:
            in_test = True
        if in_test:
            continue
        stripped = line.strip()
        if stripped.startswith("//"):
            continue
        yield lineno, line


def find_free_literals(text):
    """返回文本中不在映射表里的中文文案。"""
    free = []
    for _, line in iter_code_lines(text):
        for m in LIT.finditer(line):
            literal = m.group(1)
            if CJK.search(literal) and literal not in SLINT_MAP and literal not in RUST_MAP:
                free.append(literal)
    return free


def load_existing_texts():
    """读出既有 texts.slint 的全部属性（含手工维护条目），保证幂等合并。"""
    path = os.path.join(UI, "texts.slint")
    props = {}
    if not os.path.exists(path):
        return props
    for line in io.open(path, encoding="utf-8"):
        m = re.match(r"\s*out property <string> (\w+): (.+);", line)
        if m:
            props[m.group(1)] = m.group(2)
    return props


def sweep_slint(dry_run):
    """把 ui/*.slint 的中文文案替换为 Texts.s<n>，并生成 ui/texts.slint。

    写入路径保留全部注释行（iter_code_lines 只用于扫描语义）。
    """
    entries = []  # (id, zh, en)
    index = {}
    # 新 ID 从既有最大编号续（幂等合并的前提：不与历史 ID 冲突）。
    existing_props = load_existing_texts()
    next_id = 1 + max(
        (int(m.group(1)) for name in existing_props for m in [re.match(r"s(\d+)$", name)] if m),
        default=0,
    )
    for name in SLINT_FILES:
        path = os.path.join(UI, name)
        text = io.open(path, encoding="utf-8").read()
        if CJK.search(text) is None:
            continue
        in_test = False
        out_lines = []
        for line in io.StringIO(text):
            stripped = line.strip()
            if "#[cfg(test)]" in line:
                in_test = True
            if in_test or stripped.startswith("//"):
                # 注释行原样保留 —— 它们是代码的一部分，不是文案。
                out_lines.append(line)
                continue
            if CJK.search(line) is None:
                out_lines.append(line)
                continue
            def repl(m):
                literal = m.group(1)
                if not CJK.search(literal):
                    return m.group(0)
                key = literal
                if key not in index:
                    sid = f"s{next_id}"
                    next_id += 1
                    index[key] = sid
                    en = SLINT_MAP.get(key)
                    if en is None:
                        raise SystemExit(f"缺少 Slint 译文：{name} {literal!r}")
                    entries.append((sid, key, en))
                return f"Texts.{index[key]}"
            out_lines.append(LIT.sub(repl, line))
        if not dry_run:
            io.open(path, "w", encoding="utf-8", newline="\n").write("".join(out_lines))

    # texts.slint 幂等生成：既有属性（含手工条目）与新条目合并。
    if not dry_run:
        existing = load_existing_texts()
        for sid, zh, en in entries:
            en_escaped = en.replace("\\", "\\\\").replace('"', '\\"')
            zh_escaped = zh.replace("\\", "\\\\").replace('"', '\\"')
            existing[sid] = f'root.en ? "{en_escaped}" : "{zh_escaped}"'

        def prop_sort_key(name):
            m = re.match(r"s(\d+)$", name)
            return (0, int(m.group(1))) if m else (1, name)

        lines = [
            "// Knotwork 界面静态文案（NFR-L10N-01，M5；由 scripts/i18n_sweep.py 生成）。",
            "//",
            "// 源语言为中文；`en` 为真时输出英文。窗口在 init 与语言变更时写入",
            "// `en`（config.appearance.language），多窗口共享同一份（global 单例）。",
            "// 本文件是 ui/*.slint 中唯一允许出现中文文案字面量的地方，",
            "// 扫描测试（gt-ui/tests/text_scan.rs）据此把关。",
            "",
            "export global Texts {",
            "    in-out property <bool> en: false;",
            "",
        ]
        for sid in sorted(existing, key=prop_sort_key):
            lines.append(f"    out property <string> {sid}: {existing[sid]};")
        lines.append("}")
        io.open(os.path.join(UI, "texts.slint"), "w", encoding="utf-8", newline="\n").write(
            "\n".join(lines)
        )
    return len(entries)
    return len(entries)


def sweep_rust(dry_run):
    """把 session.rs / view_model.rs 的中文文案替换为 crate::text::tr("...")。"""
    missing = []
    count = 0
    for name in RUST_FILES:
        path = os.path.join(SRC, name)
        text = io.open(path, encoding="utf-8").read()
        if CJK.search(text) is None:
            continue
        in_test = False
        out_lines = []
        for line in io.StringIO(text):
            stripped = line.strip()
            if "#[cfg(test)]" in line:
                in_test = True
            if in_test or stripped.startswith("//"):
                # 注释行原样保留。
                out_lines.append(line)
                continue
            if CJK.search(line) is None:
                out_lines.append(line)
                continue

            def repl(m):
                nonlocal count
                literal = m.group(1)
                if not CJK.search(literal):
                    return m.group(0)
                # 幂等守卫：已经是 text::tr(...) 的字面量不再包一层。
                if line[: m.start()].endswith("text::tr(") or line[: m.start()].endswith("text::trf("):
                    return m.group(0)
                if literal not in RUST_MAP:
                    missing.append(f"{name} {literal!r}")
                    return m.group(0)
                count += 1
                return f'text::tr("{literal}")'

            out_lines.append(LIT.sub(repl, line))
        if not dry_run:
            io.open(path, "w", encoding="utf-8", newline="\n").write("".join(out_lines))
    return count, missing


def generate_text_rs():
    """生成 gt-ui/src/text.rs（动态文案资源）。"""
    lines = [
        "//! 界面动态文案（NFR-L10N-01，M5；由 `scripts/i18n_sweep.py` 的映射表驱动）。",
        "//!",
        "//! Slint 侧的静态文案在 `ui/texts.slint`；本模块负责 Rust 侧拼出来的",
        "//! 动态文案（提示条、确认框、标题等）。键即中文原文（源语言），",
        "//! 英文未收录时回落中文 —— 缺译不算缺陷，游离文案才算（扫描测试把关）。",
        "",
        "use std::sync::atomic::{AtomicBool, Ordering};",
        "",
        "static EN: AtomicBool = AtomicBool::new(false);",
        "",
        "/// 切换界面语言（应用启动与设置保存时调用）。",
        "pub fn set_english(enabled: bool) {",
        "    EN.store(enabled, Ordering::Relaxed);",
        "}",
        "",
        "/// 当前是否英文界面。",
        "#[must_use]",
        "pub fn is_english() -> bool {",
        "    EN.load(Ordering::Relaxed)",
        "}",
        "",
        "/// 取动态文案：以中文原文为键，英文缺失时回落原文。",
        "#[must_use]",
        "pub fn tr(zh: &'static str) -> &'static str {",
        "    if !is_english() {",
        "        return zh;",
        "    }",
        "    translate(zh).unwrap_or(zh)",
        "}",
        "",
        "/// 带占位符的动态文案：模板中的 `{}` 依序用 `args` 替换。",
        "///",
        "/// `format!` 的模板必须是字面量，因此带参数的文案走这里：",
        "/// `text::trf(\"{} 失败\", &[&kind])`。",
        "#[must_use]",
        "pub fn trf(zh: &'static str, args: &[&str]) -> String {",
        "    let mut out = tr(zh).to_owned();",
        "    for arg in args {",
        "        out = out.replacen(\"{}\", arg, 1);",
        "    }",
        "    out",
        "}",
        "",
        "/// 静态映射表：中文原文 → 英文。缺译回落原文（见 [`tr`]）。",
        "#[allow(clippy::too_many_lines)]",
        "fn translate(zh: &str) -> Option<&'static str> {",
        "    Some(match zh {",
    ]
    for zh, en in RUST_MAP.items():
        en_escaped = en.replace('"', '\\"')
        zh_escaped = zh.replace('"', '\\"')
        lines.append(f'        "{zh_escaped}" => "{en_escaped}",')
    lines.append("        _ => return None,")
    lines.append("    })")
    lines.append("}")
    io.open(os.path.join(SRC, "text.rs"), "w", encoding="utf-8", newline="\n").write(
        "\n".join(lines)
    )


def main():
    dry = "--check" in sys.argv
    n_slint = sweep_slint(dry)
    n_rust, missing = sweep_rust(dry)
    if missing:
        print("缺少 Rust 译文：")
        for item in missing:
            print("  ", item)
        sys.exit(1)
    if not dry:
        generate_text_rs()
    print(f"slint 条目：{n_slint}，rust 替换点：{n_rust}")
    # 终检：两层都不应再有游离中文（texts.slint / text.rs 除外）。
    leftover = []
    for name in SLINT_FILES:
        text = io.open(os.path.join(UI, name), encoding="utf-8").read()
        leftover += [("ui/" + name, s) for s in find_free_literals(text)]
    for name in RUST_FILES:
        text = io.open(os.path.join(SRC, name), encoding="utf-8").read()
        leftover += [("src/" + name, s) for s in find_free_literals(text)]
    if leftover:
        print("仍有游离文案：")
        for where, s in leftover:
            print("  ", where, repr(s))
        sys.exit(1)
    print("OK：无游离文案")


if __name__ == "__main__":
    main()
