# UI 交互修复实施计划

> 目标：修复图标对齐、恢复可拖拽的提交栏/底部 Git 栏、补齐悬停提示，并恢复或清理无效按钮。
>
> 已确认方案：顶部仓库标题栏、单文件右键菜单、布局尺寸全局持久化。

## 1. 图标对齐

### 根因

`HorizontalLayout` 的交叉轴默认不是居中；`alignment: center` 只作用于主轴，不能解决图标与文字的上下错位。`rows.slint` 的 10 个横向布局中有 0 个设置交叉轴居中，`main-window.slint` 的 29 个中只有 2 个。

### 改动

- 所有“图标 + 文字”或“徽标 + 文字”的 `HorizontalLayout` 增加：
  ```slint
  cross-axis-alignment: center;
  ```
- 重点位置：
  - `crates/gt-ui/ui/rows.slint`：`LogRowView`、`ChangeGroupRowView`、`ChangeRowView`、`CommitFileRowView`
  - `crates/gt-ui/ui/main-window.slint`：提交工具栏、分支树行、底部标签栏、日志工具栏、状态栏、标题栏
  - `crates/gt-ui/ui/widgets.slint`：`IdeaRefBadge`、`ToolWindowHeader`、`FilterButton`、`IdeaCheckbox`
- 统一图标尺寸：工具栏 16px、分组箭头 12px、分支/文件行 14px、引用徽标 12px。
- 不整体替换 SVG；只在布局修复后仍存在 1px 光学偏差时校正个别素材。

### 验收

提交列表、分支树、引用徽标、工具栏中，图标中心与文字中心偏差不超过 1px；100%/125%/150% DPI 均通过。

## 2. 恢复顶部仓库标题栏

在 `main-window.slint` 根 `VerticalLayout` 顶部插入 40px 标题栏，放在状态横幅之前。

### 内容

- 左：仓库标题 + 路径，路径超长省略。
- 中：当前分支标签、HEAD、当前远端；窄窗口隐藏 HEAD/远端文本。
- 右：`Fetch`、`Fetch All`、`Update`、`Push`、`Remotes`、`Settings`、`Refresh`。
- 未打开仓库时：只显示 `Knotwork` 标题与设置入口，分支/远端区域隐藏。
- 远端按钮复用 `fetch-selected` / `fetch-all` / `update-project` / `push-request` / `open-remotes`。
- 启用条件：`has-repo && !cli-missing && !network-busy`；Update/Push 额外要求 `!read-only`。
- 分支标签、HEAD、远端文本可省略，避免 900px 最小窗口下挤压按钮。

### 重复入口清理

- 删除提交工具栏的设置齿轮。
- 删除底部 Git 标签栏的设置和刷新。
- 保留顶部 `Refresh` 作为唯一全局刷新；提交栏只保留暂存/反暂存/丢弃。

## 3. 双面板拖拽

### `SplitterHandle`

在 `widgets.slint` 新增可复用组件：

- `orientation: 0` 垂直分隔，`1` 水平分隔。
- `in-out property <length> size`，由调用方双向绑定。
- `min-size` / `max-size`。
- 5px 热区，中心 1px 分隔线；悬停/拖拽时使用 `Palette.accent-strong`。
- 鼠标样式 `col-resize` / `row-resize`。
- 按下记录绝对指针位置，移动时计算 delta，松手触发 `resized()`。
- 使用 `TouchArea.absolute-position + mouse-x/y`，避免分隔条自身移动导致 delta 计算错误。

### 左侧提交栏

- 替换 `main-window.slint` 中提交工具窗与差异区之间的 `VDivider`。
- 宽度默认 380px，最小 280px。
- 最大宽度：
  ```text
  viewport-width - (viewport-width >= 1080 ? 340 : 0) - 365
  ```
- 收起时隐藏拖拽柄，但保留上次宽度。

### 底部 Git 栏

- 在 Git 工具窗上方插入水平拖拽柄。
- 高度默认 300px，最小 220px。
- 最大高度：
  ```text
  viewport-height - 350
  ```
- 内部改为真正可伸缩：
  - 提交文件列表：删除 `viewport-height - 573` 魔法公式，改 `vertical-stretch: 1`。
  - 分支树列表：删除固定 116px，改 `vertical-stretch: 1; min-height: 40px`。
  - 日志列表：删除固定 235px，改 `vertical-stretch: 1`。
  - 提交详情文件列表：由固定 88px 改为 `vertical-stretch: 1; min-height: 40px`。

## 4. 布局尺寸持久化

### 配置模型

在 `crates/gt-config/src/model.rs` 新增：

```rust
pub struct LayoutConfig {
    pub commit_panel_width: u16, // 默认 380
    pub git_panel_height: u16,   // 默认 300
}
```

- 加入 `UserConfig` 的 `layout` 字段。
- 在 `crates/gt-config/src/lib.rs` 导出。
- 旧配置没有 `[layout]` 时自动使用默认值。
- 读取时钳制到合理范围。

### 窗口接线

- `main-window.slint` 新增：
  - `in-out property <length> commit-panel-width: 380px;`
  - `in-out property <length> git-panel-height: 300px;`
  - `callback layout-changed();`
- `app/src/windows.rs` 的 `install()` 写入配置尺寸。
- 拖拽松手触发 `layout-changed()`，Rust 侧更新共享配置并调用 `persist_shared` 落盘；失败只记日志。
- 新增配置测试：默认值、TOML 往返、旧配置缺少 `[layout]`。

## 5. 悬停提示

### 实现方式

- `IdeaButton`、`IdeaIconButton` 增加 `in property <string> tooltip: "";`。
- 在组件根节点直接放 `Tooltip`；Slint 不允许继承 `Tooltip`，也不能把 `Tooltip` 放在布局里。
- 使用自定义内容，套 `Palette` 的 tooltip 背景/边框/文字。
- 内置延迟约 500ms。

### 覆盖范围

- 提交工具栏、日志工具栏、底部标签栏。
- 分组行右侧 +/-。
- 分支树标签行删除按钮。
- 顶部远端按钮、分支按钮。
- 禁用按钮保留提示，说明前置条件。

### 文案

- 复用现有 `s166`–`s171`、`s35`–`s37`、`s51`–`s56`、`s23`、`s26`、`s27`。
- 补充缺失的“获取/全部获取/更新/推送/删除标签/展开提交面板”等 ID。
- 全部放入 `texts.slint`；Rust 动态提示走 `gt_ui::text::tr`。

## 6. 单文件右键菜单与遗留回调清理

### 右键菜单

- `ChangeRowView` 增加 `context-clicked(length, length)`，沿用 `LogRowView` 的绝对坐标方式。
- 在 `dialogs.slint` 新增 `ChangeCtxMenu`。
- 菜单项：
  - 暂存 / 反暂存（按 `group` 只显示适用项）
  - 丢弃更改
  - 删除未跟踪文件
  - 添加到 `.gitignore`
  - 添加到 `exclude`
  - Blame
  - 文件历史
- 启用条件：`read-only` 全禁用；删除/忽略仅 `is-untracked`；暂存仅未暂存组；反暂存仅已暂存组。
- 复用回调：`stage-selected`、`unstage-selected`、`discard-selected`、`delete-selected`、`ignore-selected`、`request-blame-selected`、`request-file-history-selected`。

### 恢复标签删除

- 分支树标签行悬停时显示小垃圾桶图标，tooltip “删除标签”。
- 点击时先 `select-ref(entry.ref-index)`，再 `delete-tag()`。

### 清理遗留回调

- 保留并恢复：`fetch-*`、`update-project`、`push-request`、`open-remotes`、`stage-selected`、`unstage-selected`、`delete-selected`、`ignore-selected`、`request-blame-selected`、`request-file-history-selected`、`delete-tag`。
- 删除：
  - `open-repo`：启动页只通过 `browse-repo` / `use-recent` 打开。
  - `set-commit-message`：提交消息已双向绑定。
  - `checkout-selected-commit`：提交右键菜单的 `checkout` 已覆盖。
- 确保 `every_ui_callback_has_a_handler` 测试继续通过，且不再有声明了但无入口的回调。

## 7. 验收清单

- 图标：主要列表/工具栏全部垂直居中，深色/浅色均无错位。
- 左侧：可拖到 280px 至可用最大值；收起后展开恢复宽度；重启后保留。
- 底部：可拖到 220px 至窗口允许最大值；内部列表随高度伸缩；重启后保留。
- 标题栏：900px 宽度下不溢出；Fetch/Update/Push/Remotes 均可用。
- 提示：所有纯图标按钮悬停约 0.5 秒出现短提示；中英文切换正常。
- 右键：暂存/反暂存/丢弃/删除/忽略/Blame/历史均走现有 Rust handler。
- 清理：无重复设置/刷新按钮；无无入口的旧回调。

### 命令

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
python scripts/i18n_sweep.py --check
cargo build -p knotwork-app
```

## 8. 实施顺序

1. 图标交叉轴对齐 + 尺寸统一。
2. 顶部标题栏 + 远端操作恢复 + 重复按钮清理。
3. `SplitterHandle` + 双面板拖拽 + 内部列表伸缩。
4. `LayoutConfig` + 拖拽松手持久化。
5. Tooltip 组件与全量文案。
6. `ChangeCtxMenu` + 标签删除 + 遗留回调清理。
7. 全量测试与多尺寸截图回归。

## 9. 实施状态（2026-10-20）

- [x] 图标交叉轴对齐与尺寸统一。
- [x] 顶部仓库标题栏、远端操作、重复按钮清理。
- [x] `SplitterHandle`、双面板拖拽、内部列表伸缩。
- [x] `LayoutConfig` 持久化与加载钳制。
- [x] Tooltip 组件与主要图标按钮文案。
- [x] `ChangeCtxMenu`、标签删除、遗留回调清理。
- [x] `cargo check`、`cargo test -p gt-ui -p gt-config -p knotwork-app`、`cargo clippy -p gt-ui -p gt-config -p knotwork-app --all-targets -- -D warnings`、`python scripts/i18n_sweep.py --check`。
- [x] 多尺寸截图回归与真机交互验证（拖拽释放、右键菜单定位、Tooltip 延迟）。
- 验证要点：
  - Tooltip 必须放在按钮 `TouchArea` 之后；放在前面会被触摸区拦截 hover 事件，导致永不显示。
  - 截图回归脚本必须先调用 `SetProcessDpiAwarenessContext(-4)`，否则 `GetWindowRect` / `CopyFromScreen` 会因 150% DPI 截断窗口右侧内容，误判为顶部按钮被挤掉。
