# pocs —— M0 技术验证工程

本目录是 **M0 阶段 4 项 POC 的独立验证工程**，刻意不并入主 workspace
（`Cargo.toml` 中以 `exclude = ["pocs"]` 隔离），以免一次性验证代码污染
`gt-*` 的依赖图与 lint 约束。它们共用主仓库的 `target/` 以复用已编译的
Slint / gix 产物。

验证结论与实测数据见 `docs/M0-技术验证报告.md`。

## 目录

| 目录 | 对应 POC | 验证目标 |
|---|---|---|
| `poc-1-memory` | POC-1 | Slint 软件渲染多窗口内存（NFR-MEM-01/02） |
| `poc-2-scroll` | POC-2 | gix 大仓库虚拟化滚动帧时间（NFR-PRF-02、NFR-MEM-03） |
| `poc-3-consistency` | POC-3 | gix 与 git CLI 输出一致性（NFR-COR-01） |
| `poc-4-graph` | POC-4 | 泳道图增量布局算法（FR-LOG-05） |

## 构建与运行

所有命令在 `pocs/` 目录下执行，并通过 `CARGO_TARGET_DIR` 复用主仓库
`target/`（避免重复编译 Slint 与 gix）：

```bash
cd pocs
export CARGO_TARGET_DIR=../target
cargo build --release --workspace
```

### POC-1 多窗口内存

```bash
../target/release/poc1-memory --windows 3 --rows 10000 --hold 45
# 另开终端采样 RSS：
#   powershell -c "(Get-Process poc1-memory | Measure-Object WorkingSet64 -Sum).Sum/1MB"
../target/release/poc1-memory --gpu      # 切换 femtovg 做 A/B
```

参数：`--windows N`、`--rows M`（窗口内虚拟列表行数）、`--hold SECS`（自动退出秒数）。

### POC-2 虚拟化滚动

```bash
../target/release/poc2-scroll ../test-repos/repos/large --hold 40
```

输出加载耗时、帧间隔 P50/P95/P99、超 33ms 帧占比与行解析平均延迟。

### POC-3 一致性对比

```bash
../target/release/poc3-consistency                 # 默认扫描 ../test-repos/repos 的 small + medium
../target/release/poc3-consistency <仓库根目录>
../target/release/poc3-consistency --include-large  # 额外纳入 large（100 万提交，按需）
```

对 A（rev-list）/B（提交元数据）/C（status）/D（树差异）逐项比对并打印 PASS/FAIL。

**默认只跑 `small` + `medium`。** `large` 与中小两档走完全相同的代码路径，
规模不带来额外分支覆盖，只带来等待时间与磁盘压力，因此退化为按需的规模验证：
只有需要确认"百万提交仓库下依然一致"时才加 `--include-large`。

### POC-4 泳道布局

```bash
../target/release/poc4-graph 1000000
```

输出全量布局耗时、折叠/展开增量重排耗时、IntelliSort 局部切换耗时与非拓扑序边界演示。

## 说明

- POC-2 依赖 `test-repos/repos/large`（约 100 万提交），需先由
  `scripts/gen-synthetic-repo.sh` 生成；这是唯一必须用 large 的探针（滚动帧时间的
  意义就在规模），属手动执行的规模验证，不进入日常回归。
- **日常与回归验证只需 `small` + `medium`。** `large` 不必生成、也不必保留 ——
  删掉后 POC-3 会自动跳过，主 workspace 的 `cargo test` 完全不受影响（一致性套件
  自建临时仓库）。
- POC-3 会在系统临时目录创建 fixture 仓库用于工作区状态对比。
- 这些工程仅用于 M0 验证；POC-3 的对比脚本与 POC-4 的算法将按 M1 任务
  （1-11 / 1-4）工程化进 `gt-backend` 与 `gt-graph`。
