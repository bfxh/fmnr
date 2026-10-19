//! 「能不能对外暴露」这条政策的**唯一一格**。
//!
//! 原先它住在 `fmnr-ai::http::bind_addr`。现在局域网投递（`fmnr-share`）也要开端口，
//! 同一句政策写两份一定会有一份先过期（本仓为这条坑返工过好几轮），所以搬到中立的
//! `fmnr-core` 让两边共用：**默认只许回环，非回环必须调用方显式批准**。

/// 把 `host:port` 拼成可绑的地址串，并守住"非回环要显式批准"这条闸。
///
/// `allow_remote=false` 时只放行 `127.0.0.1` / `localhost` / `::1` / `[::1]`；
/// 其余一律 `Err`，错误文案要说清**为什么**拒以及**怎么显式放行**——
/// 只回一个"拒绝"会让人以为程序坏了。
pub fn bind_addr(host: &str, port: u16, allow_remote: bool) -> Result<String, String> {
    let loopback = host == "127.0.0.1" || host == "::1" || host == "localhost" || host == "[::1]";
    if !loopback && !allow_remote {
        return Err(format!(
            "拒绝绑定非回环地址 {host}：这个端口能被局域网里的其他机器访问。\
             确认要对外开放请显式加 --allow-remote（界面里对应「允许局域网」那一勾）。"
        ));
    }
    // IPv6 字面量要加方括号才能拼进 host:port
    let h = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok(format!("{h}:{port}"))
}

/// 本机在局域网里被别的小机器看到的那个 IPv4。
///
/// 做法是"对一个外部地址 `connect` 一个 UDP 套接字"——UDP 的 connect 只查路由表、
/// **不发一个包**，所以离线也不会卡、不会真的往 8.8.8.8 写东西；查完取本地地址。
/// 拿不到（没有可用路由）回 `None`，调用方要如实说"没找到局域网地址"，
/// 不许退化成 `127.0.0.1` 然后告诉用户"手机打开这个链接"——那台手机打不开。
pub fn lan_ipv4() -> Option<std::net::IpAddr> {
    let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("8.8.8.8:80").ok()?;
    match probe.local_addr().ok()? {
        std::net::SocketAddr::V4(v4) if !v4.ip().is_loopback() => {
            Some(std::net::IpAddr::V4(*v4.ip()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{bind_addr, lan_ipv4};

    #[test]
    fn loopback_hosts_are_always_allowed() {
        assert!(bind_addr("127.0.0.1", 8787, false).is_ok());
        assert!(bind_addr("localhost", 8787, false).is_ok());
        assert!(bind_addr("::1", 8787, false).is_ok());
    }

    /// 非回环默认必须判红，而且要说清"怎么显式放行"——这句是给用户的，不是给日志的。
    #[test]
    fn non_loopback_needs_explicit_approval_and_says_how() {
        let e = bind_addr("0.0.0.0", 8787, false).unwrap_err();
        assert!(e.contains("--allow-remote"), "得给出放行办法：{e}");
        assert!(bind_addr("192.168.1.10", 8787, false).is_err());
        assert!(bind_addr("0.0.0.0", 8787, true).is_ok());
    }

    #[test]
    fn ipv6_literals_get_bracketed_once() {
        assert_eq!(bind_addr("::1", 8787, false).unwrap(), "[::1]:8787");
        assert_eq!(bind_addr("[::1]", 8787, false).unwrap(), "[::1]:8787");
        assert_eq!(bind_addr("127.0.0.1", 80, false).unwrap(), "127.0.0.1:80");
    }

    /// 这台机器有没有局域网地址是环境事实，不能当断言；这里只钉"回 None 时不许回环"。
    #[test]
    fn lan_probe_never_answers_with_loopback() {
        if let Some(ip) = lan_ipv4() {
            assert!(!ip.is_loopback(), "局域网地址不能是回环：{ip}");
            assert!(ip.is_ipv4(), "本函数只承诺 IPv4：{ip}");
        }
    }
}
