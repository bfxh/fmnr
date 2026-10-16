//! 从注册表 URL 认出 GitHub 仓。
//!
//! 只做**纯解析**，不做网络。理由不是省依赖，是判据：上游版本查询要成立，
//! 得先知道"这条 URL 到底指不指一个仓"，而那一步本机实测的覆盖面只有
//! 6/273 条（2.2%）——所以这一层能钉死的是"认得准"，
//! 拿它当"这台机器的更新都能查"是过度声称。

/// 认得出 `owner/repo` 就返回它，否则 `None`。
///
/// 覆盖注册表里真实出现过的几种写法（样例取自本机那 6 条）：
/// `https://github.com/o/r`、`.../o/r/`、`.../o/r/releases`、`.../o/r/issues/`、
/// `.../o/r.git`、大小写混写、以及 `http://`。
pub fn repo_from_url(url: &str) -> Option<String> {
    let s = url.trim();
    // 用 ASCII 小写副本比较 scheme/主机：小写化不改字节长度，
    // 所以两个副本的下标是对齐的，可以拿小写的判断结果去切原串（保住大小写）。
    let low = s.to_ascii_lowercase();
    let at = low.find("://")?;
    if &low[..at] != "https" && &low[..at] != "http" {
        return None;
    }
    let start = at + 3;
    let slash = start + (s[start..].find('/')?);
    if &low[start..slash] != "github.com" {
        return None;
    }
    let mut it = s[slash + 1..].split('/').filter(|p| !p.is_empty());
    let owner = it.next()?.trim_end_matches(".git");
    let repo = it.next()?;
    let repo = repo.trim_end_matches(".git");
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    // owner/repo 只允许仓库名里真会出现的字符：把 `..`、`?`、空格这类挡掉，
    // 免得后面拼请求 URL 时被注入。
    let ok = |t: &str| {
        !t.is_empty()
            && t.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    };
    // 首字符不能是点（`..` 逃逸）
    if !ok(owner) || !ok(repo) || owner.starts_with('.') || repo.starts_with('.') {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_seen_in_the_registry() {
        assert_eq!(
            repo_from_url("https://github.com/chen08209/FlClash").as_deref(),
            Some("chen08209/FlClash")
        );
        assert_eq!(
            repo_from_url("https://github.com/rustdesk/rustdesk/issues/").as_deref(),
            Some("rustdesk/rustdesk")
        );
        assert_eq!(
            repo_from_url("https://github.com/moonlight-stream/moonlight-qt/releases").as_deref(),
            Some("moonlight-stream/moonlight-qt")
        );
        assert_eq!(
            repo_from_url("HTTPS://GitHub.com/BurntSushi/ripgrep").as_deref(),
            Some("BurntSushi/ripgrep")
        );
        assert_eq!(
            repo_from_url("https://github.com/vendor/foo.git").as_deref(),
            Some("vendor/foo")
        );
        assert_eq!(
            repo_from_url("https://github.com/vendor/foo/").as_deref(),
            Some("vendor/foo")
        );
    }

    /// 不是仓的必须返回 None，不能"看起来像就认"。
    #[test]
    fn rejects_non_repo_urls() {
        assert_eq!(repo_from_url("https://devolutions.net/unigetui/"), None);
        assert_eq!(repo_from_url("https://github.com/owner"), None);
        assert_eq!(repo_from_url(""), None);
        assert_eq!(repo_from_url("github.com"), None);
        // 自建域名冒充
        assert_eq!(repo_from_url("https://notgithub.com/evil/repo"), None);
        assert_eq!(repo_from_url("https://github.com.evil.com/o/r"), None);
        // 路径穿越
        assert_eq!(repo_from_url("https://github.com/../../etc/passwd"), None);
        assert_eq!(repo_from_url("https://github.com/o/r?x=/../../a"), None);
    }
}
