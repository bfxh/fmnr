//! 常驻设置。`share.json` 只存四样：要不要自动开、要不要对局域网开、
//! 上次绑上的端口、以及那把 token。
//!
//! 为什么 token 要落盘：常驻的意义是"手机上的链接明天还能用"。每次启动换一把，
//! 书签就废了。代价写在界面上——链接一旦给出去，谁拿到谁能投，直到你换钥匙。
//!
//! 🔴 从文件读出来的 token **不可信**：手改过、被别的程序写坏、或者只有 3 个字符的，
//! 一律当没有、重新生成。弱 token 比没有更糟——它会让人以为"设过了"。

use std::path::Path;

/// 128 bit = 32 个小写十六进制字符。与 `new_token()` 的形状一致。
pub fn token_is_well_formed(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prefs {
    /// 开着：下次启动自动开收件口；关着：不自动开。
    pub enabled: bool,
    /// 绑 `127.0.0.1` 还是本机的局域网 IPv4。
    pub lan: bool,
    /// 上次真绑上的端口。0 = 还没开过（第一次由系统挑，挑完记下来）。
    /// 端口不固定，链接每次都不一样，"书签还能用"就不成立。
    pub port: u16,
    /// 那把 token。可能是空的/坏的（见 `token()`）。
    pub token: String,
}

impl Prefs {
    /// 读不到、读坏了、字段缺失 ⇒ **关着**（不是"猜一个"）。沙盒里没有配置文件是常态。
    pub fn load(path: &Path) -> Prefs {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Prefs::default();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Prefs::default();
        };
        Prefs {
            enabled: v["enabled"].as_bool().unwrap_or(false),
            lan: v["lan"].as_bool().unwrap_or(false),
            port: v["port"]
                .as_u64()
                .filter(|p| *p <= u16::MAX as u64)
                .unwrap_or(0) as u16,
            token: v["token"].as_str().unwrap_or("").to_string(),
        }
    }

    /// 先写临时文件再改名：写到一半断电/崩溃时，旧设置还在，不会留下半个 JSON。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("建不了配置目录：{e}"))?;
        }
        let json = serde_json::json!({
            "enabled": self.enabled,
            "lan": self.lan,
            "port": self.port,
            "token": self.token,
        });
        let text = serde_json::to_string_pretty(&json).map_err(|e| format!("拼不出设置：{e}"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("写不了设置：{e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("替换不掉旧设置：{e}"))
    }

    /// 能用的那把 token：形状不对就当没有（**绝不拿坏 token 起服务**）。
    pub fn token(&self) -> Option<&str> {
        token_is_well_formed(&self.token).then_some(self.token.as_str())
    }

    /// 换一把新钥匙。旧的那把立刻作废（服务重启后旧链接就 401 了）。
    pub fn rotate(&mut self) -> String {
        self.token = crate::new_token();
        self.token.clone()
    }

    /// 要开服务时调：没有可用的就现生成一把写回 `token`。返回可用的那把。
    pub fn ensure_token(&mut self) -> String {
        if let Some(t) = self.token() {
            return t.to_string();
        }
        self.token = crate::new_token();
        self.token.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fmnr-share-prefs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn prefs_survive_a_round_trip() {
        let dir = tmp("round");
        let p = dir.join("share.json");
        let want = Prefs {
            enabled: true,
            lan: true,
            port: 54321,
            token: "0123456789abcdef0123456789abcdef".into(),
        };
        want.save(&p).unwrap();
        assert_eq!(Prefs::load(&p), want, "存下去和读回来的不是同一份");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 没有文件 ⇒ 关着。沙盒/新机器上这是常态，不许崩、也不许默认开着。
    #[test]
    fn a_missing_file_means_off() {
        assert_eq!(
            Prefs::load(&tmp("missing").join("nope.json")),
            Prefs::default()
        );
    }

    #[test]
    fn a_corrupt_file_means_off_not_panic() {
        let dir = tmp("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("share.json");
        std::fs::write(&p, b"{ this is not json").unwrap();
        assert_eq!(Prefs::load(&p), Prefs::default(), "坏文件要当没设过");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 🔴 手改出来的短 token 不许被当成钥匙：它会让人以为"设过了"，其实谁都能猜。
    #[test]
    fn a_hand_edited_short_token_is_not_trusted() {
        let dir = tmp("short");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("share.json");
        std::fs::write(&p, br#"{"enabled":true,"lan":true,"port":1,"token":"abc"}"#).unwrap();
        let mut got = Prefs::load(&p);
        assert_eq!(got.token(), None, "3 个字符的 token 居然被当成了钥匙");
        let fresh = got.ensure_token();
        assert_eq!(fresh.len(), 32, "该现生成一把 128 bit 的");
        assert_eq!(got.token(), Some(fresh.as_str()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn token_shape_is_exactly_128_bits_of_lowercase_hex() {
        for bad in [
            "",
            "0123456789abcdef0123456789abcde",   // 31
            "0123456789abcdef0123456789abcdef0", // 33
            "0123456789ABCDEF0123456789ABCDEF",  // 大写
            "0123456789abcdef0123456789abcdeg",  // 非十六进制
            "0123456789abcdef0123456789abcde案", // 非 ASCII
        ] {
            assert!(!token_is_well_formed(bad), "{bad:?} 不该被当成 token");
        }
        assert!(token_is_well_formed(&crate::new_token()));
    }
}
