//! SFTP 状态码 → `fmnr_core::Error` 的**纯**映射。
//!
//! 这条映射是远程卷最容易出错、也最值得钉死的地方：分类错了，MCP/HTTP 客户端
//! 收到的就是 500 而不是 400，「我把路径写错了」和「服务端炸了」分不开。
//! 本地卷已经栽过一次（`LocalVolume::list` 的 NotFound 曾映射成 Io → 500），
//! 所以这里用真状态码枚举驱动单测，不需要连机器。

use fmnr_core::Error;
use russh_sftp::protocol::StatusCode;

/// 把服务端返回的状态码映射成卷错误。
///
/// 原则：**调用方自己能修正的 → NotFound / Unsupported；修正不了的 → Other**。
/// `PermissionDenied` 归 `Other` 而不是 `NotFound`：把它报成「不存在」会诱导
/// 用户去改路径，而真正要做的是改权限或换账号。
///
/// `server_msg` 是服务端给的人类可读说明（`Status.error_message`），
/// 非空时附在错误里——远程路径的问题往往只有服务端知道细节。
pub fn map_status(code: StatusCode, path: &str, server_msg: &str) -> Error {
    let detail = if server_msg.trim().is_empty() {
        String::new()
    } else {
        format!("：{}", server_msg.trim())
    };
    match code {
        StatusCode::NoSuchFile => Error::NotFound(format!("{path}{detail}")),
        StatusCode::OpUnsupported => {
            Error::Unsupported(format!("服务端不支持该操作：{path}{detail}"))
        }
        StatusCode::PermissionDenied => Error::Other(format!("权限不足：{path}{detail}")),
        // 这两个是**伪状态码**，只由客户端本地生成（见协议注释），
        // 语义是「没连上 / 连丢了」——不是路径问题，必须走 Other 让上层重连。
        StatusCode::NoConnection | StatusCode::ConnectionLost => {
            Error::Other(format!("连接不可用：{code}（{path}）{detail}"))
        }
        StatusCode::BadMessage => Error::Other(format!("协议报文异常：{code}（{path}）{detail}")),
        StatusCode::Failure | StatusCode::Eof | StatusCode::Ok => {
            Error::Other(format!("服务端失败：{code}（{path}）{detail}"))
        }
    }
}

/// `Ok` 才是成功；`Eof` 在 readdir 结尾是正常信号，但**不是**「这步成功了」。
/// 分开判，免得把「读完了」显示成「出错了」，或反过来把 Eof 当 Ok 吞掉。
pub fn is_success(code: StatusCode) -> bool {
    matches!(code, StatusCode::Ok)
}

/// 值得让 UI 提示「可能要重连」的错误。
pub fn looks_like_transport_failure(err: &Error) -> bool {
    match err {
        Error::Other(msg) => msg.contains("连接不可用") || msg.contains("协议报文异常"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(code: StatusCode) -> Error {
        map_status(code, "/srv/x", "")
    }

    /// 路径类错误必须映射成 NotFound——这是客户端错误的口径（400/-32602）。
    #[test]
    fn missing_path_maps_to_not_found() {
        let e = map_status(StatusCode::NoSuchFile, "/srv/absent.txt", "No such file");
        assert!(
            matches!(e, Error::NotFound(_)),
            "NoSuchFile 应 NotFound，实得 {e:?}"
        );
        assert!(e.to_string().contains("/srv/absent.txt"), "错误要带上路径");
        assert!(
            e.to_string().contains("No such file"),
            "服务端原文要带出来：{e}"
        );
    }

    #[test]
    fn unsupported_maps_to_unsupported() {
        let e = m(StatusCode::OpUnsupported);
        assert!(matches!(e, Error::Unsupported(_)), "实得 {e:?}");
    }

    /// 权限不足**不能**报成 NotFound：那会把用户支去改路径，
    /// 而该做的是改权限或换账号。
    #[test]
    fn permission_denied_is_not_reported_as_missing() {
        let e = m(StatusCode::PermissionDenied);
        assert!(matches!(e, Error::Other(_)), "实得 {e:?}");
        assert!(!matches!(e, Error::NotFound(_)));
        assert!(e.to_string().contains("权限"), "文案要说清是权限问题：{e}");
    }

    /// 两个伪状态码是「连接层面的」，不是路径层面的：
    /// 报 NotFound 会让上层放弃重连。
    #[test]
    fn connection_pseudo_statuses_are_transport_failures() {
        for s in [StatusCode::NoConnection, StatusCode::ConnectionLost] {
            let e = m(s);
            assert!(matches!(e, Error::Other(_)), "{s} 应 Other，实得 {e:?}");
            assert!(
                looks_like_transport_failure(&e),
                "{s} 要能被认成传输故障以便重连：{e}"
            );
        }
        // 路径类错误不该被误认成传输故障（否则每次拼错路径都会触发重连风暴）。
        let e = m(StatusCode::NoSuchFile);
        assert!(
            !looks_like_transport_failure(&e),
            "NotFound 不该触发重连：{e}"
        );
    }

    #[test]
    fn eof_and_failure_are_not_silently_ok() {
        // Eof 走 read_dir 结尾是正常的，但若真当错误抛上来必须报出去而不是吞掉。
        for s in [StatusCode::Eof, StatusCode::Failure, StatusCode::Ok] {
            let e = m(s);
            assert!(matches!(e, Error::Other(_)), "{s} 应 Other：{e:?}");
        }
        assert!(is_success(StatusCode::Ok));
        assert!(!is_success(StatusCode::Eof), "Eof 不是成功，别当 Ok 吞掉");
    }

    /// 空白服务端消息不该在错误里留下一对空冒号。
    #[test]
    fn blank_server_message_does_not_add_noise() {
        for blank in ["", "   ", "\n"] {
            let e = map_status(StatusCode::NoSuchFile, "/p", blank);
            assert!(
                !e.to_string().ends_with('：'),
                "空白消息留下了悬空冒号：{e}"
            );
            assert!(!e.to_string().contains("： "), "不该有空的详情段：{e}");
        }
    }

    /// 分类必须**穷尽**：StatusCode 加了新变体时编译器要逼我们表态。
    #[test]
    fn every_status_code_is_classified() {
        let all = [
            StatusCode::Ok,
            StatusCode::Eof,
            StatusCode::NoSuchFile,
            StatusCode::PermissionDenied,
            StatusCode::Failure,
            StatusCode::BadMessage,
            StatusCode::NoConnection,
            StatusCode::ConnectionLost,
            StatusCode::OpUnsupported,
        ];
        for c in all {
            let e = map_status(c, "/p", "srv detail");
            let msg = e.to_string();
            assert!(!msg.is_empty(), "{c} 映射出空消息");
            assert!(msg.contains("/p"), "{c} 消息没带路径：{msg}");
            assert!(msg.contains("srv detail"), "{c} 丢了服务端原文：{msg}");
        }
        // 9 个变体全列出来：若上游加了第 10 个，这个数组长度就对不上。
        assert_eq!(
            all.len(),
            9,
            "StatusCode 变体数变了，回去检查 map_status 有没有漏臂"
        );
    }
}
