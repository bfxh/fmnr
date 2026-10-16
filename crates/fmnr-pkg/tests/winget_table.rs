//! 解析器的锁。语料是**本机 `winget upgrade` 的真实输出**
//! （`fixture-winget-upgrade.txt`，2026-10-17 抓的，只去掉探针自己加的 rc 行），
//! 不是我按想象的格式手写的样子——手写的样子只会把解析器钉在我的假设上。
//!
//! 唯一换掉的是**包名和 ID**：那份清单等于"这台机器装了哪些软件"，而仓是公开的。
//! 列宽、CJK 名、结尾计数行、版本列撑满这些**形状**逐字符保留——解析器锁的是形状。

use fmnr_pkg::{parse_upgrade_table, ParseError, Upgrade};

const REAL: &str = include_str!("fixture-winget-upgrade.txt");

fn row(name: &str, id: &str, installed: &str, available: &str, source: &str) -> Upgrade {
    Upgrade {
        name: name.into(),
        id: id.into(),
        installed: installed.into(),
        available: available.into(),
        source: source.into(),
    }
}

#[test]
fn real_output_reads_every_row() {
    let rows = parse_upgrade_table(REAL).expect("真实输出必须能解析");
    // 11 这个数字来自表格自己的结尾行「11 升级可用。」，不是数出来的。
    assert_eq!(rows.len(), 11, "整份表格的行数");
    assert_eq!(
        rows[0],
        row("AlphaTool", "vendor-a.alphatool", "0.5.2", "0.5.7", "winget")
    );
    assert_eq!(
        rows[10],
        row(
            "示例输入法",
            "vendor-j.exampleinput",
            "2.1.4.6",
            "3.0.0.37",
            "winget"
        )
    );
}

/// 从右往左数四段，剩下整段是名称。这条钉住三个真实陷阱：
/// 名称里有空格和括号、名称以数字结尾（数错一位就把版本号当成 ID）、
/// 版本列被撑满到两列之间只剩一个空格。
/// 按固定列宽或按空格对半切都会在这些行上错位。
#[test]
fn names_with_spaces_and_overflowed_version_columns() {
    let rows = parse_upgrade_table(REAL).unwrap();
    assert_eq!(
        rows[1],
        row(
            "Contoso Runtime - 8.0.31 (x64)",
            "contoso.runtime.8",
            "8.0.31",
            "8.0.32",
            "winget"
        )
    );
    assert_eq!(
        rows[2],
        row(
            "Sample Browser",
            "vendor-b.samplebrowser",
            "155.0.4283.45",
            "155.0.4283.60",
            "winget"
        )
    );
    assert_eq!(
        rows[5],
        row("Helper 0.4.8", "vendor-e.helper", "0.4.8", "1.106.3", "winget")
    );
}

/// 反「静默丢行」的门：少读一行必须判红，而不是给出 10 项的清单。
/// 「看起来没东西可升」是这类盘点最坏的一种错法。
#[test]
fn dropping_a_row_fails_because_winget_announced_the_count() {
    let mutated: String = REAL
        .lines()
        .filter(|l| !l.starts_with("Editor "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        mutated.lines().count() < REAL.lines().count(),
        "夹具得先真的被改到，这条测试才有意义"
    );
    let err = parse_upgrade_table(&mutated).expect_err("少一行必须判红");
    assert_eq!(
        err,
        ParseError::CountMismatch {
            parsed: 10,
            announced: 11
        }
    );
}

/// 英文 locale 下 `No applicable upgrade was found.` 恰好是 5 个词。
/// 没有「版本列必须含数字」这条判别，它会被读成一条可升级记录。
#[test]
fn english_no_upgrades_sentence_is_not_mistaken_for_a_row() {
    let text = "Name Id Version Available Source\n\
                ------------------------------------------------\n\
                No applicable upgrade was found.\n";
    assert_eq!(parse_upgrade_table(text).unwrap(), vec![]);
}

/// 表头只按**结构**认（5 列 + 一条横线），不按中文标签认——
/// 换语言、换 winget 版本时不该因此整片判红。
#[test]
fn header_recognition_is_structural_not_localized() {
    let text = "Name           Id              Version    Available  Source\n\
                ------------------------------------------------------------------------\n\
                Sample Editor  vendor-i.editor  1.22.0        1.24.2        winget\n\
                1 upgrade available.\n";
    let rows = parse_upgrade_table(text).expect("英文表头同样要能读");
    assert_eq!(
        rows,
        vec![row(
            "Sample Editor",
            "vendor-i.editor",
            "1.22.0",
            "1.24.2",
            "winget"
        )]
    );
}

#[test]
fn empty_output_is_an_error_not_an_empty_list() {
    assert_eq!(
        parse_upgrade_table("  \n\n").unwrap_err(),
        ParseError::Empty
    );
}

#[test]
fn missing_dash_rule_rejects_the_header() {
    let text = "名称 ID 版本 可用 源\nAlphaTool vendor-a.alphatool 0.5.2 0.5.7 winget\n";
    assert!(matches!(
        parse_upgrade_table(text),
        Err(ParseError::NoHeader(_))
    ));
}

/// 中间断了一行（名称太长被换行是真实风险），而结尾又没有可核对的数字时，
/// 要报出**是哪一行**拆不开，而不是悄悄给出短一截的清单。
#[test]
fn broken_row_without_a_count_names_the_line() {
    let text = "名称 ID 版本 可用 源\n\
                ----------------------------------------\n\
                Editor vendor-i.editor 1.22.0 1.24.2 winget\n\
                这一行是凭空的说明文字\n";
    let err = parse_upgrade_table(text).expect_err("拆不开的行必须判红");
    match err {
        ParseError::BadRow { line, text, why } => {
            assert_eq!(line, 4);
            assert!(text.starts_with("这一行"));
            assert!(why.contains("段"), "得说清为什么拆不出来：{why}");
        }
        other => panic!("期望 BadRow，拿到 {other:?}"),
    }
}
