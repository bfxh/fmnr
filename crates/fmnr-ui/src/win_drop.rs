//! 接管窗口的 OLE 拖放：**为了收到文本/URL**。
//!
//! 背景：`egui-winit` 只把 `CF_HDROP`（文件路径）转成 `DroppedFile`，
//! `CF_UNICODETEXT` / `text/uri-list` 根本没接。要在 fmnr 里"拖一个链接进来"，
//! 只能在窗口上自己注册一个 `IDropTarget` —— 而一个 HWND 同时只能有一个，
//! 所以必须先 `RevokeDragDrop` 把 winit 那个换掉，文件和文本都由我们转给 UI。
//!
//! 三条纪律：
//! 1. **装不上就退回**：任何一步失败都返回 false，winit 的拖放继续可用（A 档不受影响）。
//! 2. **逃生开关**：设 `FMNR_NO_OLE_DROP=1` 就完全不接管。
//! 3. 回调里**只做拷贝**，不碰 UI、不 panic 跨 FFI（`catch_unwind` 兜底）。

use std::path::PathBuf;
use std::sync::OnceLock;

/// 一次拖放里我们认得的两种载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))] // Files 只有 Windows 的 OLE 路径会构造
pub enum DropItem {
    Files(Vec<PathBuf>),
    /// 文本或 URI-list（浏览器拖链接时通常两样都给，优先取 uri-list）。
    Text(String),
}

fn queue() -> &'static std::sync::Mutex<Vec<DropItem>> {
    static Q: OnceLock<std::sync::Mutex<Vec<DropItem>>> = OnceLock::new();
    Q.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// 取走自上次以来收到的拖放载荷。
pub fn take() -> Vec<DropItem> {
    match queue().lock() {
        Ok(mut q) => std::mem::take(&mut *q),
        Err(_) => Vec::new(),
    }
}

#[cfg(not(windows))]
pub unsafe fn install(_hwnd: *mut core::ffi::c_void) -> bool {
    false
}

#[cfg(windows)]
mod imp {
    use super::{queue, DropItem};
    use std::path::PathBuf;
    use std::sync::OnceLock;

    use windows::core::{implement, Ref, Result as WinResult};
    use windows::Win32::Foundation::{HWND, POINTL};
    use windows::Win32::System::Com::{
        IDataObject, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, TYMED_HGLOBAL,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::{
        IDropTarget, IDropTarget_Impl, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
        DROPEFFECT, DROPEFFECT_COPY,
    };
    use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    /// 标准剪贴板格式（Win32 ABI 常量，几十年没变）。
    const CF_UNICODETEXT: u16 = 13;
    const CF_HDROP: u16 = 15;

    /// 逃生开关：想回到 winit 原生拖放（只收文件）时设 `FMNR_NO_OLE_DROP=1`。
    fn enabled_by_env() -> bool {
        std::env::var_os("FMNR_NO_OLE_DROP").is_none()
    }

    fn push_all(items: Vec<DropItem>) {
        if items.is_empty() {
            return;
        }
        if let Ok(mut q) = queue().lock() {
            q.extend(items);
        }
    }

    #[implement(IDropTarget)]
    struct Target;

    fn set_copy(pdweffect: *mut DROPEFFECT) {
        if !pdweffect.is_null() {
            unsafe { *pdweffect = DROPEFFECT_COPY };
        }
    }

    fn fet(cf: u16) -> FORMATETC {
        FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        }
    }

    fn uri_list_format() -> u16 {
        static F: OnceLock<u16> = OnceLock::new();
        *F.get_or_init(|| unsafe {
            windows::Win32::System::DataExchange::RegisterClipboardFormatW(windows::core::w!(
                "text/uri-list"
            )) as u16
        })
    }

    /// 从 HGLOBAL 取 UTF-16 文本（以 NUL 结尾），拷贝完立刻释放。
    unsafe fn text_from_medium(med: &mut STGMEDIUM) -> Option<String> {
        let h = med.u.hGlobal;
        if h.is_invalid() {
            return None;
        }
        let p = GlobalLock(h);
        if p.is_null() {
            return None;
        }
        let ptr = p as *const u16;
        let mut len = 0usize;
        // 上限 1 MiB 字符：拖进来的文本不该是内存炸弹。
        while len < 1_048_576 && *ptr.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
        let _ = GlobalUnlock(h);
        Some(s)
    }

    unsafe fn get_text(obj: &IDataObject, cf: u16) -> Option<String> {
        if cf == 0 {
            return None;
        }
        let fe = fet(cf);
        if obj.QueryGetData(&fe).ok().is_err() {
            return None;
        }
        let mut med = obj.GetData(&fe).ok()?;
        let out = text_from_medium(&mut med);
        ReleaseStgMedium(&mut med);
        out
    }

    unsafe fn get_files(obj: &IDataObject) -> Option<Vec<PathBuf>> {
        let fe = fet(CF_HDROP);
        if obj.QueryGetData(&fe).ok().is_err() {
            return None;
        }
        let mut med = obj.GetData(&fe).ok()?;
        let drop_h = HDROP(med.u.hGlobal.0);
        let mut out = Vec::new();
        // 0xFFFFFFFF = 问数量
        let count = DragQueryFileW(drop_h, u32::MAX, None);
        for i in 0..count.min(1024) {
            let len = DragQueryFileW(drop_h, i, None) as usize;
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len + 1];
            let got = DragQueryFileW(drop_h, i, Some(&mut buf));
            if got > 0 {
                out.push(PathBuf::from(String::from_utf16_lossy(
                    &buf[..got as usize],
                )));
            }
        }
        ReleaseStgMedium(&mut med);
        (!out.is_empty()).then_some(out)
    }

    /// 优先 uri-list（浏览器拖链接时给的是干净的一行 URL），其次纯文本，最后文件。
    unsafe fn read(obj: &IDataObject) -> Vec<DropItem> {
        if let Some(t) = get_text(obj, uri_list_format()) {
            if !t.trim().is_empty() {
                return vec![DropItem::Text(t)];
            }
        }
        if let Some(t) = get_text(obj, CF_UNICODETEXT) {
            if !t.trim().is_empty() {
                return vec![DropItem::Text(t)];
            }
        }
        get_files(obj)
            .map(|v| vec![DropItem::Files(v)])
            .unwrap_or_default()
    }

    impl IDropTarget_Impl for Target_Impl {
        fn DragEnter(
            &self,
            _pdataobj: Ref<IDataObject>,
            _grfkeystate: MODIFIERKEYS_FLAGS,
            _pt: &POINTL,
            pdweffect: *mut DROPEFFECT,
        ) -> WinResult<()> {
            set_copy(pdweffect);
            Ok(())
        }

        fn DragOver(
            &self,
            _grfkeystate: MODIFIERKEYS_FLAGS,
            _pt: &POINTL,
            pdweffect: *mut DROPEFFECT,
        ) -> WinResult<()> {
            set_copy(pdweffect);
            Ok(())
        }

        fn DragLeave(&self) -> WinResult<()> {
            Ok(())
        }

        fn Drop(
            &self,
            pdataobj: Ref<IDataObject>,
            _grfkeystate: MODIFIERKEYS_FLAGS,
            _pt: &POINTL,
            pdweffect: *mut DROPEFFECT,
        ) -> WinResult<()> {
            set_copy(pdweffect);
            let Some(obj) = pdataobj.as_ref() else {
                return Ok(());
            };
            // 不 panic 跨 FFI：拿数据的每一步都可能撞上坏 IDataObject。
            let items =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { read(obj) }))
                    .unwrap_or_default();
            push_all(items);
            Ok(())
        }
    }

    /// 接管成功返回 true；否则 false（调用方什么都不用做，winit 那条照旧）。
    pub unsafe fn install(hwnd: *mut core::ffi::c_void) -> bool {
        if !enabled_by_env() {
            return false;
        }
        let target: IDropTarget = Target.into();
        let h = HWND(hwnd);
        // winit 已经注册过；一个 HWND 只能有一个 drop target，先摘掉它的。
        let _ = RevokeDragDrop(h);
        if RegisterDragDrop(h, &target).is_err() {
            // 注册失败就把目标丢掉；此时 Win32 上没有 drop target，
            // egui 也不会再收到文件拖放——所以这一步失败要**说清楚**（见 install 的调用点）。
            return false;
        }
        // 故意泄漏这一个对象：COM 那边还握着它的引用，必须在进程存活期间一直有效。
        let _ = Box::leak(Box::new(target));
        true
    }
}

#[cfg(windows)]
pub unsafe fn install(hwnd: *mut core::ffi::c_void) -> bool {
    imp::install(hwnd)
}

/// 拖进来的文本该怎么处理。**纯函数**，因为拖拽没法在 headless 里注入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DroppedText {
    /// 一条能交给路径栏/卷路由的地址（本地路径或 `local:`/`wsl:`/`ssh:`/`archive:`）。
    Target(String),
    /// 网页链接：本程序不开网页，只把链接说出来。
    Web(String),
    /// 空文本或只有注释（uri-list 允许 `#` 开头的注释行）。
    Nothing,
}

pub(crate) fn classify_text(raw: &str) -> DroppedText {
    let Some(line) = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
    else {
        return DroppedText::Nothing;
    };
    let lower = line.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return DroppedText::Web(line.to_string());
    }
    // file:///C:/x → C:/x；file://server/share → //server/share
    if let Some(rest) = line
        .strip_prefix("file://")
        .or_else(|| line.strip_prefix("FILE://"))
    {
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        // file:///C:/x 去掉第一个 '/' 后是 "C:/x"（盘符形式直接可用）；
        // file://server/share 去掉后是 "server/share"，要补回 UNC 的 `\\`。
        let fixed = if rest.len() >= 2 && rest.as_bytes()[1] == b':' {
            rest.to_string()
        } else {
            format!("//{rest}")
        };
        return DroppedText::Target(fixed);
    }
    DroppedText::Target(line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_comments_and_blank_lines_are_skipped() {
        let raw = "# 从浏览器拖来的\n\n  https://example.com/a?b=1  \nhttps://second\n";
        assert_eq!(
            classify_text(raw),
            DroppedText::Web("https://example.com/a?b=1".into())
        );
        assert_eq!(classify_text("   \n#only comment\n"), DroppedText::Nothing);
    }

    #[test]
    fn file_urls_become_paths_including_unc() {
        assert_eq!(
            classify_text("file:///C:/Users/x/a%20b.txt"),
            DroppedText::Target("C:/Users/x/a%20b.txt".into())
        );
        assert_eq!(
            classify_text("file://server/share/x.txt"),
            DroppedText::Target("//server/share/x.txt".into())
        );
    }

    #[test]
    fn our_own_volume_ids_pass_through_untouched() {
        for raw in ["local:E:\\x", "wsl:Ubuntu", "ssh:host", "archive:C:\\a.zip"] {
            assert_eq!(
                classify_text(raw),
                DroppedText::Target(raw.to_string()),
                "协议串要原样交给路径栏"
            );
        }
        assert_eq!(
            classify_text("D:\\KF\\fmnr"),
            DroppedText::Target("D:\\KF\\fmnr".into())
        );
    }
}
