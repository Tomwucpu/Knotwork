# IDEA expui 图标素材

本目录的 SVG 取自本机 IntelliJ IDEA 2025.1.7.2 的 `expui` / `vcs` 资源，
文件头保留 JetBrains 的 Apache License 2.0 声明。选取这些素材是为了让
Knotwork 的工具栏图标与 IDEA Git 工具窗保持同一套视觉语言；项目只使用
`Image.colorize` 做单色着色，不修改图标形状。

来源归档：`<IDEA>/lib/app-client.jar`。

| 用途 | 文件 |
| --- | --- |
| 刷新 | `refresh.svg` |
| 新增 / 暂存 | `add.svg` |
| 移除 / 反暂存 | `remove.svg` |
| 查看差异 | `eye.svg` |
| 折叠 / 展开 | `collapse-all.svg`、`expand-all.svg` |
| 关闭 / 更多 | `close.svg`、`more.svg` |
| 分组箭头 | `chevron-right.svg`、`chevron-down.svg` |
| 搜索 / 过滤 / 设置 | `search.svg`、`filter.svg`、`settings.svg` |
| 分支 / 标签 / 目录 / 文件 | `branch.svg`、`tag.svg`、`folder.svg`、`file.svg` |
| 模块 / 勾选 | `module.svg`、`check.svg` |
| 菜单角标（直角三角形） | `menu-corner.svg`（本项目自绘） |
| 提交 / Fetch / Push / Update | `commit.svg`、`fetch.svg`、`push.svg`、`update.svg` |
| 导出 / 历史 / 回退 | `download.svg`、`history.svg`、`revert.svg` |
| 更改 / 撤销 / 重做 | `changes.svg`、`undo.svg`、`redo.svg` |

Apache License 2.0：<https://www.apache.org/licenses/LICENSE-2.0>

## 文件类型图标（filetypes/）

文件树与提交详情的文件行使用 expui 的彩色文件类型图标，来源为
[JetBrains/intellij-community](https://github.com/JetBrains/intellij-community)
的 `platform/icons/src/expui/fileTypes`（Apache License 2.0）。每个图标都保留了
原始文件头声明；同时下载了 `_dark` 变体，由 `FileTypeIcon` 按
`Palette.dark` 自动切换。

`rust.svg` 来自 [intellij-rust](https://github.com/intellij-rust/intellij-rust)
的 `src/main/resources/icons/rustFile.svg`（MIT License，Copyright (c)
2015 Aleksey Kladov, Evgeny Kurbatsky, Alexey Kudinkin and contributors；
Copyright (c) 2016 JetBrains）。该图标没有暗色变体。

`vue.svg`、`gitignore.svg`、`rust.svg` 没有 `_dark` 素材，两套主题共用同一个文件。
