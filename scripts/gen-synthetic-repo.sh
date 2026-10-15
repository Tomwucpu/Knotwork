#!/usr/bin/env bash
# 生成合成基准仓库（可复现，无需联网）。
#
# 通过 `git fast-import` 写入提交，速度远高于逐次 `git commit`：
# 100 万条提交通常在数十秒内完成，用于 M0 POC-2（大仓库虚拟化滚动）
# 与 POC-4（泳道图增量布局）的稳定复现。
#
# 生成的图结构：一条 main 主线 + 若干"分叉后立即合并"的旁支，
# 用于同时覆盖长线性段（可折叠）与合并气泡（泳道切换）两种渲染路径。
#
# 用法：
#   ./scripts/gen-synthetic-repo.sh <目标目录> [--commits N] [--branches K] [--branch-len L]
#
# 示例：
#   ./scripts/gen-synthetic-repo.sh test-repos/repos/small    --commits 1000
#   ./scripts/gen-synthetic-repo.sh test-repos/repos/medium   --commits 50000   --branches 20
#   ./scripts/gen-synthetic-repo.sh test-repos/repos/large    --commits 1000000 --branches 50

set -euo pipefail

usage() {
    sed -n '2,20p' "$0"
}

target=""
commits=10000
branches=3
branch_len=20

while [[ $# -gt 0 ]]; do
    case "$1" in
        --commits)    commits="$2";    shift 2 ;;
        --branches)   branches="$2";   shift 2 ;;
        --branch-len) branch_len="$2"; shift 2 ;;
        -h|--help)    usage; exit 0 ;;
        -*)           echo "未知参数: $1" >&2; usage >&2; exit 2 ;;
        *)            target="$1"; shift ;;
    esac
done

if [[ -z "$target" ]]; then
    echo "错误：必须提供目标目录" >&2
    usage >&2
    exit 2
fi

if [[ -e "$target/.git" ]]; then
    echo "目标已是 Git 仓库，拒绝覆盖：$target/.git" >&2
    exit 1
fi

mkdir -p "$target"
git -C "$target" init -q -b main
# 基准数据必须与平台换行策略无关
git -C "$target" config core.autocrlf false

echo "生成合成仓库：commits=$commits branches=$branches branch-len=$branch_len -> $target"

awk -v N="$commits" -v K="$branches" -v L="$branch_len" '
function emit_commit(ref, mk, msg, from_mark, merge_mark, with_file) {
    print "commit " ref;
    print "mark :" mk;
    print "author " AUTHOR " " TS " +0800";
    print "committer " AUTHOR " " TS " +0800";
    print "data " length(msg);
    printf "%s", msg;
    print "";
    if (from_mark != "")  print "from :" from_mark;
    if (merge_mark != "") print "merge :" merge_mark;
    if (with_file) print "M 100644 :1 file.txt";
    print "";
    TS++;
}

BEGIN {
    AUTHOR = "Bench <bench@example.invalid>";
    TS = 1700000000;

    payload = "benchmark payload\n";
    print "blob";
    print "mark :1";
    print "data " length(payload);
    printf "%s", payload;
    print "";

    mk = 2;
    bubble = 0;
    last_fork = 0;

    emit_commit("refs/heads/main", mk++, "init\n", "", "", 1);

    for (i = 1; i < N; i++) {
        is_fork = 0;
        if (bubble < K) {
            f = int(N * (bubble + 1) / (K + 1));
            if (i == f && f > last_fork) {
                is_fork = 1;
            }
        }

        if (is_fork) {
            fork_mark = mk - 1;
            last_fork = i;
            bname = "refs/heads/bench-" (bubble + 1);
            branch_head = "";

            for (j = 1; j <= L; j++) {
                bm = mk++;
                parent = (j == 1) ? fork_mark : "";
                emit_commit(bname, bm, "b" (bubble + 1) "-" j "\n", parent, "", 0);
                branch_head = bm;
            }

            emit_commit("refs/heads/main", mk++, "merge " (bubble + 1) "\n", "", branch_head, 0);
            bubble++;
        } else {
            emit_commit("refs/heads/main", mk++, "c" i "\n", "", "", 0);
        }
    }

    print "done";
}
' | tr -d '\r' | git -C "$target" fast-import --quiet

git -C "$target" reset --hard -q main

total_commits=$(git -C "$target" rev-list --count main)
size=$(du -sh "$target/.git" 2>/dev/null | cut -f1)
echo "完成：main 提交数 $total_commits，.git 体积 $size"
