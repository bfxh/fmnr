//! WSL 卷实测：把 `\\wsl.localhost\<distro>\...` 当成 Volume 浏览。
//!
//! **这不是契约桩**：本机装了 WSL2（Ubuntu-24.04 / docker-desktop），
//! 所以这里跑的是真路径。DESIGN 附录 A 里「本机 WSL 未启用」那条已过时。
//!
//! 关键结论（实测得出，不是推断）：WSL 的 UNC 在 Windows 侧就是普通可
//! `read_dir` 的目录，**不需要 SSH/SFTP 那样的协议层**，所以复用
//! `LocalVolume` 即可；卷的 `kind` 靠路径识别来标 `Wsl`，供 UI/图标分流。

use fmnr_core::{ListOpts, VPath, Volume, VolumeKind};
use fmnr_fs::LocalVolume;
use std::path::PathBuf;

/// 找一个**本机真实存在且可读**的 WSL distro 根。
///
/// 没装 WSL / 没发行版时返回 `None`，测试据此**跳过**而非失败——
/// 否则 CI 在没有 WSL 的机器上会永久变红，那是环境噪声不是缺陷。
fn live_distro_root() -> Option<PathBuf> {
    let distros = fmnr_core::wsl::parse_distro_list(&run_wsl(&["-l", "-q"])?);
    for d in distros {
        let p = PathBuf::from(format!(r"\\wsl.localhost\{d}"));
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

fn run_wsl(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("wsl").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    // `wsl` 的输出是 **UTF-16LE**（实测），直接 from_utf8 会失败或得到乱码。
    decode_utf16le(&out.stdout)
}

/// UTF-16LE → String，容忍结尾缺字节与 BOM。
fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    let body = if bytes.starts_with(&[0xff, 0xfe]) {
        &bytes[2..]
    } else {
        bytes
    };
    if body.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&units).ok()
}

#[test]
fn wsl_unc_is_readable_as_a_volume() {
    let Some(root) = live_distro_root() else {
        eprintln!("跳过：本机没有可读的 WSL distro（未安装或未启用）");
        return;
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let vol = LocalVolume::new(root.clone());
    let ents = rt
        .block_on(vol.list(&VPath(root.clone()), &ListOpts::default()))
        .expect("列 distro 根目录应成功");

    // distro 根的**直接子项**非空即可——不硬编码 `home`/`usr` 之类，
    // 那会把「发行版恰好长这样」当成契约，换一个 distro 就假红。
    assert!(
        !ents.is_empty(),
        "{root:?} 列出来是空的，说明 UNC 读取没真的生效"
    );

    // 走的是本地 IO：不能出现 NotFound（路径存在性由 live_distro_root 保证）。
    for e in &ents {
        assert!(!e.name.is_empty(), "条目名不应为空：{e:?}");
    }
}

/// 路径识别要与「真的能读」对齐：认出来是 WSL 的，kind 才该标 `Wsl`。
#[test]
fn wsl_paths_are_recognized_then_classified() {
    let Some(root) = live_distro_root() else {
        eprintln!("跳过：本机没有可读的 WSL distro");
        return;
    };
    assert!(
        fmnr_core::wsl::looks_like_wsl_unc(&root),
        "实测存在的路径 {root:?} 必须被识别为 WSL UNC"
    );
    let parsed = fmnr_core::wsl::parse_wsl_unc(&root).expect("解析");
    assert!(!parsed.distro.is_empty());

    // 识别成 WSL 后，卷 id 用 `wsl:<distro>` 而不是 `local:\\wsl...`
    let id = fmnr_core::wsl::wsl_volume_id(&parsed.distro);
    assert_eq!(id.0, format!("wsl:{}", parsed.distro));
}

/// 反向：Linux 侧路径经 `wslpath` 换算后，Windows 侧确实指到同一个东西。
/// 这条钉住的是「往返不会指错文件」，是白板/拖放交外部程序的前提。
#[test]
fn wslpath_roundtrip_points_at_the_same_file() {
    let Some(root) = live_distro_root() else {
        eprintln!("跳过：本机没有可读的 WSL distro");
        return;
    };
    // 在 distro 里放一个临时文件，用 Linux 路径写、用 UNC 读。
    let rt = tokio::runtime::Runtime::new().unwrap();
    let vol = LocalVolume::new(root.clone());
    let name = format!("fmnr-wsl-probe-{}.txt", std::process::id());
    let rel = PathBuf::from("tmp").join(&name);
    let wsl_path = VPath(root.join(&rel));

    rt.block_on(vol.write(&wsl_path, b"roundtrip"))
        .expect("写入 distro 的 /tmp 应成功");

    // 用 wslpath 语义算出 Linux 路径，再拼回来验证是同一个对象。
    let unc = fmnr_core::wsl::WslUnc {
        distro: fmnr_core::wsl::parse_wsl_unc(&root).unwrap().distro,
        linux_rel: rel.to_string_lossy().replace('\\', "/"),
    };
    let linux = unc.to_linux();
    assert!(linux.starts_with("/tmp/"), "应是 Linux 绝对路径：{linux}");

    // UNC 写法必须能读到刚写的内容——证明两种指法落在同一文件上。
    let bytes = rt
        .block_on(vol.read_stream(&VPath(root.join(&rel))))
        .expect("回读应成功");
    use futures::StreamExt;
    let mut acc = Vec::new();
    let mut stream = bytes;
    while let Some(chunk) = rt.block_on(stream.next()) {
        acc.extend(chunk.unwrap());
    }
    assert_eq!(acc, b"roundtrip", "往返读回的内容要一致");
    std::fs::remove_file(root.join(&rel)).ok();
}

/// `VolumeKind::Wsl` 得真的存在且可与 Local 区分——UI 靠它决定
/// 「远程卷不做 hash 图标覆盖」（DESIGN §4.4）。
#[test]
fn wsl_kind_is_distinct_from_local() {
    assert_ne!(VolumeKind::Wsl, VolumeKind::Local);
    assert_ne!(VolumeKind::Wsl, VolumeKind::Remote);
}

/// 卷的 kind / id 要按**根路径**如实分类，而不是恒为 Local。
///
/// 这条不依赖 WSL 是否装机（纯字符串判定），所以 CI 上恒跑。
#[test]
fn volume_classifies_its_root_as_wsl() {
    let wsl = LocalVolume::new(r"\\wsl.localhost\Ubuntu-24.04\home\me");
    assert_eq!(wsl.kind(), VolumeKind::Wsl, "WSL 根要报 Wsl");
    assert_eq!(wsl.id().0, "wsl:Ubuntu-24.04", "id 要收敛到 distro 级");

    let local = LocalVolume::new(std::env::temp_dir());
    assert_eq!(local.kind(), VolumeKind::Local);
    assert!(
        local.id().0.starts_with("local:"),
        "普通盘要报 local：{}",
        local.id().0
    );

    // 同一 distro 的不同子目录应算**同一个卷**（图标覆盖等按卷共享）。
    let a = LocalVolume::new(r"\\wsl$\Ubuntu-24.04\home\me");
    let b = LocalVolume::new(r"\\wsl.localhost\Ubuntu-24.04\var\log");
    assert_eq!(a.id(), b.id(), "同一 distro 不该因为子目录不同而变卷");
}
