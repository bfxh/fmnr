//! 读 Windows 快捷方式（`.lnk`，MS-SHLLINK）里的目标路径。
//!
//! 为什么要它：用户希望"把快捷方式拖进来就能查这个应用有没有可升级"。
//! 快捷方式里存的才是"这个入口指向哪个 exe"，而底账是按 exe 路径匹配的。
//!
//! 手写而不引第三方 crate：目标只是其中一条字符串，引一个依赖换来的是
//! 整个 ExtraData 体系。
//!
//! 🔴 目标**只**从 `LinkInfo` 的 `LocalBasePath`（加 `CommonPathSuffix`）取。
//! 这条范围是拿本机真件对质 Windows 自己的 oracle（`WScript.Shell` 的 `TargetPath`）
//! 量出来的：桌面 + 公共桌面 + 两个开始菜单 + `Recent` 去重共 **235 个 .lnk**，
//! 其中 **230 个当场可读**、**183 个带结构完整的 LinkInfo**。
//! 下表三档跑在**同一批件、同一个快照**上：
//!
//! | 取法 | 对 | 错 | 报"没有" |
//! | --- | --- | --- | --- |
//! | 修前：读 StringData 里我以为的 DrivePath 那一格 | 26 | **155** | 49 |
//! | 修对 LinkInfo 之后再拼 `LinkTargetIDList` 最后一个 FileEntryItem 的文件名 | 175 | 5 | 50 |
//! | 本实现：只读 LinkInfo（+ 后缀拼接规则） | **176** | 4 | 50 |
//!
//! 由此定三条：
//! 1. StringData 根本没有"DrivePath"那一格（只有 Name / RelativePath /
//!    WorkingDir / Arguments / IconLocation 五格，修前把 0x80 当"有 DrivePath"用，
//!    而 0x80 是 `IsUnicode`）——所以整个 StringData 解析连同那格兜底一起删掉，
//!    它就是把"解析不出来"变成"返回一个错答案"的元凶。
//! 2. 不拼 IDList 的文件名：LinkInfo 读对以后这步是**净负**（176 对变 175 对），
//!    因为真件的 LocalBasePath 本来就含文件名。
//! 3. 剩下 50 个没有 LinkInfo 的件一律 `NoTarget`，不拿 IDList 或相对路径去猜。
//!    那 4 条"错"是形如 `C:\Users\<装机那台机器的用户名>\…` 这种在别人机器上做出来的件：Windows 会
//!    用 IDList 把它重新落到本机同名目录，我们不跟这个行为，也不假装认得。

/// 解析失败的原因。分得细是因为"这文件不是 lnk"和"是 lnk 但没存目标"
/// 对用户的说法完全不同。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LnkError {
    TooShort(usize),
    BadHeaderSize(u32),
    BadClsid,
    /// 结构合法，但里面没写出目标（没有 LinkInfo，或 LinkInfo 里没有绝对路径）
    NoTarget,
    /// 截断或自相矛盾：某一段声明的长度超出文件末尾、或小到装不下自己的字段
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

/// MS-SHLLINK 规定的 LinkCLSID：`{00021401-0000-0000-C000-000000000046}`。
const CLSID: [u8; 16] = [
    0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];

/// 🔴 LinkInfo 头部的实测布局（偏移都相对 LinkInfo 起点）：
///
/// ```text
/// +0  LinkInfoSize            含这 4 个字节自己
/// +4  Version                 真件恒为 28
/// +8  LinkInfoFlags           真件实测只有 1 和 3 两种
/// +12 LinkInfoHeaderSize      183 个带 LinkInfo 的真件全为 28
/// +16 LocalBasePathOffset     ← 目标路径
/// +20 CommonRelativePathOffset
/// +24 CommonPathSuffixOffset  ← 需要接在 LocalBasePath 后面的那半截
/// +28 VolumeID（从 HeaderSize 处开始，头 7 个 u32 = 28 字节）
/// ```
///
/// `+8`/`+12` 的先后与常见转述相反，是这么定下来的：从桌面、公共桌面、两个开始
/// 菜单、Recent 去重出来 235 个 .lnk，其中 183 个带结构完整的 LinkInfo；
/// `+8` 恒是个小位掩码（值 1 有 160 个、值 3 有 23 个）、`+12` 恒为 28，而把
/// `+16` 那个数当偏移去指串时取出的正是 oracle 给的路径。按转述的顺序（把 `+8`
/// 当 HeaderSize）解，183 个真件一个都取不出路径。
/// 同一批件里 `+16`/`+24` 的偏移**全部落在 LinkInfoSize 之内**（0 个越界），
/// 所以下面按块尾收口不会误伤真件。
const LI_MIN_HEADER: usize = 28;
const LI_BASE_PATH: usize = 16;
const LI_COMMON_SUFFIX: usize = 24;

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

/// 看起来像绝对路径吗（盘符或 UNC）。不像就当没这条，不返回半截相对路径。
fn looks_absolute(s: &str) -> bool {
    let b = s.as_bytes();
    let drive = b.len() >= 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    let unc = b.len() >= 2 && b[0] == b'\\' && b[1] == b'\\';
    drive || unc
}

/// 取出快捷方式的目标路径。见文件头：只认 LinkInfo 的 LocalBasePath（+ 共同后缀），
/// 拿不到就 `NoTarget`——**不猜**。
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
        // IDList 只在这里**跳过去**：目标不从它取（文件头第 2 条）。
        // IDListSize 不含它自己那 2 个字节。
        let size = u16_at(bytes, off).ok_or(LnkError::Truncated)? as usize;
        off = off
            .checked_add(2)
            .and_then(|x| x.checked_add(size))
            .ok_or(LnkError::Truncated)?;
        if off > bytes.len() {
            return Err(LnkError::Truncated);
        }
    }
    if flags & FLAG_LINKINFO == 0 {
        return Err(LnkError::NoTarget);
    }
    link_info_target(bytes, off)
}

/// 从 `LinkInfo` 结构里复原目标路径。
///
/// 两条实测来的规矩：
/// - 串偏移必须落在 `[LinkInfo起点+28, LinkInfo起点+LinkInfoSize)` 内。真件的串
///   都在本块之内（183 个里 0 个越界），所以越界一律当坏件，而不去隔壁 StringData
///   里捞一条看着像路径的串。
/// - `CommonPathSuffix` 要么已经是 `LocalBasePath` 的尾巴（Blender 那种：整条路径
///   存在 LocalBasePath 里，后缀只是它末尾的 `exe`），要么是真缺的那半截
///   （GitHub Desktop 那种：`C:\Users\` + `dev\AppData\Local\…\GitHubDesktop.exe`）。
///   所以只在"不是尾巴"时拼接——这一条把只读 LocalBasePath 的 154 对提到 176 对。
fn link_info_target(d: &[u8], start: usize) -> Result<String, LnkError> {
    let size = u32_at(d, start).ok_or(LnkError::Truncated)? as usize;
    if size < LI_MIN_HEADER {
        return Err(LnkError::Truncated);
    }
    let end = start.checked_add(size).ok_or(LnkError::Truncated)?;
    if end > d.len() {
        return Err(LnkError::Truncated);
    }
    let base = string_at_offset(d, start, end, LI_BASE_PATH).ok_or(LnkError::NoTarget)?;
    if !looks_absolute(&base) {
        return Err(LnkError::NoTarget);
    }
    let suffix = string_at_offset(d, start, end, LI_COMMON_SUFFIX);
    Ok(match suffix {
        Some(s) if !tail_eq(&base, &s) => format!("{base}{s}"),
        _ => base,
    })
}

/// `base` 是不是已经以 `suffix` 结尾（大小写不敏感，同 Windows 的路径习惯）。
fn tail_eq(base: &str, suffix: &str) -> bool {
    base.to_lowercase().ends_with(&suffix.to_lowercase())
}

/// 取 `LinkInfo` 里由 `field` 那个偏移字段所指的串。0 或坏偏移 = 没有这条。
///
/// 读到的范围上界是 `end`（= LinkInfo 块尾），所以偏移指到块外时两条读法都会
/// 拿到空串而返回 `None`——**不会**顺手去隔壁 StringData 里捞一条看着像路径的串。
fn string_at_offset(d: &[u8], li: usize, end: usize, field: usize) -> Option<String> {
    let off = u32_at(d, li.checked_add(field)?)? as usize;
    if off < LI_MIN_HEADER {
        return None; // 指向头部自己 = 坏件
    }
    let at = li.checked_add(off)?;
    if is_utf16_at(d, at) {
        read_nul_utf16(d, at, end)
    } else {
        read_nul_ansi(d, at, end)
    }
}

/// UTF-16LE 的签名：ASCII 字符后面紧跟 0x00（`C`,`0`,`:`,`0`）。
/// 本机 183 个带 LinkInfo 的真件里 LocalBasePath 全是 ANSI，这一档是给偏移指向 UTF-16 的件留的。
fn is_utf16_at(d: &[u8], at: usize) -> bool {
    d.get(at..at + 4)
        .is_some_and(|w| w[0] != 0 && w[1] == 0 && w[2] != 0 && w[3] == 0)
}

fn read_nul_utf16(d: &[u8], at: usize, end: usize) -> Option<String> {
    let (pairs, _) = d.get(at..end)?.as_chunks::<2>();
    let units: Vec<u16> = pairs
        .iter()
        .map(|b| u16::from_le_bytes(*b))
        .take_while(|&u| u != 0)
        .collect();
    if units.is_empty() {
        return None;
    }
    String::from_utf16(&units).ok()
}

/// ANSI/OEM 版：高位字节认不出码页（GBK、cp1252、OEM 都可能），所以只接受纯
/// ASCII 的，否则宁可判"没有这条"，也不硬凑一条指向不存在文件的路径。
/// 🔴 已知边界：带中文的 LocalBasePath 因此报 NoTarget（本机 183 个带 LinkInfo 的真件里 0 个）。
fn read_nul_ansi(d: &[u8], at: usize, end: usize) -> Option<String> {
    let bytes: Vec<u8> = d
        .get(at..end)?
        .iter()
        .copied()
        .take_while(|&b| b != 0)
        .collect();
    if bytes.is_empty() || !bytes.iter().all(|b| b.is_ascii()) {
        return None;
    }
    String::from_utf8(bytes).ok()
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

    /// 造一个 LinkInfo 块，形状照本机真件：28 字节头 + 21 字节 VolumeID，
    /// 串从 +49 开始。`suffix` 为空时不写、偏移字段留 0。
    /// `uni` 为真时两条串按 UTF-16LE 存（真件都是 ANSI，这条测的是编码签名分支）。
    ///
    /// 🔴 这里的字段位置**故意写字面量 16/24，不引用 `LI_BASE_PATH`/`LI_COMMON_SUFFIX`**：
    /// 夹具跟被测代码共用常数的话，把常数改错（比如按常见转述把基路径放到 +12）
    /// 夹具会跟着一起改，测试照样绿——那条错法就永远判不红。实测这轮就有这么一次：
    /// 改成 +20 之后所有测试仍然全绿。
    fn link_info(base: &str, suffix: &str, uni: bool) -> Vec<u8> {
        let enc = |s: &str| -> Vec<u8> {
            if uni {
                let mut v = wide(s);
                v.extend_from_slice(&[0, 0]);
                v
            } else {
                let mut v = s.as_bytes().to_vec();
                v.push(0);
                v
            }
        };
        let base_bytes = enc(base);
        let suffix_bytes = enc(suffix);
        let base_at = 49usize;
        let suffix_at = if suffix.is_empty() {
            0
        } else {
            base_at + base_bytes.len()
        };
        let size = base_at
            + base_bytes.len()
            + if suffix.is_empty() {
                0
            } else {
                suffix_bytes.len()
            };
        let mut d = vec![0u8; base_at];
        d[0..4].copy_from_slice(&(size as u32).to_le_bytes());
        d[4..8].copy_from_slice(&28u32.to_le_bytes()); // Version
        d[8..12].copy_from_slice(&1u32.to_le_bytes()); // LinkInfoFlags
        d[12..16].copy_from_slice(&28u32.to_le_bytes()); // LinkInfoHeaderSize
        d[16..20].copy_from_slice(&(base_at as u32).to_le_bytes()); // LocalBasePathOffset
        d[24..28].copy_from_slice(&(suffix_at as u32).to_le_bytes()); // CommonPathSuffixOffset
        d.extend_from_slice(&base_bytes);
        if !suffix.is_empty() {
            d.extend_from_slice(&suffix_bytes);
        }
        d
    }

    /// 头 + 可选 IDList + 可选 LinkInfo。IDList 只要长度对得上，内容不参与目标复原。
    fn wrap(flags: u32, idlist: &[u8], link_info: Option<&[u8]>) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&76u32.to_le_bytes());
        d.extend_from_slice(&CLSID);
        d.extend_from_slice(&flags.to_le_bytes());
        d.resize(HDR_SIZE, 0);
        if flags & FLAG_IDLIST != 0 {
            d.extend_from_slice(&(idlist.len() as u16).to_le_bytes());
            d.extend_from_slice(idlist);
        }
        if let Some(li) = link_info {
            d.extend_from_slice(li);
        }
        d
    }

    const WITH_LI: u32 = FLAG_LINKINFO;

    #[test]
    fn reads_local_base_path_from_link_info() {
        let li = link_info(r"D:\Apps\UniGetUI\UniGetUI.exe", "", false);
        let d = wrap(WITH_LI, &[], Some(&li));
        assert_eq!(target_of(&d).unwrap(), r"D:\Apps\UniGetUI\UniGetUI.exe");
    }

    /// 带 IDList 的常见形状：IDList 只被跳过，不参与拼接。
    #[test]
    fn skips_a_real_sized_idlist_before_link_info() {
        let li = link_info(r"C:\Tools\a.exe", "", false);
        let d = wrap(FLAG_IDLIST | WITH_LI, &[0x00, 0x40, 1, 2, 3, 4], Some(&li));
        assert_eq!(target_of(&d).unwrap(), r"C:\Tools\a.exe");
    }

    /// 🔴 主缺陷回归：`C:\Users\` 这种只存了前缀的 LocalBasePath，要靠
    /// CommonPathSuffix 补全。真件形状来自桌面 `GitHub Desktop.lnk`（oracle 给的正是
    /// 拼完的那条）；用户名已换成 `dev`，不往公共仓里写本机路径。
    #[test]
    fn appends_common_path_suffix_when_base_is_only_a_prefix() {
        let li = link_info(
            r"C:\Users\",
            r"dev\AppData\Local\GitHubDesktop\GitHubDesktop.exe",
            false,
        );
        let d = wrap(WITH_LI, &[], Some(&li));
        assert_eq!(
            target_of(&d).unwrap(),
            r"C:\Users\dev\AppData\Local\GitHubDesktop\GitHubDesktop.exe"
        );
    }

    /// 反向的一半：LocalBasePath 已经含文件名、后缀只是它末尾的 `exe` 时，
    /// 不能再拼一遍。真件：`Blender 4.2.lnk`。
    #[test]
    fn does_not_double_append_when_suffix_is_already_the_tail() {
        let li = link_info(
            r"C:\Program Files\Blender Foundation\Blender 4.2\blender-launcher.exe",
            "exe",
            false,
        );
        let d = wrap(WITH_LI, &[], Some(&li));
        assert_eq!(
            target_of(&d).unwrap(),
            r"C:\Program Files\Blender Foundation\Blender 4.2\blender-launcher.exe"
        );
    }

    #[test]
    fn accepts_unc_target() {
        let li = link_info(r"\\srv\share\a.exe", "", false);
        let d = wrap(WITH_LI, &[], Some(&li));
        assert_eq!(target_of(&d).unwrap(), r"\\srv\share\a.exe");
    }

    #[test]
    fn reads_a_utf16_local_base_path() {
        let li = link_info(r"E:\工具\包管理器\a.exe", "", true);
        let d = wrap(WITH_LI, &[], Some(&li));
        assert_eq!(target_of(&d).unwrap(), r"E:\工具\包管理器\a.exe");
    }

    /// 没有 LinkInfo ⇒ 明说没有目标。这是**量过之后的范围决定**：那 50 个真件
    /// 只有 IDList/相对路径，靠 RelativePath + 所在目录只能多复原 7 条，且多数是
    /// `%windir%` 这类要环境变量展开的系统入口，不值得为它留整套 StringData 解析。
    #[test]
    fn no_linkinfo_is_reported_not_guessed() {
        let d = wrap(FLAG_IDLIST, &[0x00, 0x40, 1, 2], None);
        assert_eq!(target_of(&d), Err(LnkError::NoTarget));
    }

    /// 只有相对路径的 LocalBasePath 不当绝对目标返回。
    #[test]
    fn relative_local_base_path_is_not_an_answer() {
        let li = link_info(r"Sub\dir\a.exe", "", false);
        let d = wrap(WITH_LI, &[], Some(&li));
        assert_eq!(target_of(&d), Err(LnkError::NoTarget));
    }

    /// 偏移指回头部自己（或 0）= 坏件，不能拿头部字节当路径读出来。
    #[test]
    fn offset_inside_the_header_is_refused() {
        let mut li = link_info(r"C:\Tools\a.exe", "", false);
        li[16..20].copy_from_slice(&4u32.to_le_bytes());
        let d = wrap(WITH_LI, &[], Some(&li));
        assert_eq!(target_of(&d), Err(LnkError::NoTarget));
    }

    /// 偏移越过 LinkInfo 块尾 = 坏件；**不去隔壁 StringData 里捞**。
    #[test]
    fn offset_beyond_the_block_is_refused() {
        let mut li = link_info(r"C:\Tools\a.exe", "", false);
        let over = (li.len() + 100) as u32;
        li[16..20].copy_from_slice(&over.to_le_bytes());
        let d = wrap(WITH_LI, &[], Some(&li));
        assert_eq!(target_of(&d), Err(LnkError::NoTarget));
    }

    /// 🔴 这条把「字段顺序」钉在**Windows 自己写出来的字节**上，而不是我造的夹具上：
    /// 取自 `C:\ProgramData\Microsoft\Windows\Start Menu\Programs\Node.js\
    /// Node.js command prompt.lnk` 的 78 字节 LinkInfo 块，与本机无关（不读盘），
    /// 所以 CI 上也能判红。唯一改动是把 +28..+49 的 VolumeID 整段抹平（那是这台
    /// 机器的卷序列号与卷标，跟解析无关）。目标 `C:\Windows\System32\cmd.exe`。
    /// 它的 CommonPathSuffixOffset 是 77、那块字节为 0 ⇒ 空后缀：拼一遍就错了。
    #[test]
    fn parses_a_real_windows_written_link_info_block() {
        let li: [u8; 78] = [
            78, 0, 0, 0, 28, 0, 0, 0, 1, 0, 0, 0, 28, 0, 0, 0, 49, 0, 0, 0, 0, 0, 0, 0, 77, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 67, 58, 92, 87, 105,
            110, 100, 111, 119, 115, 92, 83, 121, 115, 116, 101, 109, 51, 50, 92, 99, 109, 100, 46,
            101, 120, 101, 0, 0,
        ];
        let d = wrap(WITH_LI, &[], Some(&li));
        assert_eq!(target_of(&d).unwrap(), r"C:\Windows\System32\cmd.exe");
    }

    /// 声明的 LinkInfoSize 连自己的偏移字段都装不下 ⇒ 判截断，
    /// 不能退回去把别的字节当路径读。（后面补够了真实字节，所以不是"越界"判红，
    /// 而是这条"小到装不下头"的判据本身在判。）
    #[test]
    fn linkinfo_smaller_than_its_own_header_is_truncated() {
        let mut d = Vec::new();
        d.extend_from_slice(&76u32.to_le_bytes());
        d.extend_from_slice(&CLSID);
        d.extend_from_slice(&FLAG_LINKINFO.to_le_bytes());
        d.resize(HDR_SIZE, 0);
        d.extend_from_slice(&8u32.to_le_bytes()); // 声明 8 字节的 LinkInfo
        d.extend_from_slice(&[0u8; 64]); // 但文件里管够
        assert_eq!(target_of(&d), Err(LnkError::Truncated));
    }

    /// 已知边界的回归：ANSI 段里的高位字节认不出码页（GBK/cp1252/OEM 都可能），
    /// 宁可报"没有目标"，也不硬凑一条看着像路径的乱码。
    #[test]
    fn non_ascii_local_base_path_is_refused_not_guessed() {
        // "D:\工具\a.exe" 的 GBK 字节——本机真件不会有这种形状（183 个里 0 个），
        // 这是把"我们主动不猜"这条范围钉住，不是复现某个真件。
        let gbk: [u8; 15] = [
            0x44, 0x3a, 0x5c, 0xb9, 0xa4, 0xbe, 0xdf, 0x5c, 0x61, 0x2e, 0x65, 0x78, 0x65, 0x00,
            0x00,
        ];
        let mut li = vec![0u8; 49];
        let size = 49 + gbk.len();
        li[0..4].copy_from_slice(&(size as u32).to_le_bytes());
        li[4..8].copy_from_slice(&28u32.to_le_bytes());
        li[8..12].copy_from_slice(&1u32.to_le_bytes());
        li[12..16].copy_from_slice(&28u32.to_le_bytes());
        li[16..20].copy_from_slice(&49u32.to_le_bytes());
        li.extend_from_slice(&gbk);
        let d = wrap(WITH_LI, &[], Some(&li));
        assert_eq!(target_of(&d), Err(LnkError::NoTarget));
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

    /// 声明了超长/超短 LinkInfo 的坏文件必须判错，不能 panic 也不能越界读。
    #[test]
    fn truncated_declared_sizes_are_errors_not_panics() {
        let mut d = vec![0u8; HDR_SIZE + 6];
        d[0..4].copy_from_slice(&76u32.to_le_bytes());
        d[4..20].copy_from_slice(&CLSID);
        d[20..24].copy_from_slice(&FLAG_LINKINFO.to_le_bytes());
        d[76..80].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(target_of(&d).err(), Some(LnkError::Truncated));

        let mut small = vec![0u8; HDR_SIZE + 6];
        small[0..4].copy_from_slice(&76u32.to_le_bytes());
        small[4..20].copy_from_slice(&CLSID);
        small[20..24].copy_from_slice(&FLAG_LINKINFO.to_le_bytes());
        small[76..80].copy_from_slice(&8u32.to_le_bytes()); // 比头还小的 LinkInfo
        assert_eq!(target_of(&small).err(), Some(LnkError::Truncated));

        // 声明了超长 IDList 的坏文件同理
        let mut d2 = vec![0u8; HDR_SIZE + 2];
        d2[0..4].copy_from_slice(&76u32.to_le_bytes());
        d2[4..20].copy_from_slice(&CLSID);
        d2[20..24].copy_from_slice(&FLAG_IDLIST.to_le_bytes());
        d2[76..78].copy_from_slice(&1000u16.to_le_bytes());
        assert_eq!(target_of(&d2).err(), Some(LnkError::Truncated));
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
