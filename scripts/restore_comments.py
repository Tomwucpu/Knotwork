# -*- coding: utf-8 -*-
"""注释恢复工具（i18n sweep 事故修复专用，一次性）。

原理：当前文件（注释被误删、代码正确）与基准文件（注释完整、代码较旧）
按"规范化代码行"做 difflib 对齐：等值块的注释从基准回填；
当前独有行（新代码）原样保留（其注释随后手工补）。

用法：python scripts/restore_comments.py <base> <current>
"""

import difflib
import io
import sys


def is_comment(line):
    s = line.strip()
    return s.startswith("//")


def is_blank(line):
    return line.strip() == ""


def units(raw_lines):
    """把原始行切成单元：前置注释/空行 + 一行锚点代码。

    返回 [(norm_anchor, raw_start, raw_end)]，raw_end 含锚点行。
    """
    units = []
    pending = []
    for idx, line in enumerate(raw_lines):
        if is_comment(line) or is_blank(line):
            pending.append(idx)
            continue
        norm = line.strip()
        units.append((norm, pending[0] if pending else idx, idx))
        pending = []
    # 尾部悬挂注释并入最后单元之后的输出（直接附到最后一个单元）。
    if pending and units:
        last = units[-1]
        units[-1] = (last[0], last[1], pending[-1])
    return units


def restore(base_lines, cur_lines):
    base_u = units(base_lines)
    cur_u = units(cur_lines)
    sm = difflib.SequenceMatcher(
        None, [u[0] for u in base_u], [u[0] for u in cur_u], autojunk=False
    )
    out = []
    recovered = 0
    for tag, i1, i2, j1, j2 in sm.get_opcodes():
        if tag == "equal":
            start = min(base_u[i1][1], base_u[i2 - 1][2] + 1)
            end = base_u[i2 - 1][2] + 1
            block = base_lines[start:end]
            recovered += sum(1 for l in block if is_comment(l))
            out.extend(block)
        elif tag in ("replace", "insert"):
            start = min(cur_u[j1][1], cur_u[j2 - 1][2] + 1)
            end = cur_u[j2 - 1][2] + 1
            out.extend(cur_lines[start:end])
        # delete：基准独有（已删除的代码），跳过。
    return out, recovered


def main():
    base_path, cur_path = sys.argv[1], sys.argv[2]
    base = io.open(base_path, encoding="utf-8").read().splitlines(keepends=True)
    cur = io.open(cur_path, encoding="utf-8").read().splitlines(keepends=True)
    out, recovered = restore(base, cur)
    io.open(cur_path, "w", encoding="utf-8", newline="\n").write("".join(out))
    print(f"{cur_path}: 回填注释 {recovered} 行，输出 {len(out)} 行")


if __name__ == "__main__":
    main()
