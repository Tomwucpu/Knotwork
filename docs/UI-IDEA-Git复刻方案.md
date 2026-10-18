# UI IDEA Git 双工具窗复刻方案

> 版本：v1.0（P1–P4 已实施）
>
> 编制日期：2026-10-18
>
> 参考基线：`E:\idea\IntelliJ IDEA 2025.1.7.2`（New UI，Dark / Light）
>
> 关联文档：
>
> - 《UI样式重构规划.md》（S1–S4 视觉地基）
> - 《IDEA-Git功能调研与架构匹配分析.md》（功能与观感来源）
> - 《轻量级Git工具-功能需求设计文档.md》（FR-LOG / FR-STA 验收口径）

---

## 1. 目标与范围

把主窗口从"功能齐全的单列布局"改成 IDEA 2025 New UI 中 Git 相关工具窗的形态：

1. **提交工具窗**（左上，固定宽度）与**差异编辑器**（右上）并排；
2. **Git 日志工具窗**（底部通栏）包含 Git / Console / Stash 三个标签；
3. Git 标签内为"分支树 + 提交表 + 提交详情"三栏；
4. 保留既有全部能力（搜索 / 正则 / 图选项 / 分支作用域），不新增用户 / 日期 / 路径筛选。

参考截图对应 IDEA 的 **Commit** 与 **Git** 两个工具窗。复刻目标是"形似"：分组、工具条、按钮形态、行高、选中色与引用徽标与 IDEA 对齐，不做逐像素对照。

---

## 2. 参考基线提取

### 2.1 色值

色值不靠肉眼取色，直接读取本机 IDEA 安装目录中的主题定义：

- `lib/app-client.jar` → `themes/expUI/expUI_dark.theme.json`
- `lib/app-client.jar` → `themes/expUI/expUI_light.theme.json`

提取结果集中写入 `crates/gt-ui/ui/theme.slint`，本轮新增/确认的关键 token：

| 语义 | Dark | Light | IDEA 来源 |
| --- | --- | --- | --- |
| `bg` | `#1E1F22` | `#F7F8FA` | Gray1 / Gray13 |
| `panel` | `#2B2D30` | `#FFFFFF` | Gray2 / Gray14 |
| `line` | `#393B40` | `#EBECF0` | Gray3 / Gray12 |
| `text` | `#DFE1E5` | `#1E1F22` | Gray12 / Gray1 |
| `text-dim` | `#B4B8BF` | `#5A5D6B` | Gray11 / Gray5 |
| `text-faint` | `#9DA0A8` | `#6C707E` | Gray9 / Gray6 |
| `selection` | `#2E436E` | `#D4E2FF` | Blue2 |
| `log-branch-bg` | `#283044` | `#EDF3FF` | VCS Log 当前分支带 |
| `accent-strong` | `#3574F0` | `#3574F0` | Blue6 / Blue4 |
| `ref-head` | `#F5D273` | `#FFAF0F` | Yellow8 / Yellow4 |
| `ref-local` | `#5FAD65` | `#369650` | Green7 / Green5 |
| `ref-remote` | `#B589EC` | `#834DF0` | Purple9 / Purple4 |

### 2.2 尺寸

| 项 | 值 | 依据 |
| --- | --- | --- |
| 列表 / 提交行高 | 24px | IDEA 列表行高 |
| 工具窗标题 | 32px | ToolWindowHeader |
| 工具条 | 32px | 工具栏 |
| 标签栏 | 32px | 编辑器 / 工具窗标签 |
| 图标 | 16px（热区 26px） | New UI 工具条 |
| 按钮 | 28px，最小宽 64px，圆角 6px | ActionButton |
| 提交面板宽 | 380px（<1100px 窗口时 320px） | IDEA 默认宽度 |
| 提交详情宽 | 340px（<1080px 隐藏） | VCS Log 详情栏 |
| 分支树宽 | 228px（<1060px 时 190px） | VCS Log 分支栏 |
| 提交消息区高 | 110px | 提交工具窗 |
| Git 工具窗高 | 300px | 底部通栏 |

### 2.3 图标

从 `lib/app-client.jar` 提取 IDEA expui SVG（Apache License 2.0），放置于 `crates/gt-ui/ui/icons/`，并在 `icons/README.md` 记录来源与许可。`icons.slint` 暴露 `IdeaIcon`，按 `Palette` token 着色：

- `refresh` / `add` / `remove` / `revert` / `settings`
- `chevron-right` / `chevron-down` / `search` / `close`
- `branch` / `tag` / `file` / `commit` 等

---

## 3. 布局结构

```
┌───────────────────────────┬────────────────────────────────────────────┐
│ 提交工具窗（380）           │ 差异编辑器                                  │
│ 标题 / 工具条               │ 标题：<短哈希> — <提交主题> / <文件>          │
│ 已暂存 / 未暂存（分组）       │ 差异行 / 空状态 / 图片对比 / Blame           │
│ 提交消息（110）             │                                            │
│ 修订 / 保留作者              │                                            │
│ 提交 | 提交并推送 | 设置      │                                            │
├───────────────────────────┴────────────────────────────────────────────┤
│ Git | Console | Stash                                    刷新 / 设置     │
│ ┌────────────┬──────────────────────────────────┬───────────────────┐  │
│ │ 分支与标签   │ 搜索框 + 图选项           │ 提交详情（340）     │  │
│ │ HEAD        │ 提交表（泳道 / refs / 作者 / 时间）│ 元数据 + 文件清单   │  │
│ │ 本地 / 远程  │                                  │                   │  │
│ │ 标签        │                                  │                   │  │
│ │ 分支操作     │                                  │                   │  │
│ └────────────┴──────────────────────────────────┴───────────────────┘  │
└────────────────────────────────────────────────────────────────────────┘
```

### 3.1 响应式规则

窗口默认 1180×680，不做强制最小尺寸（Slint `Window` 不允许同时声明 `width` 与 `min-width`）。窄窗口按宽度降级：

| 窗口宽度 | 行为 |
| --- | --- |
| ≥ 1100 | 提交面板 380px |
| < 1100 | 提交面板 320px |
| ≥ 1080 | 显示提交详情栏 |
| < 1080 | 隐藏提交详情栏，提交表占满右侧 |
| < 1060 | 分支栏 190px，提交表作者列隐藏（<520px 行宽时 refs 列收窄为 96px） |

已在 1024×700 初始尺寸下截图验证：提交、差异、分支树、提交表均可用；详情栏按规则隐藏。

---

## 4. 数据契约变更（均为增量）

`ui/model.slint` 中的新增字段经 `include_modules!` 进入 Rust，保持只增不改：

| struct | 新增字段 | 用途 |
| --- | --- | --- |
| `ChangeRow` | `row-index` / `kind` / `group` / `count` / `collapsed` | 分组头与文件行共用同一个虚拟化列表 |
| `RefEntry` | `row-kind` / `ref-index` / `group` / `count` / `collapsed` | 分支树分组；`ref-index` 指回 `ordered_refs` |
| `RefBadge` | 新增 struct（`name` / `kind`） | 提交行引用徽标 |
| `LogRow` | `refs` / `refs-more` / `on-current-branch` | 徽标、`+N` 折叠、当前分支底色 |
| `CommitDetail` | 新增 struct | 提交元数据 + 文件清单 |

列表仍只有一个 `ListView`（分组头 + 行），虚拟化不受影响；提交详情文件列表同样复用 `CompareRow`。

### 4.1 后端请求

- 新增 `RequestKind::CommitFiles { old, commit }`，走 Log 通道（代际门控）；
- `gt-service` 复用 `repo.tree_changes(old, commit)`，返回 `Outcome::CommitFiles`；
- 点击提交 → 请求父提交与提交之间的文件列表；
- 点击详情文件 → 以"父提交 vs 提交"为两侧请求文件差异，复用既有 `FileDiff` 通道。

### 4.2 交互

| 交互 | 行为 |
| --- | --- |
| 提交工具窗分组标题 | 折叠 / 展开；右侧 `-` / `+` 执行全部反暂存 / 全部暂存 |
| 提交 / 提交并推送 | 复用既有提交写入；成功后按标志继续推送 |
| 分支组标题 | 折叠 / 展开 HEAD / 本地 / 远程 / 标签 / 其他 |
| 分支行 | 选中并回填新建分支输入框 |
| 提交行 | 选中、加载提交详情与文件列表；右键打开提交上下文菜单 |
| 详情文件行 | 选中并在主差异区打开该文件的历史差异 |
| 日志行 | 当前分支可达提交显示浅蓝分支带；选中态使用更深的蓝色 |

---

## 5. 实施记录（文件）

| 文件 | 内容 |
| --- | --- |
| `crates/gt-ui/ui/theme.slint` | 双主题 token、尺寸阶梯、引用颜色、日志分支底色 |
| `crates/gt-ui/ui/icons.slint` + `ui/icons/` | IDEA expui 图标与着色组件 |
| `crates/gt-ui/ui/widgets.slint` | IdeaButton / IdeaIconButton / ToolbarSeparator / ToolWindowHeader / IdeaSearchField / IdeaCheckbox / IdeaRefBadge 等 |
| `crates/gt-ui/ui/rows.slint` | 提交行、分组行、变更行、详情文件行、refs 徽标列 |
| `crates/gt-ui/ui/main-window.slint` | 双工具窗布局、提交面板、分支树、提交详情、响应式规则 |
| `crates/gt-ui/ui/texts.slint` | 新增中英文文案（含 HEAD 分组、提交详情、`+N` 场景） |
| `crates/gt-ui/src/view_model.rs` | 分组行、refs 徽标/`+N`、当前分支可达集、提交详情、单元测试 |
| `crates/gt-ui/src/session.rs` | 分组折叠、提交详情状态、CommitFiles 处理、文件差异请求 |
| `crates/gt-service/src/task.rs` / `service.rs` | `CommitFiles` 请求与结果 |
| `app/src/windows.rs` | 新回调接线（stage-all / unstage-all / toggle-group / commit-and-push / select-commit-file / toggle-ref-group） |
| `crates/gt-ui/tests/text_scan.rs` | 支持 rustfmt 换行后的 `text::tr(` 资源键识别 |

---

## 6. 验证

本轮完成的验证（2026-10-18）：

- `cargo build -p knotwork-app`：通过；
- `cargo test --workspace`：全部通过（gt-ui 40 项 + text_scan 2 项 + 其他 crate）；
- `cargo clippy --workspace --all-targets -- -D warnings`：通过；
- `python scripts/i18n_sweep.py --check`：OK，无游离中文；
- 运行态冒烟：
  - 提交行选中 → 详情元数据、父提交、文件清单加载；
  - 详情文件点击 → 主差异区显示 `父提交 vs 提交` 差异（README.md：`+1 -0`）；
  - 分支组折叠 / 展开；点击 `feature` 选中并回填输入框；
  - 窄窗口（1024×700）：详情栏自动隐藏、提交表可用；
  - 浅色主题（强制 Light 构建）：表面、文本、选中、引用徽标均可读；
  - 长差异（120 行）：列表在差异区内滚动，不覆盖 Git 工具窗。

截图保存在本地 `.workbuddy/tmp-ui/`（`p3`–`p27` 系列，不纳入仓库）。

---

## 7. 已知限制与后续

1. **提交详情栏在窄窗口隐藏**：IDEA 允许拖拽分栏；本工具尚未提供分隔条，窄窗口下需放宽窗口才能查看详情。
2. **提交/分支列表高度按默认骨架计算**：Git 工具窗固定 300px，列表高度为常量；后续若支持工具窗拖拽改变高度，需改为布局拉伸并联动。
3. **提交详情文件列表高 88px**：默认窗口下可见 3 个文件，更多文件需滚动。
4. **历史差异的逐块操作按钮仍显示为禁用**：IDEA 会在历史差异中隐藏暂存按钮；本轮只做禁用，未做隐藏。
5. **FR-LOG-02 的筛选未纳入本轮**：用户 / 日期 / 路径筛选按既定范围留待后续。
6. **程序化改变窗口尺寸在测试环境偶发 winit 0 尺寸崩溃**：未在正常手动缩放中复现，记录为环境风险；后续可升级 winit/Slint 或增加窗口尺寸守卫。
