//! 文件名子串匹配：UI 的流式搜索与 MCP/CLI 的有界搜索共用这一份判定。
//!
//! 分开实现过一版（UI 自己写了一份），结果是「高亮命中」和「搜索命中」
//! 可能给出不同答案——用户看到高亮却搜不到，或反过来。判定只能有一处。

/// 在 `haystack` 里找 `needle`，返回**原文的字节范围**；找不到返回 None。
///
/// `case_sensitive = false` 时按小写比较，但返回的范围一定落在 `haystack`
/// 的真实字符边界上（`to_lowercase` 对 ß/İ 等会改变长度，不能直接拿它的偏移）。
pub fn find_range(
    haystack: &str,
    needle: &str,
    case_sensitive: bool,
) -> Option<std::ops::Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    if case_sensitive {
        return haystack.find(needle).map(|i| i..i + needle.len());
    }
    let hchars: Vec<char> = haystack.to_lowercase().chars().collect();
    let nchars: Vec<char> = needle.to_lowercase().chars().collect();
    if nchars.len() > hchars.len() {
        return None;
    }
    for i in 0..=(hchars.len() - nchars.len()) {
        if hchars[i..i + nchars.len()] == nchars[..] {
            let start = haystack.char_indices().nth(i).map(|(b, _)| b)?;
            let end = haystack
                .char_indices()
                .nth(i + nchars.len())
                .map(|(b, _)| b)
                .unwrap_or(haystack.len());
            return Some(start..end);
        }
    }
    None
}

/// 大小写不敏感的文件名子串判定（搜索用）。
///
/// `needle` 的**大小写无关**：这里自己小写化。曾经要求调用方先小写
/// （参数名 `needle_lower`），结果是忘了小写的调用方拿到静默的漏判——
/// 搜索框里打 "README" 找不到 readme.md。判定只能有一处，陷阱也不该留给调用方。
pub fn name_matches(name: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    name.to_lowercase().contains(&needle.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_case_insensitive_finds_byte_range() {
        let r = find_range("Report.txt", "port", false).unwrap();
        assert_eq!(&"Report.txt"[r], "port");
    }

    #[test]
    fn case_sensitive_rejects_wrong_case() {
        assert!(find_range("Report.txt", "report", true).is_none());
        assert!(find_range("Report.txt", "Report", true).is_some());
    }

    /// 非 ASCII：小写化后长度会变，范围必须仍落在原文的字符边界上。
    #[test]
    fn multibyte_falls_back_to_original_boundaries() {
        let hay = "Ökobilanz_2024.xlsx";
        let r = find_range(hay, "bilanz", false).unwrap();
        assert_eq!(&hay[r], "bilanz");
    }

    #[test]
    fn empty_needle_is_no_match() {
        assert!(find_range("abc", "", false).is_none());
        assert!(!name_matches("abc", ""));
    }

    #[test]
    fn needle_longer_than_haystack_is_no_match() {
        assert!(find_range("ab", "abc", false).is_none());
    }

    #[test]
    fn name_matches_is_case_insensitive() {
        assert!(name_matches("README.MD", "readme"));
        assert!(name_matches("readme.md", "README"));
        assert!(!name_matches("readme.md", "changelog"));
    }

    /// 两个入口对同一对输入的判定必须一致——这是把它们放在一个模块的原因。
    /// needle 大小写都要试：调用方传原样的大小写是常态，两者不能分道扬镳。
    #[test]
    fn range_and_bool_agree() {
        for (name, needle) in [
            ("Report.txt", "port"),
            ("Report.txt", "PORT"),
            ("Report.txt", "zzz"),
            ("readme.md", "README"),
            ("Ökobilanz_2024.xlsx", "bilanz"),
            ("Ökobilanz_2024.xlsx", "BILANZ"),
            ("a", "abc"),
        ] {
            let by_range = find_range(name, needle, false).is_some();
            let by_bool = name_matches(name, needle);
            assert_eq!(
                by_range, by_bool,
                "{name:?} / {needle:?} 两个入口判定不一致"
            );
        }
    }
}
