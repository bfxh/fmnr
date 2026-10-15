//! 远程卷的**真连接**验收测试。
//!
//! 本机没有可用 SSH 主机，所以这些默认**跳过**（打印原因后通过），
//! 不让 CI 因环境缺东西而永久变红——与 `fmnr-fs/tests/wsl_live.rs` 同一策略。
//!
//! 有主机时这样打开：
//! ```text
//! FMNR_SSH_TEST_HOST=ssh://user@host:22/tmp \
//! FMNR_SSH_TEST_PASSWORD=... cargo test -p fmnr-remote --test ssh_live -- --nocapture
//! ```
//! 没设环境变量时它们**什么都没验证**，别把全绿当成远程卷可用。

use fmnr_core::{ListOpts, VPath, Volume};
use fmnr_remote::{AuthMethod, HostKeyPolicy, SshConfig, SshVolume};
use std::path::PathBuf;

/// 读夹具配置；缺环境变量就返回 `None`（调用方据此跳过）。
fn fixture() -> Option<SshConfig> {
    let endpoint = std::env::var("FMNR_SSH_TEST_HOST").ok()?;
    let endpoint = fmnr_remote::config::parse_endpoint(&endpoint)
        .unwrap_or_else(|e| panic!("FMNR_SSH_TEST_HOST 格式不对（{endpoint}）：{e}"));
    let auth = match std::env::var("FMNR_SSH_TEST_PASSWORD") {
        Ok(p) => AuthMethod::Password(p),
        Err(_) => AuthMethod::KeyFile {
            path: std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".ssh/id_ed25519"),
            passphrase: None,
        },
    };
    Some(SshConfig {
        endpoint,
        auth,
        // 测试夹具走默认（校验）策略：如果连这都放行，说明 known_hosts
        // 配好了；AcceptAny 留给使用者显式改，别在测试里悄悄放宽。
        host_key: HostKeyPolicy::default(),
    })
}

macro_rules! need_fixture {
    () => {
        match fixture() {
            Some(c) => c,
            None => {
                eprintln!("跳过：未设 FMNR_SSH_TEST_HOST（远程卷无真实主机可连）");
                return;
            }
        }
    };
}

#[tokio::test]
async fn lists_a_remote_directory() {
    let cfg = need_fixture!();
    let path = cfg.endpoint.path.clone();
    let vol = SshVolume::new(cfg);
    let ents = vol
        .list(&VPath(PathBuf::from(&path)), &ListOpts::default())
        .await
        .expect("列远端目录应成功");
    // 不硬编码内容：只要求列得动、且没有 "." / ".." 漏出来。
    for e in &ents {
        assert!(!e.name.eq("."), "不该列出 . ：{e:?}");
        assert!(!e.name.eq(".."), "不该列出 .. ：{e:?}");
        assert!(!e.name.is_empty(), "条目名为空：{e:?}");
    }
}

#[tokio::test]
async fn writes_then_reads_back_the_same_bytes() {
    let cfg = need_fixture!();
    let base = cfg.endpoint.path.clone();
    let vol = SshVolume::new(cfg);
    let rel = format!("fmnr-ssh-probe-{}.txt", std::process::id());
    let p = VPath(PathBuf::from(if base.is_empty() {
        rel.clone()
    } else {
        format!("{base}/{rel}")
    }));

    vol.write(&p, b"roundtrip").await.expect("写入远端");
    let stream = vol.read_stream(&p).await.expect("回读");
    use futures::StreamExt;
    let mut acc = Vec::new();
    let mut stream = stream;
    while let Some(chunk) = stream.next().await {
        acc.extend(chunk.expect("块读取"));
    }
    assert_eq!(acc, b"roundtrip", "远端往返内容要一致");

    vol.delete(
        std::slice::from_ref(&p),
        std::sync::Arc::new(fmnr_core::Progress::new()),
    )
    .await
    .expect("清理远端夹具");
    // 删掉后再读必须是 NotFound，而不是「空文件」——静默空内容最危险。
    // （Box<dyn Stream> 不实现 Debug，所以不能用 unwrap_err。）
    match vol.read_stream(&p).await {
        Ok(_) => panic!("删除后仍能读到远端文件，delete 没生效"),
        Err(e) => assert!(
            matches!(e, fmnr_core::Error::NotFound(_)),
            "删除后应 NotFound，实得 {e:?}"
        ),
    }
}

/// 未知主机在默认策略下**连不上**。这条即使没有可用主机也能跑：
/// 指向一个不存在的主机名即可，所以它不设环境变量也执行。
#[tokio::test]
async fn unknown_host_fails_closed_even_without_fixture() {
    let cfg = SshConfig {
        endpoint: fmnr_remote::config::parse_endpoint(
            "ssh://nobody@fmnr-nonexistent-host.invalid/tmp",
        )
        .unwrap(),
        auth: AuthMethod::Password("irrelevant".into()),
        host_key: HostKeyPolicy::default(),
    };
    let vol = SshVolume::new(cfg);
    let e = vol
        .list(&VPath(PathBuf::from("tmp")), &ListOpts::default())
        .await
        .expect_err("不存在的远端主机不该列成功");
    // 具体文案取决于 DNS/网络，这里只要求：**是错误**，且不是 NotFound 之外的假成功。
    assert!(!matches!(e, fmnr_core::Error::Unsupported(_)));
}
