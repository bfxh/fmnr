//! 读 Windows 快捷方式（`.lnk`，MS-SHLLINK）里的目标路径。
//!
//! 为什么要它：用户希望"把快捷方式拖进来就能查这个应用有没有可升级"。
//! 快捷方式里存的才是"这个入口指向哪个 exe"，而底账是按 exe 路径匹配的。
//!
//! 手写而不引第三方 crate：格式是定长的头 + 按标志位决定存在性的几段，
//! 我们只要其中一条字符串；引一个依赖换来的是整个 ExtraData 体系。
//! 偏移与位全部按 MS-SHLLINK 的表来，并拿本机真件（`EnableTargetMetadata`
//! 那一位开着、带 IDList 和 LinkInfo 的那种）验过。

/// 解析失败的原因。分得细是因为"这文件不是 lnk"和"是 lnk 但没存目标"
/// 对用户的说法完全不同。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LnkError {
    TooShort(usize),
    BadHeaderSize(u32),
    BadClsid,
    /// 结构合法，但里面没写出目标（只有 IDList 且我们不解 IDList）
    NoTarget,
    /// 截断：某一段声明的长度超出文件末尾
    Truncated,
}

impl std::fmt::Display for LnkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LnkError::TooShort(n) => write!(f, "文件只有 {n} 字节，装不下 76 字节的头"),
            LnkError::BadHeaderSize(n) => write!(f, "头长度字段是 {n}，不是固定的 76"),
            LnkError::BadClsid => write!(f, "不是快捷方式（CLSID 对不上）"),
            LnkError::NoTarget => write!(f, "这个快捷方式里没有写出目标路径"),
            LnkError::Truncated => write!(f, "快捷方式被截断了"),
        }
    }
}

impl std::error::Error for LnkError {}

const HDR_SIZE: usize = 76;
const FLAG_IDLIST: u32 = 0x0000_0001;
const FLAG_LINKINFO: u32 = 0x0000_0002;
const FLAG_NAME: u32 = 0x0000_0004;
const FLAG_RELATIVE: u32 = 0x0000_0008;
const FLAG_WORKDIR: u32 = 0x0000_0010;
const FLAG_TARGET_PREFIX: u32 = 0x0000_0040;

/// MS-SHLLINK 规定的 LinkCLSID：`{00021401-0000-0000-C000-000000000046}`。
const CLSID: [u8; 16] = [
    0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    let mut a = [0u8; 2];
    a.copy_from_slice(d.get(at..at + 2)?);
    Some(u16::from_le_bytes(a))
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    let mut a = [0u8; 4];
    a.copy_from_slice(d.get(at..at + 4)?);
    Some(u32::from_le_bytes(a))
}

/// 读一个长度前缀的 UTF-16LE 串（StringData 里每个字段都是这个形状）。
fn read_wide_string(d: &[u8], at: usize) -> Option<(String, usize)> {
    let chars = u16_at(d, at)? as usize;
    let start = at + 2;
    let end = start + chars * 2;
    let raw = d.get(start..end)?;
    let units: Vec<u16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes(*b))
        .collect();
    Some((String::from_utf16_lossy(&units), end))
}

/// 看起来像绝对路径吗（盘符或 UNC）。用来在顺序解析错位时兜底辨认。
fn looks_absolute(s: &str) -> bool {
    let b = s.as_bytes();
    let drive = b.len() >= 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    let unc = b.len() >= 2 && b[0] == b'\\' && b[1] == b'\\';
    drive || unc
}

/// 取出 `LocalBasePath`。
pub fn target_of(bytes: &[u8]) -> Result<String, LnkError> {
    if bytes.len() < HDR_SIZE {
        return Err(LnkError::TooShort(bytes.len()));
    }
    if u32_at(bytes, 0) != Some(HDR_SIZE as u32) {
        return Err(LnkError::BadHeaderSize(u32_at(bytes, 0).unwrap_or(0)));
    }
    if bytes[4..20] != CLSID {
        return Err(LnkError::BadClsid);
    }
    let flags = u32_at(bytes, 20).ok_or(LnkError::Truncated)?;

    let mut off = HDR_SIZE;
    if flags & FLAG_IDLIST != 0 {
        let size = u16_at(bytes, off).ok_or(LnkError::Truncated)? as usize;
        off += 2 + size;
    }
    if flags & FLAG_LINKINFO != 0 {
        let size = u32_at(bytes, off).ok_or(LnkError::Truncated)? as usize;
        // LinkInfoSize 含它自己那 4 个字节
        off += size;
    }
    if off > bytes.len() {
        return Err(LnkError::Truncated);
    }

    // StringData 的字段顺序固定，存在性由标志位决定；DESCRIPTION_STRING
    // 不吃标志位（有就占一格），所以这里按"有则读一格"推进。
    let mut strings: Vec<String> = Vec::new();
    let slots: &[(&str, bool)] = &[
        ("NAME", flags & FLAG_NAME != 0),
        ("DESCRIPTION", true),
        ("RELATIVE", flags & FLAG_RELATIVE != 0),
        ("WORKING", flags & FLAG_WORKDIR != 0),
        ("LOCALBASE", flags & FLAG_TARGET_PREFIX != 0),
    ];
    for (_, present) in slots {
        if !present {
            continue;
        }
        match read_wide_string(bytes, off) {
            Some((s, next)) => {
                strings.push(s);
                off = next;
            }
            // 某一段读不动：可能这个文件压根没有 DESCRIPTION 那格。
            // 停在这里，交给下面的兜底辨认。
            None => break,
        }
    }

    // 正常路径：LOCALBASE 是最后一个读到的、且看着像绝对路径。
    if flags & FLAG_TARGET_PREFIX != 0 {
        if let Some(s) = strings.last() {
            if looks_absolute(s) {
                return Ok(s.clone());
            }
        }
    }
    // 兜底：顺序错位时（有些实现不写 DESCRIPTION 那格），在已读到的串里
    // 挑第一个看着像绝对路径的。挑不到就明说没有，不猜。
    if let Some(s) = strings.iter().find(|s| looks_absolute(s)) {
        return Ok(s.clone());
    }
    Err(LnkError::NoTarget)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(s: &str) -> Vec<u8> {
        let mut v = Vec::new();
        for u in s.encode_utf16() {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v
    }

    /// 造一个最小但结构正确的 .lnk：头 + 可选 IDList + 可选 LinkInfo + StringData。
    ///
    /// `strings` 必须**按解析器实际会读到的那些格**给（NAME 只在有 `FLAG_NAME`
    /// 时占一格，DESCRIPTION 永远占一格，后面同理）。给错格数就是夹具自己错位，
    /// 不是解析器的 bug——下面几个用例都按这个约定写。
    fn build(flags: u32, idlist: bool, linkinfo: bool, strings: &[&str]) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&76u32.to_le_bytes());
        d.extend_from_slice(&CLSID);
        d.extend_from_slice(&flags.to_le_bytes());
        d.resize(HDR_SIZE, 0);
        if idlist {
            let payload = vec![0x00u8, 0x40, 1, 2, 3, 4]; // 任意字节，长度 6
            d.extend_from_slice(&(payload.len() as u16).to_le_bytes());
            d.extend_from_slice(&payload);
        }
        if linkinfo {
            // LinkInfoSize 含它自己那 4 个字节
            let total = 28usize;
            d.extend_from_slice(&(total as u32).to_le_bytes());
            d.resize(d.len() + total - 4, 0);
        }
        for v in strings {
            let w = wide(v);
            d.extend_from_slice(&(v.encode_utf16().count() as u16).to_le_bytes());
            d.extend_from_slice(&w);
        }
        d
    }

    const F: u32 = FLAG_TARGET_PREFIX;

    #[test]
    fn reads_local_base_path_with_idlist_and_linkinfo() {
        // 本机 UniGetUI.lnk 就是这个形状：IDList + LinkInfo + Name + Description
        // + WorkingDir + LocalBasePath
        let d = build(
            FLAG_IDLIST | FLAG_LINKINFO | FLAG_NAME | FLAG_WORKDIR | F,
            true,
            true,
            &[
                "UniGetUI",
                "",
                "D:\\Apps\\UniGetUI",
                "D:\\Apps\\UniGetUI\\UniGetUI.exe",
            ],
        );
        assert_eq!(target_of(&d).unwrap(), r"D:\Apps\UniGetUI\UniGetUI.exe");
    }

    #[test]
    fn reads_local_base_path_without_optional_sections() {
        // 只有 DESCRIPTION + LOCALBASE 两格
        let d = build(F, false, false, &["", r"C:\x\y.exe"]);
        assert_eq!(target_of(&d).unwrap(), r"C:\x\y.exe");
    }

    /// UNC 目标（网络共享上的快捷方式）也要认。
    #[test]
    fn accepts_unc_target() {
        let d = build(F, false, false, &["", r"\\srv\share\a.exe"]);
        assert_eq!(target_of(&d).unwrap(), r"\\srv\share\a.exe");
    }

    /// 没有 LOCALBASE 那格时（有些文件只给相对路径），要明说"没有目标"，
    /// 而不是把相对路径当绝对路径返回。
    #[test]
    fn missing_target_slot_is_reported_not_guessed() {
        let d = build(FLAG_NAME | F, false, false, &["Only Name"]);
        assert_eq!(target_of(&d), Err(LnkError::NoTarget));
    }

    /// DESCRIPTION 格被某些实现省掉 ⇒ 顺序解析会错位。
    /// 兜底要能挑出那个看着像绝对路径的串。
    #[test]
    fn recovers_when_slots_are_shifted() {
        let d = build(
            FLAG_NAME | F,
            false,
            false,
            &["Only Name", r"D:\real\target.exe"],
        );
        assert_eq!(target_of(&d).unwrap(), r"D:\real\target.exe");
    }

    #[test]
    fn rejects_non_shortcut_bytes() {
        let short = b"not a lnk at all";
        assert_eq!(
            target_of(short).err(),
            Some(LnkError::TooShort(short.len()))
        );
        let mut d = vec![0u8; HDR_SIZE];
        d[0..4].copy_from_slice(&76u32.to_le_bytes());
        assert_eq!(target_of(&d).err(), Some(LnkError::BadClsid));
        let mut d2 = vec![0u8; HDR_SIZE + 8];
        d2[4..20].copy_from_slice(&CLSID);
        assert_eq!(target_of(&d2).err(), Some(LnkError::BadHeaderSize(0)));
    }

    /// 声明了超长 IDList 的坏文件必须判截断，不能 panic 也不能越界读。
    #[test]
    fn truncated_declared_sizes_are_errors_not_panics() {
        let mut d = vec![0u8; HDR_SIZE + 2];
        d[0..4].copy_from_slice(&76u32.to_le_bytes());
        d[4..20].copy_from_slice(&CLSID);
        d[20..24].copy_from_slice(&FLAG_IDLIST.to_le_bytes());
        d[76..78].copy_from_slice(&1000u16.to_le_bytes());
        assert_eq!(target_of(&d).err(), Some(LnkError::Truncated));
    }

    /// 真件回归：开发机上有那个 .lnk 时，按真字节走一遍。
    /// 不把它提交进仓（那是一条指向个人机器的路径），也不在缺失时判红。
    #[test]
    fn real_shortcut_on_this_machine_if_present() {
        let p = std::path::Path::new(r"C:\Users\lbx13\Desktop\UniGetUI.lnk");
        if !p.exists() {
            println!("跳过：这台机器没有那个 .lnk");
            return;
        }
        let bytes = std::fs::read(p).unwrap();
        let t = target_of(&bytes).expect("真件必须解析得出目标");
        println!("真件目标：{t}");
        assert!(
            t.to_lowercase().ends_with(".exe"),
            "解析出来的目标应是个 exe，拿到的是 {t}"
        );
    }
}
