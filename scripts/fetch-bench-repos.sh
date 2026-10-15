#!/usr/bin/env bash
# 拉取真实基准仓库（可选，体积较大，需联网）。
#
# 使用 blobless 部分克隆（--filter=blob:none）：只下载提交与树对象，
# 不下载文件内容，历史规模完整而体积仅为完整克隆的一小部分 ——
# 这正是 Log / revwalk / 泳道图测试所需的形态。
#
# 用法：
#   ./scripts/fetch-bench-repos.sh [输出目录，默认 test-repos/repos]
#
# 注意：
#   - linux 内核仓库即使 blobless 也有 1GB 量级，请按需拉取；
#   - 合成仓库（gen-synthetic-repo.sh）在多数场景下已足够，且完全可复现。

set -euo pipefail

out="${1:-test-repos/repos}"
mkdir -p "$out"

clone_blobless() {
    local url="$1"
    local dir="$2"
    local desc="$3"

    if [[ -e "$out/$dir/.git" ]]; then
        echo "已存在，跳过：$dir"
        return 0
    fi

    echo "拉取 $desc -> $out/$dir"
    git clone --filter=blob:none --no-checkout --quiet "$url" "$out/$dir"
    local count
    count=$(git -C "$out/$dir" rev-list --count HEAD)
    echo "  完成：$count 个提交"
}

# 中档：约 8 万提交，用于日常一致性测试
clone_blobless "https://github.com/git/git.git"        "git-git" "git/git（约 8 万提交）"

# 大档：约 140 万提交，用于 M0 POC-2 与内存回归
echo ""
echo "提示：以下仓库体积较大（1GB 量级），可按需执行："
echo "  git clone --filter=blob:none --no-checkout https://github.com/torvalds/linux.git $out/linux"
