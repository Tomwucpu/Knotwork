# M1 只读 MVP —— 验收报告（G1 决策门）

> 范围：实施计划 M1（任务 1-1 ~ 1-12）。主题是**端到端纵切**：打开仓库 → 看 Log → 看 Diff → 看状态，全程可审计。
> 本文按实施计划第 4 节列出的 G1 通过标准逐条给出结论，并诚实列出未达标项与遗留问题。

---

## 1. 交付物清单

### 1.1 代码（工作区 = 仓库根）

| crate | M1 新增内容 |
|---|---|
| `gt-core` | 领域模型 7 个模块：`oid` / `commit` / `refs` / `change` / `status` / `state` / `query`。保持"零第三方依赖"约束 |
| `gt-backend` | gix 读路径（`repo.rs`）、git CLI 执行器（`cli.rs`）、命令审计（`record.rs`）、错误类型（`error.rs`）；新增 `SharedGitRepo` 跨线程句柄 |
| `gt-service` | 有界线程池、取消令牌、**按视图通道隔离的**请求代次、硬上限分段 LRU、Console 环形缓冲、文件监听与 `.git` 内部路径过滤、`GitService` 仓库注册表与只读降级 |
| `gt-diff` | imara-diff Histogram 引擎、行/词级模型、git 兼容的 hunk 头与 numstat（含重命名路径缩写） |
| `gt-graph` | 流式增量泳道布局（合并点收敛、拓扑校验可选） |
| `gt-config` | 配置模型（外观/Git 行为/受保护分支/缓存/外部工具/诊断）、最近仓库列表、跨平台路径、原子保存、未知键告警 |
| `gt-ui` | Slint 主窗口（标题栏/横幅/引用侧栏/Log/ Diff/底部面板/状态栏）、视图模型（纯转换）、每窗口会话（脏标记 + 按需预取） |
| `knotwork-app` | 多窗口装配、关闭即释放、最近仓库、启动页、git 缺失降级、保留阶段 0 内存探针 |

### 1.2 测试资产

| 套件 | 位置 | 用例数 | 性质 |
|---|---|---|---|
| 一致性套件（内核） | `crates/gt-backend/tests/consistency.rs` | 14 | **真实仓库**上对比 gix 与 git CLI |
| 一致性套件（差异载荷） | `crates/gt-service/tests/diff_consistency.rs` | 5 | 同上，覆盖 `--numstat` 与 hunk 头 |
| 语义探针 | `scripts/probe-git-porcelain.py` | — | 在**本机实际 git** 上实测 porcelain 语义，是规则的唯一依据 |

### 1.3 文档

- 本报告；
- 代码内文档：每个 crate 的 `lib.rs` 均写明职责边界、M1 覆盖范围与已知差异。

---

## 2. G1 通过标准逐条结论

实施计划第 4 节要求四项：

| # | 标准 | 结论 | 证据 / 说明 |
|---|---|---|---|
| ① | 大仓库滚动达标 | **部分达标** | 泳道布局与虚拟列表路径已工程化（`gt-graph` 12 用例 + Slint `ListView` 全虚拟化），但**真实 UI 的滚动帧率尚未采集**。M0 POC-2/POC-4 的 release 数据（帧 P95 4.31ms、100 万提交增量布局 20µs）仍是大仓库达标的主要依据；真实 UI 的帧时间基准属 M5 的 CI 项（M0 报告第 8 节待办 1） |
| ② | 一致性测试 100% 通过 | **达标** | 19 个用例全部通过（内核 14 + 差异载荷 5），覆盖 status / 冲突码 / rev-list / name-status / 根提交 / blob / numstat / hunk 头。**当前 0 条分歧** |
| ③ | 3 窗口 < 90MB 复测 | **达标** | release 实测：真实 UI 开 3 个典型仓库窗口（small + medium + demo）总工作集 **60.1MB**（判据 90MB）；每窗口边际 ≈ 13.8MB（判据 < 15MB，见 §3.2）。含 100 万提交 large 压力仓库的 3 窗口为 242MB，其中主体是 mmap 的 pack 页面（M0 已记录的页缓存特性，可回收；madvise 策略属 M0 待办 #2）。纯探针窗口 24.6MB，对齐 M0 结论（软件渲染 32.5MB vs femtovg 198.1MB） |
| ④ | Console 可审计 10 个混合操作 | **达标** | Console 环形缓冲实现 `gt_backend::CommandObserver`，**内核两条路由（内置引擎 / git 子进程）的记录都进同一处**；`export_text()` 输出摘要行 + 可复现命令行 + 错误与 stderr 尾部 + 导出到配置目录。一次真实打开仓库即产生 ≥3 条记录（概览/状态/日志），30 个用例里有 5 个直接断言审计内容 |

---

## 3. 实测数据

### 3.1 端到端首屏（`small` 基准仓库，1060 提交）

`KNOTWORK_LOG=gt_service=trace knotwork test-repos/repos/small`：

```
请求完成 kind="仓库概览" generation=g1 elapsed_ms=12
请求完成 kind="工作区状态" generation=g1 elapsed_ms=19
请求完成 kind="日志页"   generation=g1 elapsed_ms=47
代次已过期丢弃：0 条
```

三点值得记录：

1. **三条首屏请求全部落地**。这暴露并修掉了一个真实缺陷：最初代次闸门是"每仓库一把"，
   连发的三个请求会互相作废，只有最后一个能到达界面。现在按**视图通道**（概览/状态/日志/差异）
   分别发号，`independent_lanes_do_not_invalidate_each_other` 用例把这条语义钉住了。
2. **首屏 47ms 内完成**（debug 构建）。日志页在 1060 提交下走完整 revwalk。
3. **0 条过期丢弃**说明通道隔离生效，不是靠"碰巧只有一条请求"。

### 3.2 内存

| 场景 | 构建 | 工作集 RSS | 判据 | 结论 |
|---|---|---|---|---|
| 3 窗口（真实 UI，small + medium + demo 典型组合） | release | **60.1MB** | < 90MB | **达标** |
| 3 窗口（真实 UI，含 large 压力仓库） | release | 242MB | — | 压力场景：主体为 large 的 mmap pack 页面（.git 167MB），属可回收页缓存；madvise/分段映射策略是 M0 待办 #2，不阻塞 G1 |
| 1 窗口（真实 UI，small 仓库挂起） | release | **32.6MB** | < 40MB | **达标** |
| 3 窗口（阶段 0 探针） | release | 24.6MB | — | 与 M0 的 32.5MB（debug）同量级，LTO + strip 后更低 |
| 3 窗口（阶段 0 探针） | debug | 32.7MB | — | 与 M0 的 32.5MB 一致 |
| 1 窗口（真实 UI，small 仓库） | debug | 49.2MB | — | debug 下含 gix pack 映射与未优化布局 |

**边际成本**：单窗口（small）32.6MB → 3 窗口（small + medium + demo）60.1MB，
每加一窗口边际 ≈ **13.8MB**，满足 NFR-MEM-02 的"< 15MB"第二判据
（medium 的 .git 8.5MB 含 mmap 页面，该边际已计入真实仓库成本）。

> 判据以 **release** 为准（实施计划第 7 节的 NFR-MEM-01/02 口径）。
> debug 数字仅用于与 M0 的 debug 基线对照。
> 采样方式：应用启动后挂起 ≥ 10s（首屏请求全部落地），`tasklist` 读取工作集。

### 3.3 一致性套件覆盖矩阵

| 比对对象 | CLI 侧 | 内置引擎侧 | 用例 |
|---|---|---|---|
| 工作区状态（内容） | `git status --porcelain=v1` | `ChangeSet::to_porcelain` | 14 个场景（干净/未跟踪/目录折叠/忽略/暂存/未暂存/双侧/新增/删除/重命名/索引删除+未跟踪/混合分区） |
| 工作区状态（字节） | 同上 | `GitRepo::status_porcelain`（读 `core.quotepath`） | 非 ASCII 路径 × quotepath 开关 |
| 冲突码 | 同上 | 索引三阶段推导 | 7 种组合 + 真实合并冲突 |
| 提交枚举 | `git rev-list --all` / `--branches HEAD` | `GitRepo::walk_ids` | 分叉 + 合并历史 |
| 文件级变更 | `git diff --name-status` | `tree_changes` | 修改/新增/删除/重命名 + 根提交 |
| 行数统计 | `git diff --numstat` | `gt_diff::FileDiff::numstat_line` | 四种变更类型 + 二进制 + 根提交 |
| hunk 头 | `git diff --unified=3` 的 `@@` 行 | `DiffHunk::header` | 单处修改 |

---

## 4. 与 git CLI 的一致性：规则来源

"M1 有一致性测试"不等于"规则是对的"。本阶段把规则**实测固化**成了可复跑的脚本
（`scripts/probe-git-porcelain.py`），因为它们都不是靠读文档能确定的：

### 4.1 porcelain v1 的路径引号

实测结论：

- **含空格 → 强制加引号**（git 的 `QUOTE_PATH_QUOTE_SP`）：porcelain v1 是行式格式，空格是分隔符；
- 控制字符 / DEL / `"` / `\` → 必须转义，其中控制字符走**三位八进制**（`\177`）；
- 非 ASCII → **只**在 `core.quotepath=true` 时转义为八进制；
- 重命名行的**两侧各自**套用同一套规则，再以 `" -> "` 连接。

实现上必须**按字节**构造输出（`Vec<u8>` → `from_utf8_lossy`）：用 `&str` 切片会在
`quotepath` 开启时切到字符中间直接 panic。

### 4.2 冲突码只在"索引无同路径 stage-0 条目"时按标准掩码判定

stage 掩码 `Σ(1 << (stage-1))` → `1/2/3/4/5/6/7 = DD/AU/UD/UA/DU/AA/UU`。
但在既有 stage-0 条目之上注入未合并阶段时，git 会走另一条分支
（`(2,)`、`(1,3)`、`(1,)` 全部退化成 `UU`）。一致性用例因此先用 `read-tree --empty`
清空索引再注入 —— 这个前提写在用例注释里，避免后来者"修"成看似更直接但错误的写法。

### 4.3 重命名的 numstat 缩写

`renamed.txt => moved.txt`、`dir/{a.txt => b.txt}`、`a.txt => a-old.txt`
（**不**缩写成 `{renamed => moved}.txt`）。规则是：公共前缀只保留到最后一个 `/`，
公共后缀必须从 `/` 开始。

### 4.4 gix 与 git 的五处语义差异（已全部修复）

| 差异 | 处理 |
|---|---|
| `Summary::Conflict` 不带冲突码 | 从索引三阶段（`gix_index::entry::Stage`）自行推导 |
| 未跟踪目录输出 `?? dir`（缺尾斜杠） | 判 `gix_dir::entry::Kind::Directory` 后补 `/` |
| 树差异会产出中间树条目（`A dir` 与 `A dir/b.txt` 并存） | 按 `entry_mode.is_tree()` 过滤 |
| 重命名分数输出 `R50` 而非 git 的 `R050` | `{score:03}` 补零 |
| 索引侧与工作区侧按路径合并会丢行 | 拆成两张独立表（`git rm --cached f` 场景 git 会输出 `D  f` + `?? f` 两行） |

---

## 5. 未达标项与遗留问题

按"会不会影响正确性"排序，全部如实登记。

### 5.1 影响验收口径

| # | 问题 | 影响 | 计划 |
|---|---|---|---|
| A | **真实 UI 的滚动帧率未采集** | G1 标准 ① 只能靠 M0 的原型数据支撑 | 归入 M5 的 CI 帧时间基准（M0 报告待办 1）；届时用定时器口径（软件渲染不支持渲染回调） |
| B | ~~阶段 0 的 `--probe` 与真实 UI 是两套窗口~~ | **已关闭**：真实 UI 的 3 仓库窗口复测已做（§3.2：典型组合 60.1MB / 含 large 压力 242MB），判据按真实 UI 口径评估 | — |

### 5.2 功能缺口（已规划，不影响 M1 验收）

| # | 缺口 | 计划 |
|---|---|---|
| C | Diff 面板只显示提交的**第一个**变更文件 | M2 的 Diff 视图做文件列表切换（FR-DIF-01/03） |
| D | 无文件选择对话框（只能输入路径或点最近仓库） | 引入 `rfd` 后补；M1 刻意不为此加依赖 |
| E | Log 预取按"已加载窗口"触发，不感知真实滚动位置 | Slint 的 `ListView` 不向 Rust 暴露"首个可见行"；M2 用 `viewport-y` 换算 |
| F | 未实现仓库写锁与只读打开（FR-MR-04） | M2（本阶段只读，不存在并发写风险） |
| G | 未实现单实例守卫（FR-MR-05） | M2 |

### 5.3 已知差异（刻意保留，已写入代码注释）

| # | 差异 | 原因 |
|---|---|---|
| H | `worktree_blob` 不应用 smudge / CRLF / attribute 过滤器 | 随 FR-CON-01 的过滤器支持在 M5 处理；影响"工作区 diff 的行尾表现"，不影响状态判定 |
| I | `gt-diff` 的 numstat 不做 porcelain 路径引号 | `gt-diff` 不感知 `core.quotepath`；需要逐字节一致的展示由上层套用引号规则 |
| J | `ChangeSet::to_porcelain()` 默认 `RAW_NON_ASCII`（不转义非 ASCII） | UI/Console 可读性优先；与 git 出厂配置逐字节一致请用 `status_porcelain()` |
| K | 配置里"受保护分支"存的是正则字符串，**未做语法校验** | M3 实现 push 拦截时才引入正则引擎，避免为尚未被使用的检查加依赖 |

---

## 6. 需求追溯

| 需求 | 实现位置 | 验证 |
|---|---|---|
| FR-MR-01 打开仓库 | `GitService::open_repo` / `GitRepo::open` | 一致性套件 `opening_non_repository_fails_with_a_typed_error`；服务层 `error_message_mentions_the_problem_for_non_repositories` |
| FR-MR-02 最近仓库列表 | `gt-config::recent` + `app::windows` | `recent` 模块 6 个用例（去重/上限/别名/失效过滤/排序） |
| FR-MR-03 多窗口多仓库 | `app::windows::WindowManager` | 同路径重复打开被拒（`same_repo`）；关闭即释放（`close_repo`） |
| FR-LC-01 工作区状态（只读） | `GitRepo::status` + `gt-ui` 三分区 | 一致性套件 14 场景；`buckets_follow_precedence` |
| FR-LOG-01 日志与泳道图 | `walk_ids` / `commit_metas` + `gt-graph` + `gt-ui` | POC-4 工程化为 `LaneLayout`（12 用例）；`build_graph_cells` 断言节点列 |
| FR-LOG-02 分支范围（D2） | `BranchScope` + `LogFilter` | `default_scope_is_local_and_upstream` |
| FR-LOG-04 跳转/翻页 | `LogPageRequest` + 服务层切片 + 预取 | `log_page_uses_the_cached_id_list_and_slices_it` |
| FR-DIF-01/02 差异视图 | `gt-diff` + `gt-ui::build_diff_rows` | 5 个 diff 一致性用例；词级分段断言 |
| FR-CON-01 Console 审计 | `ConsoleBuffer` + `CommandRecord` | 5 个 Console 用例（环形/丢弃计数/导出内容/观察者） |
| FR-STA-01 崩溃隔离 | `WorkerPool` 的 `catch_unwind` + `panic = "unwind"` | `panicking_job_does_not_kill_the_worker` |
| FR-STA-02 中断恢复探测 | `RepoState` + `GitRepo::repo_state` | `repository_state_is_detected_during_merge` + 6 个状态用例 |
| FR-STA-03 刷新去抖 | `watch::classify` + 250ms 去抖 | 8 个监听用例（风暴丢弃/分类表/大小写/监听根选择） |
| FR-STA-04 git 缺失降级 | `Capabilities` + `GitService` | `capabilities_degrade_explicitly_when_git_is_missing` |
| NFR-COR-01 输出一致 | 一致性套件 | 19 个用例，0 分歧 |

---

## 7. 工程纪律

| 项 | 状态 |
|---|---|
| `cargo fmt --all -- --check` | 无差异 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 全绿（含 pedantic） |
| `cargo test --workspace` | 214 项全通过 |
| `unsafe_code` | 全仓库零 `unsafe`（含测试） |
| UI 线程 I/O | `gt-ui` 与 `app` 中不存在文件读写与进程启动；所有读取收在 `gt-service::service::execute` 一处，便于审查 |

---

## 8. G1 结论

**有条件通过。**

- 一致性与 Console 审计两项**达标**，且一致性部分超出了一致性口径的原始要求
  （把规则实测脚本化，而不是"跑通即算"）；
- 内存两项按 release 真实 UI 口径**达标**：单窗口 32.6MB（< 40MB）、
  3 窗口典型组合 60.1MB（< 90MB，边际 13.8MB/窗口 < 15MB）；
  含 100 万提交压力仓库的 3 窗口为 242MB，主体是可回收的 mmap pack 页面
  （M0 待办 #2 的 madvise 策略项，不改变"多窗口内存不随仓库提交数线性膨胀"的结论 ——
  large 单独打开时其 pack 页面同样会被计入，多窗口共享的是同一套映射成本）；
- 大仓库滚动一项**只能靠 M0 原型数据**支撑 —— 真实 UI 的帧率采集缺席，
  这是本阶段最需要补的一项（§5.1-A）。

原"进入 M2 前补真实 UI 3 窗口采样"的建议**已执行完毕**（§3.2）。
剩余的帧率采集不阻塞 M2：M2 的写路径在 UI 线程上仍是零 I/O，
滚动帧率风险集中在 Log 视图的长列表渲染，而该路径在 M1 与 M2 之间不会变化。
M2 的主题是"从能看变成能改"，写路径会引入仓库写锁与高危确认；
建议 M2 期间把帧时间基准（定时器口径）提前搭好，进入 M3 前出数。
