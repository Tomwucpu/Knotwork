# 基准测试仓库资产

本目录不存放仓库本体（见 `.gitignore`），只存放**获取方式与用途定义**。
基准仓库是内存回归（NFR-MEM-03）、性能基准（NFR-PRF-02）与一致性测试（NFR-COR-01）的
共同前提 —— 没有稳定的基准，"低内存"与"与 CLI 一致"这两个卖点无法被验证。

## 四档基准（对应实施计划任务 0-5）

| 档位 | 规模 | 获取方式 | 主要用途 | 运行频率 |
|---|---|---|---|---|
| **S 小** | 1 千提交 | 合成生成 | 单元测试、快速一致性冒烟 | 每次 push |
| **M 中** | 5 万提交 | 合成生成 | 筛选/搜索/分页、日常一致性测试 | 每次 push |
| **L 大** | 100 万提交 | 合成生成 | M0 POC-2 滚动、内存回归、泳道图布局 | 每日 |
| **R 真实** | 8 万提交（git/git） | blobless 克隆 | 真实历史结构下的一致性测试 | 每日 |
| **XL 超大** | 140 万提交（Linux 内核） | blobless 克隆 | 极端场景验证（决策点 D3，v1.0 不承诺） | 手动 |

**优先使用合成仓库。** 它完全可复现、无需联网、体积可控，且能精确构造
"长线性段 + 合并气泡"的图结构 —— 这正是泳道图（FR-LOG-01/05）与
IntelliSort 拓扑排序最需要的测试形态。真实仓库只用于验证"面对真实历史结构的鲁棒性"。

## 生成合成基准仓库

```bash
# S 档：1 千提交
./scripts/gen-synthetic-repo.sh test-repos/repos/small --commits 1000 --branches 3

# M 档：5 万提交
./scripts/gen-synthetic-repo.sh test-repos/repos/medium --commits 50000 --branches 20

# L 档：100 万提交（数十秒量级，取决于磁盘）
./scripts/gen-synthetic-repo.sh test-repos/repos/large --commits 1000000 --branches 50
```

生成器通过 `git fast-import` 写入（而非逐次 `git commit`），并对输出流做 `tr -d '\r'`
归一化，确保在 Windows 上不会因 CRLF 导致解析失败。仓库结构为：

```
main      ─●─●─●───────●(merge)─●─●───────●(merge)─●─►
              \       /             \       /
bench-1        ●─●─●─┘   bench-2     ●─●─●─┘
```

即"分叉后立即合并"的旁支，同时覆盖长线性段（可折叠，FR-LOG-05）与合并气泡（泳道切换）。

## 拉取真实基准仓库（可选，体积大）

```bash
./scripts/fetch-bench-repos.sh
```

脚本使用 `--filter=blob:none` 部分克隆：只取提交与树对象、不取文件内容，
在保留完整历史规模的前提下把体积压到完整克隆的一小部分。

## 一致性测试的快照要求

用于 NFR-COR-01 的仓库必须**固定到具体提交**，避免上游推进导致基线漂移。
约定：`test-repos/snapshots.toml` 记录每个仓库的 pin 提交（该文件在 M0 建立
一致性测试套件时创建）。
