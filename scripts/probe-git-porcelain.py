#!/usr/bin/env python3
"""探明 git 的 porcelain 语义，作为实现与测试的依据。

本项目把"与 git CLI 行为一致"当作核心卖点，因此 `gt-core::status` 里的
porcelain 映射与路径引号规则都不是猜出来的，而是由本脚本在**本机实际的 git**
上实测得出。git 版本变更后重跑本脚本即可确认规则是否仍然成立。

覆盖三组语义：

1. 路径的 C 风格引号 —— 何时加引号、如何转义（含 `core.quotepath` 的影响）；
2. 重命名行的引号 —— 两侧是否独立渲染；
3. 索引阶段组合与冲突码 —— `XX/AU/UD/UA/DU/AA/UU` 的判定来源。

用法：`python scripts/probe-git-porcelain.py`
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile

# Windows 上部分文件名（含制表符、双引号、通配符、控制字符）无法创建，
# 脚本会跳过并提示，不影响其余用例。
PATHS_NEEDING_QUOTES = {
    "plain": "plain.txt",
    "space": "with space.txt",
    "double_space": "a  b.txt",
    "trailing_space": "trail .txt",
    "hash": "with#hash.txt",
    "bracket": "with[bracket].txt",
    "tilde": "with~tilde.txt",
    "leading_dash": "-dash.txt",
    "cjk": "中文.txt",
    "accent": "café.txt",
    "emoji": "emoji-🎯.txt",
    "tab": "with\ttab.txt",
    "quote": 'with"quote.txt',
    "backslash": "with\\backslash.txt",
    "ctrl": "ctrl\x01.txt",
    "del": "del\x7f.txt",
}

RENAME_CASES = {
    "plain": ("a.txt", "b.txt"),
    "new_has_space": ("a.txt", "b c.txt"),
    "old_has_space": ("a b.txt", "c.txt"),
    "both_have_space": ("a b.txt", "c d.txt"),
    "new_is_cjk": ("a.txt", "中文.txt"),
}

# stage 位掩码 → porcelain 冲突码（git 的 `wt_status_unmerged_status_string`）。
STAGE_COMBOS = (
    ("UU", (1, 2, 3)),
    ("AA", (2, 3)),
    ("AU", (2,)),
    ("UA", (3,)),
    ("DU", (1, 3)),
    ("UD", (1, 2)),
    ("DD", (1,)),
)


class Repo:
    """一个临时仓库，提供文本与二进制两条 git 通道。"""

    def __init__(self, prefix: str) -> None:
        self.root = tempfile.mkdtemp(prefix=prefix)
        self.git("init", "-q", "-b", "main")
        for key, value in (
            ("user.email", "probe@knotwork.invalid"),
            ("user.name", "Knotwork Probe"),
            ("core.autocrlf", "false"),
            ("core.quotepath", "false"),
        ):
            self.git("config", key, value)

    def git(self, *args: str, stdin: str | None = None, check: bool = True) -> str:
        return self._run(args, None if stdin is None else stdin.encode(), check)

    def git_bytes(self, *args: str, stdin: bytes) -> str:
        """二进制通道：Windows 的文本模式会把 `\\n` 变成 `\\r\\n`，会污染 index-info。"""
        return self._run(args, stdin, True)

    def _run(self, args: tuple[str, ...], stdin: bytes | None, check: bool) -> str:
        result = subprocess.run(
            ["git", *args], cwd=self.root, capture_output=True, input=stdin
        )
        if check and result.returncode != 0:
            raise RuntimeError(
                f"git {' '.join(args)} 失败: "
                f"{result.stderr.decode('utf-8', 'replace').strip()}"
            )
        return result.stdout.decode("utf-8", "replace")

    def write(self, name: str, content: str = "x\n") -> bool:
        try:
            with open(
                os.path.join(self.root, name), "w", encoding="utf-8", newline=""
            ) as handle:
                handle.write(content)
        except OSError as err:
            print(f"    跳过 {name!r}：{err.strerror}", file=sys.stderr)
            return False
        return True

    def commit_all(self, message: str = "seed") -> None:
        self.git("add", "-A")
        self.git("commit", "-q", "-m", message)

    def hash_object(self, content: str) -> str:
        return self.git("hash-object", "-w", "--stdin", stdin=content).strip()

    def porcelain(self) -> list[str]:
        return self.git("status", "--porcelain=v1").splitlines()


def probe_path_quoting() -> None:
    print("=" * 68)
    print("1. 路径的 C 风格引号（git status --porcelain=v1）")
    print("=" * 68)

    for quotepath in ("false", "true"):
        repo = Repo("knotwork-probe-quote-")
        repo.git("config", "core.quotepath", quotepath)
        for name in PATHS_NEEDING_QUOTES.values():
            repo.write(name)
        print(f"\n-- core.quotepath={quotepath} --")
        for line in repo.porcelain():
            print(f"   {line!r}")

    print("\n结论：")
    print("  · 含空格 → 强制加引号（QUOTE_PATH_QUOTE_SP），否则行式格式无法解析；")
    print("  · 控制字符 / DEL / 双引号 / 反斜杠 → 必须转义，控制字符走三位八进制；")
    print("  · 非 ASCII → 仅当 core.quotepath=true 时转义成八进制。")


def probe_rename_quoting() -> None:
    print("\n" + "=" * 68)
    print("2. 重命名行的引号（两侧独立渲染）")
    print("=" * 68)

    for label, (old, new) in RENAME_CASES.items():
        repo = Repo("knotwork-probe-rename-")
        if not repo.write(old, "line\n" * 20):
            continue
        repo.commit_all()
        moved = subprocess.run(
            ["git", "mv", old, new], cwd=repo.root, capture_output=True
        )
        if moved.returncode != 0:
            print(f"\n-- {label}: {old!r} -> {new!r} --")
            print(f"   跳过：{moved.stderr.decode('utf-8', 'replace').strip()}")
            continue
        print(f"\n-- {label}: {old!r} -> {new!r} --")
        for line in repo.porcelain():
            print(f"   {line!r}")

    print("\n结论：git 对旧、新两侧分别套用同一套路径引号规则，再用 ' -> ' 连接。")


def probe_conflict_stages() -> None:
    print("\n" + "=" * 68)
    print("3. 索引阶段组合 → 冲突码")
    print("=" * 68)

    repo = Repo("knotwork-probe-conflict-")
    repo.write("a.txt", "aaaaaaaaaa\nbbbbbbbbbb\n")
    repo.commit_all("base")

    blobs = {
        1: repo.hash_object("base version\n"),
        2: repo.hash_object("our version\n"),
        3: repo.hash_object("their version\n"),
    }

    print("\n-- 清空索引后只注入未合并条目（纯粹未合并路径）--")
    for expected, stages in STAGE_COMBOS:
        repo.git("reset", "-q", "--mixed", "HEAD")
        repo.git("read-tree", "--empty")
        payload = "".join(
            f"100644 {blobs[stage]} {stage}\ta.txt\n" for stage in stages
        )
        repo.git_bytes("update-index", "--index-info", stdin=payload.encode())
        actual = repo.porcelain() or ["<空>"]
        mark = "✓" if actual == [f"{expected} a.txt"] else "✗"
        print(f"   {mark} stages={stages!r:12} 期望={expected} 实际={actual!r}")

    print("\n-- 在已有 stage-0 条目之上注入（git 走另一条判定分支）--")
    for stages in (item[1] for item in STAGE_COMBOS):
        repo.git("reset", "-q", "--mixed", "HEAD")
        payload = "".join(
            f"100644 {blobs[stage]} {stage}\ta.txt\n" for stage in stages
        )
        repo.git_bytes("update-index", "--index-info", stdin=payload.encode())
        print(f"   stages={stages!r:12} 实际={repo.porcelain()!r}")

    print("\n结论：")
    print("  · stage 掩码 = Σ(1 << (stage - 1))，按 1/2/3/4/5/6/7 → DD/AU/UD/UA/DU/AA/UU；")
    print("  · 该映射只在\"索引中不存在同路径 stage-0 条目\"时成立 —— 在既有 stage-0")
    print("    条目之上注入未合并阶段会让 git 走另一条分支（部分组合退化为 UU）。")
    print("    一致性测试因此先用 read-tree --empty 清空索引再注入。")


def main() -> int:
    print(f"git 版本：{subprocess.run(['git', '--version'], capture_output=True, text=True).stdout.strip()}")
    probe_path_quoting()
    probe_rename_quoting()
    probe_conflict_stages()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
