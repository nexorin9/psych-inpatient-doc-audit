//! **错误码表 ↔ 代码双向钉死**：防文档漂移的那道测试。
//!
//! 错误码表最大的失败模式不是写错，而是**过期**：代码里加了 `E_FHIR_WHATEVER`，
//! 表里没加，值班的人拿着这张表判断「这个码不在我们这儿」，
//! 于是去查错方向。反过来表里多了一条代码已不再返回的码，同样是误导。
//!
//! 两个方向都要钉：
//!
//! - **正向**：源码里出现的每一个 `E_XXX` 字面量，表里都有一行；
//! - **反向**：表里每一行注明的 `origin` 文件里，真的能扫到同一个码。
//!
//! 扫描范围是 `core/crates/*/src/`（不含 `tests/`）：测试里出现的码是
//! 「断言用的期望值」，不构成产品契约的一部分——把测试期望也纳入扫描
//! 会让「改一条测试断言」平白多一张表的维护负担。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use psychdoc_model::errcodes::{self, ERROR_CODES};

/// 工作区 `core/crates` 目录（本 crate 在 `core/crates/psychdoc-store`，上一层）。
fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .expect("找不到 core/crates")
        .to_path_buf()
}

/// 递归收 `src/` 下所有 `.rs` 文件（**不含** `tests/`）。
fn src_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            src_files(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

/// 扫出一个文件里出现的所有 `E_XXX` 形态码。
///
/// 只认**双引号字面量**，且只认字面量的**首段**是码：
/// `CliError::new("E_X", ..)`、`ApiError::new("E_X", ..)`，
/// 以及 `#[error("E_X: 中文…")]`——thiserror 把码写进 `Display` 首段，
/// `CliError::from_display` 再从首段把码摘回来，这是产品既有的约定。
///
/// 注释里的行内代码 `` `E_X` `` 不算：把它算进来会让「文档里提了一句」
/// 变成「这个码存在」，方向正好反了。前缀通配（`"E_REGRESS_*"`）也不算。
/// [`PROBE_CODES`] 里那几个是这张表自己的单测探针，同样不算。
fn codes_in(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != '"' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        while j < bytes.len() && bytes[j] != '"' && bytes[j] != '\n' {
            j += 1;
        }
        let lit: String = bytes[start..j].iter().collect();
        if let Some(code) = leading_code(&lit) {
            // 字面量正好等于某个**前缀族常量**（`pub const REGRESS_PREFIX:
            // &str = "E_REGRESS_"`）时，它声明的是一族而不是一个码。
            // 族里的每一个具体码都已单独登记——漏一个，
            // `every_code_the_code_actually_returns_is_in_the_table`
            // 会把它连同所在文件一起点出来。
            if code != errcodes::REGRESS_PREFIX {
                out.insert(code);
            }
        }
        i = j + 1;
    }
    out
}

/// 取字面量首段的码。与 `psychdoc::contract::leading_code` 同一口径：
/// `E_` + `[A-Z0-9_]+`，必须紧跟 `:` / 空格或到末尾才算
/// （免得把正文里恰好出现的英文缩写当成码）。
fn leading_code(lit: &str) -> Option<String> {
    let rest = lit.strip_prefix("E_")?;
    let end = rest
        .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    match rest[end..].chars().next() {
        None => Some(format!("E_{}", &rest[..end])),
        Some(c) if c == ':' || c.is_whitespace() => Some(format!("E_{}", &rest[..end])),
        Some(_) => None,
    }
}

/// 探针码：`src/` 里出现、但**刻意不是**产品契约一部分的码。
///
/// 全部来自这张表自己所在 crate 的单测——它们断言的是「查不到就不猜」
/// 「未知码回落配置侧」，登记在册反而会让值班的人去找一个不存在的码。
/// 列成一张表而不是就地 `continue`，是为了让「多出来的探针要在这里登记」
/// 成为一条可核对的规矩。
const PROBE_CODES: &[&str] = &[
    // contract.rs 单测：断言「未知码回落配置侧」的假码。
    "E_SOMETHING_NEW",
    // errcodes.rs 单测：断言「查不到就不猜」的假码。
    "E_NOT_A_REAL_CODE",
];

/// 全工作区扫一遍，返回 `相对 core/crates 的路径 → 该文件里的码集合`。
fn scan_workspace() -> BTreeMap<String, BTreeSet<String>> {
    let root = crates_dir();
    let mut out = BTreeMap::new();
    for crate_dir in std::fs::read_dir(&root).expect("core/crates 应可读").flatten() {
        let src = crate_dir.path().join("src");
        if !src.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        src_files(&src, &mut files);
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else {
                continue;
            };
            let found = codes_in(&text);
            if found.is_empty() {
                continue;
            }
            let rel = f
                .strip_prefix(&root)
                .expect("扫描结果应能相对化")
                .to_string_lossy()
                .to_string();
            out.insert(rel, found);
        }
    }
    out
}

#[test]
fn every_code_the_code_actually_returns_is_in_the_table() {
    let scan = scan_workspace();
    assert!(
        scan.len() >= 5,
        "只扫到 {} 个 crate 的 src，扫描范围本身可能坏了",
        scan.len()
    );

    let mut missing: BTreeSet<String> = BTreeSet::new();
    for (file, codes) in &scan {
        for code in codes {
            // 测试专用哨兵：断言「查不到就不猜」用的假码，
            // 刻意不在产品契约里（见 [`PROBE_CODES`]）。
            if PROBE_CODES.contains(&code.as_str()) {
                continue;
            }
            if errcodes::lookup(code).is_none() {
                missing.insert(format!("{code}（{file}）"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "代码里返回了这些码，但错误码表里没有登记（值班的人会照着缺行的表查错方向）：\n  {}",
        missing.into_iter().collect::<Vec<_>>().join("\n  ")
    );
}

#[test]
fn every_row_in_the_table_names_a_file_that_really_returns_it() {
    let scan = scan_workspace();
    let mut wrong: Vec<String> = Vec::new();
    for c in ERROR_CODES {
        match scan.get(c.origin) {
            // 前缀族（E_REGRESS_*）的每一行指向同一个文件，逐个核对即可。
            Some(found) if found.contains(c.code) => {}
            Some(_) => wrong.push(format!("{} → {} 里扫不到它", c.code, c.origin)),
            None => wrong.push(format!("{} → {} 不存在", c.code, c.origin)),
        }
    }
    assert!(
        wrong.is_empty(),
        "错误码表的 origin 与代码对不上（表里留着一行代码已不再返回的码）：\n  {}",
        wrong.join("\n  ")
    );
}

#[test]
fn the_table_tells_the_reader_whether_retrying_can_help() {
    // 「不可重试」占多数才对：多数错误是配置与形态问题，
    // 重试一万次也不会自己变好。若哪天全变成可重试，多半是分类写崩了。
    let retryable = ERROR_CODES.iter().filter(|c| c.retryable).count();
    assert!(
        retryable < ERROR_CODES.len() / 2,
        "{retryable}/{} 标成可重试，多到让 CI 陷入无意义的重试",
        ERROR_CODES.len()
    );
    assert!(retryable > 0, "一个可重试的都没有，说明外部依赖侧的码被误分类了");
}

#[test]
fn the_exported_markdown_and_the_table_cannot_diverge() {
    // 导出是**函数**：同一份表、同一段 markdown。
    // 手抄进 README 的那份是 Task 15 的事，这里只保证数据源本身是一份。
    assert_eq!(psychdoc_store::error_codes_markdown(), errcodes::markdown_table());
    assert_eq!(ERROR_CODES.len(), errcodes::ERROR_CODES.len());
}

#[test]
fn the_descriptions_are_written_for_a_person_on_shift() {
    // 每行都要能回答「谁该处理」——处置建议里出现「见文档」就算不合格。
    for c in ERROR_CODES {
        for (field, text) in [("触发条件", c.trigger), ("处置建议", c.remedy)] {
            assert!(
                text.len() >= 8,
                "{} 的{}太短，写不出可执行的信息：{text}",
                c.code,
                field
            );
            assert!(
                !text.contains("见文档") && !text.contains("TBD") && !text.contains("待补"),
                "{} 的{}里留了占位",
                c.code,
                field
            );
        }
    }
}
