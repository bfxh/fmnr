//! SSH/SFTP 远程卷。
//!
//! 🔴 **实测状态：未连过任何真实主机。** 本机没有可用 SSH 目标，
//! 用户 2026-10-16 明确选择「只做接口 + 无主机时自动跳过的测试」。
//! 因此下面这些是**已验证**的：端点解析、凭据不泄露、host key 策略默认值、
//! SFTP 状态码 → 错误分类的映射（纯函数，用真状态码枚举驱动）。
//! 这些是**未验证**的：真的握手、认证成功率、大文件流式读、断连恢复。
//! 别把测试全绿读成「远程卷能用了」。
//!
//! 依赖选型：`russh` 关掉 default features、显式取 `ring`。
//! 默认的 `aws-lc-rs` 要 CMake+NASM，本机 windows-gnu 编不动（实测过依赖树可编）。

pub mod config;
pub mod status;
pub mod volume;

pub use config::{AuthMethod, HostKeyPolicy, SshConfig, SshEndpoint};
pub use volume::SshVolume;
