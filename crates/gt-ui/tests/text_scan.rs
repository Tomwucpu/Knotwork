//! 硬编码文案扫描（NFR-L10N-01，M5）。
//!
//! 规则：**界面文案必须经过资源层**——
//! - Slint 侧：`ui/*.slint` 中不允许出现中文文案字面量，
//!   唯一例外是 `texts.slint`（双语资源表本体）；
//! - Rust 侧：`session.rs` / `view_model.rs` 的非测试代码不允许出现
//!   中文文案字面量，必须走 `text::tr` / `text::trf`
//!   （资源表在 `gt-ui/src/text.rs`）。
//!
//! 扫描按行进行：跳过注释行与 `#[cfg(test)]` 模块；
//! 字符串字面量含 CJK 即视为违规（语言名"中文"这类专有名词也放进
//! texts.slint，不留例外，避免规则出现第二 explanatory）。

use std::io::Read as _;
use std::path::{Path, PathBuf};

// 粗判即可：任何字符落在 CJK 统一表意区即算中文。
fn contains_cjk(s: &str) -> bool {
    s.chars()
        .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c) || ('\u{3400}'..='\u{4dbf}').contains(&c))
}

/// 提取一行里的字符串字面量（内容 + 起始字节偏移）。
fn literals_with_positions(line: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut chars = line.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if ch != '"' {
            continue;
        }
        let mut lit = String::new();
        let mut escaped = false;
        for (_, c) in chars.by_ref() {
            if escaped {
                lit.push(c);
                escaped = false;
                continue;
            }
            match c {
                '\\' => escaped = true,
                '"' => {
                    out.push((idx, lit));
                    break;
                }
                _ => lit.push(c),
            }
        }
    }
    out
}

/// 兼容别名：仅取内容。
fn literals_on_line(line: &str) -> Vec<String> {
    literals_with_positions(line)
        .into_iter()
        .map(|(_, lit)| lit)
        .collect()
}

/// 判断一个字面量是否是资源键（紧跟在 `text::tr(` / `text::trf(` 之后）。
fn is_resource_key(line: &str, literal_start: usize) -> bool {
    let before = &line[..literal_start];
    let trimmed = before.trim_end();
    trimmed.ends_with("text::tr(") || trimmed.ends_with("text::trf(")
}

/// `rustfmt` 会把过长的资源键换到下一行；上一行以 `text::tr(` 结尾时同样视为资源键。
fn is_wrapped_resource_key(previous_line: &str) -> bool {
    let trimmed = previous_line.trim_end();
    trimmed.ends_with("text::tr(") || trimmed.ends_with("text::trf(")
}

fn read(path: &Path) -> String {
    let mut s = String::new();
    std::fs::File::open(path)
        .expect("打开被扫描文件")
        .read_to_string(&mut s)
        .expect("读取被扫描文件");
    s
}

fn ui_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("ui")
}

#[test]
fn slint_ui_has_no_hardcoded_chinese_outside_texts() {
    let dir = ui_dir();
    let mut violations = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("列出 ui 目录")
        .map(|entry| entry.expect("读取目录项").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "slint"))
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .expect("文件名")
            .to_string_lossy()
            .to_string();
        if name == "texts.slint" {
            continue; // 双语资源表本体
        }
        for (lineno, line) in read(&path).lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for lit in literals_on_line(line) {
                if contains_cjk(&lit) {
                    violations.push(format!("{name}:{} \"{lit}\"", lineno + 1));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "Slint 界面存在硬编码中文文案（应放入 texts.slint 并引用 Texts.*）：\n{}",
        violations.join("\n")
    );
}

#[test]
fn rust_ui_code_has_no_hardcoded_chinese_outside_text_module() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    for name in ["session.rs", "view_model.rs"] {
        let path = src.join(name);
        let source = read(&path);
        let lines: Vec<&str> = source.lines().collect();
        let mut in_test = false;
        for (lineno, line) in lines.iter().enumerate() {
            if line.contains("#[cfg(test)]") {
                in_test = true;
            }
            if in_test || line.trim_start().starts_with("//") {
                continue;
            }
            let previous = lineno
                .checked_sub(1)
                .and_then(|index| lines.get(index))
                .copied()
                .unwrap_or_default();
            for (start, lit) in literals_with_positions(line) {
                if contains_cjk(&lit)
                    && !is_resource_key(line, start)
                    && !is_wrapped_resource_key(previous)
                {
                    violations.push(format!("{name}:{} \"{lit}\"", lineno + 1));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "Rust 侧存在硬编码中文文案（应走 text::tr / text::trf）：\n{}",
        violations.join("\n")
    );
}
