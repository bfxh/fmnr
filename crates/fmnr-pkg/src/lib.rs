//! 包更新盘点：把 `winget upgrade` 的表格读成结构化条目。
//!
//! **只做只读盘点，不代跑升级。** 三个理由，都不是谦虚：
//! 1. `winget upgrade` 没有 `--output-format`（本机 `winget upgrade --help` 实测），
//!    输出是**本地化的对齐表格**，读它=读一张会随语言变的报表；
//! 2. 升级要动系统状态，而 .NET 运行时这类包**第一次点击只是排队**，
//!    要再点一次才真装（用户 2026-10-17 的告诫）。fmnr 在这里当"第二个进度条"
//!    只会让人以为已经装完了；
//! 3. 一旦代跑，就得替用户决定 MSI 的 `Scope`/`Silent`/重启策略——那是 winget 的活。
//!
//! 所以这里只负责"看清楚有什么可升"，动作交回 `winget upgrade --id <ID>` 或 UniGetUI。

/// 一行可升级条目。字段按 winget 表格的五列（名称 / ID / 版本 / 可用 / 源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upgrade {
    pub name: String,
    pub id: String,
    /// 已装版本
    pub installed: String,
    /// 可升级到的版本
    pub available: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("winget 输出是空的：可能没装、被组策略挡下，或者源还没同意")]
    Empty,
    #[error("认不出表头（前两行应是「5 列标题」+「一条横线」）：{0}")]
    NoHeader(String),
    #[error("第 {line} 行拆不出「名称 ID 已装 可用 源」：{text} —— {why}")]
    BadRow {
        line: usize,
        text: String,
        why: String,
    },
    #[error("winget 自己说 {announced} 项可升级，这里只读到 {parsed} 行：表格被换行或列错位打断了，不猜")]
    CountMismatch { parsed: usize, announced: usize },
    #[error("表格后面还有读不懂的行，而且行数对不上任何汇总数字：{0}")]
    Trailing(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(String),
    #[error("winget 的输出不是 UTF-8：编码假设破了，按别的编码猜会把包名读坏，直接判失败")]
    NotUtf8,
    #[error("winget 退出码 {code:?}：{detail}")]
    Status { code: Option<i32>, detail: String },
    #[error(transparent)]
    Parse(#[from] ParseError),
}

/// 版本列的判别：只允许版本串里会出现的字符，且至少含一个数字。
///
/// 这条不是洁癖，是防**假通过**：英文 locale 下 `No applicable upgrade was found.`
/// 恰好是 5 个词，光按"5 段"拆会把它读成一条可升级记录。
/// 加上"含数字"后 `was` / `found.` 都进不来。
fn looks_like_version(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
        && s.chars().any(|c| c.is_ascii_digit())
}

/// 解析 `winget upgrade` 的标准输出。
///
/// 刻意**严格**：任何一行拆不出来就整份判失败，而不是跳过那一行。
/// 静默丢行会伪装成"没有可升级的包"，那是这类盘点最坏的一种错法。
pub fn parse_upgrade_table(text: &str) -> Result<Vec<Upgrade>, ParseError> {
    let lines: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.trim()))
        .filter(|(_, l)| !l.is_empty())
        .collect();

    let Some((_, header)) = lines.first() else {
        return Err(ParseError::Empty);
    };
    if header.split_whitespace().count() != 5 {
        return Err(ParseError::NoHeader(format!("第一行不是 5 列：{header}")));
    }
    let (_, rule) = lines
        .get(1)
        .ok_or_else(|| ParseError::NoHeader(format!("表头后面没有分隔线：{header}")))?;
    if rule.chars().count() < 20 || !rule.chars().all(|c| c == '-') {
        return Err(ParseError::NoHeader(format!(
            "第二行不是横线（拿到 {rule:?}）"
        )));
    }

    let mut rows: Vec<Upgrade> = Vec::new();
    let mut tail: Vec<&str> = Vec::new();
    let mut broken: Option<(usize, String, String)> = None;
    for (lineno, line) in lines.iter().skip(2) {
        if tail.is_empty() {
            match split_row(line) {
                Ok(row) => rows.push(row),
                // 拆不出来的第一行就是汇总行的起点：不再往里塞数据，
                // 后面统一按「winget 自己报了几项」来核对读到的行数。
                Err(why) => {
                    broken = Some((*lineno, line.to_string(), why));
                    tail.push(line);
                }
            }
        } else {
            tail.push(line);
        }
    }

    match announced_count(&tail) {
        Some(n) if n != rows.len() => Err(ParseError::CountMismatch {
            parsed: rows.len(),
            announced: n,
        }),
        Some(_) => Ok(rows),
        // 没有数字汇总：要么真的没东西可升（0 行 + 一句说明），
        // 要么整份输出压根没有结尾（只有表格）——这两种都按读到的算。
        None if tail.is_empty() || rows.is_empty() => Ok(rows),
        None => match broken {
            Some((line, text, why)) => Err(ParseError::BadRow { line, text, why }),
            None => Err(ParseError::Trailing(tail.join(" / "))),
        },
    }
}

/// 一行数据 = 最后 4 段是 ID/已装/可用/源，**剩下的整段是名称**（名称里有空格）。
fn split_row(line: &str) -> Result<Upgrade, String> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 5 {
        return Err(format!("只有 {} 段，至少要 5 段", f.len()));
    }
    let n = f.len();
    let (id, installed, available, source) = (f[n - 4], f[n - 3], f[n - 2], f[n - 1]);
    if !looks_like_version(installed) {
        return Err(format!("倒数第 3 段 {installed:?} 不像已装版本"));
    }
    if !looks_like_version(available) {
        return Err(format!("倒数第 2 段 {available:?} 不像可用版本"));
    }
    Ok(Upgrade {
        name: f[..n - 4].join(" "),
        id: id.to_string(),
        installed: installed.to_string(),
        available: available.to_string(),
        source: source.to_string(),
    })
}

/// 从汇总行里取那个"几项可升级"的数字。取第一个纯数字词，中英文都命中。
fn announced_count(tail: &[&str]) -> Option<usize> {
    tail.iter()
        .flat_map(|l| l.split_whitespace())
        .find(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()))
        .and_then(|t| t.parse().ok())
}

/// 跑一次 `winget upgrade` 并解析。阻塞调用，放工作线程里用。
pub fn list_upgrades() -> Result<Vec<Upgrade>, Error> {
    let out = run_winget(&["upgrade", "--accept-source-agreements"])?;
    let text = std::str::from_utf8(&out.stdout).map_err(|_| Error::NotUtf8)?;
    Ok(parse_upgrade_table(text)?)
}

fn run_winget(args: &[&str]) -> Result<std::process::Output, Error> {
    let mut cmd = std::process::Command::new("winget");
    cmd.args(args);
    #[cfg(windows)]
    {
        // winget 是 app-execum 别名，直接 spawn 会闪一个控制台窗口出来。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd
        .output()
        .map_err(|e| Error::Io(format!("启动 winget 失败：{e}")))?;
    if !out.status.success() {
        return Err(Error::Status {
            code: out.status.code(),
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(out)
}
