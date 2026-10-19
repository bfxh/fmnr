//! `LocalVolume::write_stream` 的门：跨卷搬运的落地那一头。
//!
//! 先记一条**量不出来的东西**，免得下一个人拿"更聪明的判据"再试一遍：
//! "边收边写在盘上是否立刻看得见"这条**当不了判据**——实测
//! `write_all(...).await` 已经返回成功之后，另开一个句柄去问大小仍报 **0 字节**，
//! 要先在**写句柄**上做一次 `stream_position()`（seek）才变得可见。
//! 我第一版就是照"另一个句柄看到的长度"写的门，红在尺子上而不是代码上（查了三轮）。
//!
//! 所以"不整份进内存"这句话钉在两个能确定量的地方：
//! - `ops::stream_to_writer` 的单测看**写入次数**（三块流必须 ≥3 次写）；
//! - 这里的门看**内容、空流边界，以及那道 #61 的闸**。

use fmnr_core::{VPath, Volume};
use fmnr_fs::LocalVolume;
use std::io;
use std::path::PathBuf;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fmnr-ws-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 多块流式写：内容逐字节相等，返回值等于实际写出的字节数。
#[tokio::test]
async fn many_chunks_land_byte_exact() {
    let d = tmpdir("exact");
    let vol = LocalVolume::new(d.clone());
    let target = VPath(d.join("big.bin"));
    let parts: Vec<Vec<u8>> = (0..6u8).map(|k| vec![k; 4096]).collect();
    let expect: Vec<u8> = parts.concat();

    let n = vol
        .write_stream(
            &target,
            Box::new(futures::stream::iter(
                parts.into_iter().map(Ok::<_, io::Error>),
            )),
        )
        .await
        .expect("流式写该成功");

    assert_eq!(n, expect.len() as u64, "返回值要等于写出的字节数");
    assert_eq!(std::fs::read(&target.0).unwrap(), expect);
    std::fs::remove_dir_all(&d).ok();
}

/// 空流也要把目标建成一个**空文件**并返回 0——0 字节文件是合法内容，
/// 不是"什么都没发生"；两者长得一样，所以必须钉住"文件真的建出来了"。
#[tokio::test]
async fn an_empty_stream_still_creates_an_empty_file() {
    let d = tmpdir("empty");
    let vol = LocalVolume::new(d.clone());
    let target = VPath(d.join("zero.bin"));
    let n = vol
        .write_stream(
            &target,
            Box::new(futures::stream::empty::<io::Result<Vec<u8>>>()),
        )
        .await
        .unwrap();
    assert_eq!(n, 0);
    assert!(
        target.0.exists(),
        "0 字节文件也是文件：必须真的在盘上，不能静默不建"
    );
    std::fs::remove_dir_all(&d).ok();
}

/// 🔴 这条新通路不许绕过 #61 那道闸：目标被别的程序开着写时必须**拒**。
/// 绕过去就退回"覆盖别人正开写的文件 → 静默 truncate 成 0 字节而调用方看到成功"。
#[cfg(windows)]
#[test]
fn streaming_write_refuses_a_file_another_writer_holds() {
    use std::io::Write;
    let rt = tokio::runtime::Runtime::new().unwrap();
    let d = tmpdir("busy");
    let target = d.join("busy.bin");

    // 对方以 std 的全共享掩码持有写句柄并已写 8 字节（正是实测里会被抹成 0 的那种）
    let mut peer = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&target)
        .unwrap();
    peer.write_all(b"peer-data!").unwrap();
    peer.flush().unwrap();

    let vol = LocalVolume::new(d.clone());
    let e = rt
        .block_on(vol.write_stream(
            &VPath(target.clone()),
            Box::new(futures::stream::iter(vec![Ok::<_, io::Error>(
                b"mine".to_vec(),
            )])),
        ))
        .expect_err("对方正开着写时流式写必须失败");
    assert!(
        e.to_string().contains("正被另一个程序"),
        "报错要说人话，拿到：{e}"
    );
    drop(peer);
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"peer-data!",
        "被拒之后对方写进去的字节一根都不该少"
    );

    // 对照组：没人占用时同一条流式写要成功（免得这条门空转）
    let n = rt
        .block_on(vol.write_stream(
            &VPath(target.clone()),
            Box::new(futures::stream::iter(vec![Ok::<_, io::Error>(
                b"mine".to_vec(),
            )])),
        ))
        .expect("没人占用时流式写也要能写");
    assert_eq!(n, 4);
    assert_eq!(std::fs::read(&target).unwrap(), b"mine");
    std::fs::remove_dir_all(&d).ok();
}
