//! WebDAV：把收件目录挂成手机上的一个盘。
//!
//! 为什么是 WebDAV 而不是再发明一条私协议：安卓上能挂盘的文件管理器大多说它，
//! 而且**它是双向的**——手机既能往里丢，也能从里拿，不用回浏览器点链接。
//!
//! 边界（与 HTTP 投递那条腿一致，别读成"开放了整台机器"）：
//! - 只暴露 `Hub::inbox` 这一个目录，路径逐段过 `sanitize_name`，`..`/盘符/分隔符一律拒。
//! - 鉴权仍是那把 token，走 **Basic**（用户名随便填，密码必须是 token）。明文 HTTP
//!   ⇒ Basic 只是"不裸奔"，不是加密；这是自家网档位，与 DESIGN 那条口径一致。
//! - 同名的语义与页面那条腿**不同**：这里是盘，PUT 到已存在的名字 = **覆盖**；
//!   页面那条腿是"丢件"，同名让路。两条都写清在界面上。
//!
//! 🔴 覆盖走"写临时件 → 改名"而不是"直接截断目标"：截断后写到一半失败，
//! 旧件就没了；改名则在改名那一刻之前，旧件一直完好。

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::Hub;

/// 一次 PROPFIND 最多列多少个孩子。盘上十万个文件时，客户端也只吃第一页。
const MAX_CHILDREN: usize = 2000;

/// 逐段校验后的相对路径。空 = 根。
pub fn segments(raw: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for seg in raw.split('/') {
        if seg.is_empty() {
            continue;
        }
        let decoded = percent_decode(seg)?;
        // 与全仓其他入口同一条规则：`sanitize_name` 认什么，这里才认什么。
        // 要求"原样返回"是关键——它一旦压平（去掉 `/`、盘符、尾随点），就说明这段
        // 不是一个合法单段名，必须拒，不能拿压平后的结果去落盘。
        if crate::sanitize_name(&decoded).as_deref() != Some(decoded.as_str()) {
            return None;
        }
        out.push(decoded);
        if out.len() > 16 {
            return None;
        }
    }
    Some(out)
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = b.get(i + 1..i + 3)?;
            let v = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Basic 认证。用户名随便填——很多客户端只给一个"密码"框，会把 token 填进用户名，
/// 所以两边都认。也接受 `?k=`，方便 curl 手测。
pub fn authorized(hub: &Hub, headers: &HeaderMap, query: &HashMap<String, String>) -> bool {
    if let Some(k) = query.get("k") {
        if crate::token_ok(&hub.token, Some(k)) {
            return true;
        }
    }
    let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(b64) = v
        .strip_prefix("Basic ")
        .or_else(|| v.strip_prefix("basic "))
    else {
        return false;
    };
    use base64::Engine;
    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
        return false;
    };
    let Ok(text) = String::from_utf8(raw) else {
        return false;
    };
    match text.split_once(':') {
        Some((u, p)) => p == hub.token || u == hub.token,
        None => text == hub.token,
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"fmnr\"")],
        "需要登录：用户名随便填，密码是链接里那串 token。",
    )
        .into_response()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn href_of(segs: &[String], is_dir: bool) -> String {
    let mut s = String::from("/dav");
    for seg in segs {
        s.push('/');
        s.push_str(&crate::url_escape(seg));
    }
    if is_dir && !s.ends_with('/') {
        s.push('/');
    }
    s
}

fn depth_of(headers: &HeaderMap) -> u8 {
    match headers.get("depth").and_then(|v| v.to_str().ok()) {
        Some("0") => 0,
        Some("infinity") => 2,
        _ => 1,
    }
}

fn entry_xml(segs: &[String], md: &std::fs::Metadata) -> String {
    let is_dir = md.is_dir();
    let mut prop = format!(
        "<D:displayname>{}</D:displayname>",
        xml_escape(segs.last().map(String::as_str).unwrap_or(""),)
    );
    if is_dir {
        prop.push_str("<D:resourcetype><D:collection/></D:resourcetype>");
    } else {
        prop.push_str("<D:resourcetype/>");
        prop.push_str(&format!(
            "<D:getcontentlength>{}</D:getcontentlength>",
            md.len()
        ));
    }
    format!(
        "<D:response><D:href>{}</D:href><D:propstat><D:prop>{prop}</D:prop>\
         <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
        xml_escape(&href_of(segs, is_dir))
    )
}

fn multistatus(inner: String) -> Response {
    (
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\">{inner}</D:multistatus>"),
    )
        .into_response()
}

/// 从 `If:` 里掏出所有尖括号里的锁令牌。够用即可：客户端只会写成
/// `If: (<urn:uuid:...>)` 或 `If: <http://host/path> (<urn:uuid:...>)`。
fn if_tokens(headers: &HeaderMap) -> Vec<String> {
    let mut out = Vec::new();
    for v in headers.get_all("if").iter().filter_map(|v| v.to_str().ok()) {
        let mut rest = v;
        while let Some(a) = rest.find('<') {
            let after = &rest[a + 1..];
            let Some(b) = after.find('>') else { break };
            let tok = &after[..b];
            if tok.starts_with("urn:uuid:") {
                out.push(tok.to_string());
            }
            rest = &after[b + 1..];
        }
    }
    out
}

fn lock_token_header(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("lock-token")?.to_str().ok()?.trim();
    let t = raw.trim_start_matches('<').trim_end_matches('>');
    (!t.is_empty()).then(|| t.to_string())
}

/// 租期：`Timeout: Second-N` / `Infinite`。默认 15 分钟，最长 8 小时。
fn timeout_secs(headers: &HeaderMap) -> u64 {
    let raw = headers
        .get("timeout")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    for part in raw.split(',') {
        let part = part.trim();
        if part.eq_ignore_ascii_case("infinite") {
            return 8 * 3600;
        }
        if let Some(n) = part.strip_prefix("Second-") {
            if let Ok(v) = n.parse::<u64>() {
                return v.clamp(1, 8 * 3600);
            }
        }
    }
    900
}

fn locked(hub: &Hub, key: &str, headers: &HeaderMap) -> Option<Response> {
    hub.write_blocked_by(key, &if_tokens(headers)).map(|_| {
        (
            StatusCode::LOCKED,
            "这个资源被另一个客户端锁着：先 UNLOCK，或者带着匹配的 If: 锁令牌再来。",
        )
            .into_response()
    })
}

fn lock_response(token: &str, segs: &[String], is_dir: bool) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:prop xmlns:D=\"DAV:\">         <D:lockdiscovery><D:activelock><D:locktype><D:write/></D:locktype>         <D:lockscope><D:exclusive/></D:lockscope>         <D:depth>{}</D:depth>         <D:locktoken><D:href>{}</D:href></D:locktoken>         <D:lockroot><D:href>{}</D:href></D:lockroot>         </D:activelock></D:lockdiscovery></D:prop>",
        if is_dir { "infinity" } else { "0" },
        xml_escape(token),
        xml_escape(&href_of(segs, is_dir))
    );
    let mut resp = (StatusCode::OK, body).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/xml; charset=utf-8".parse().unwrap(),
    );
    resp.headers_mut().insert(
        header::HeaderName::from_static("lock-token"),
        format!("<{token}>").parse().unwrap(),
    );
    resp
}

fn lock_handler(hub: &Hub, segs: &[String], key: &str, headers: &HeaderMap) -> Response {
    let secs = timeout_secs(headers);
    // 续租：If: 里那把正好是这条路径的锁 ⇒ 直接续，不再申请新锁。
    for t in if_tokens(headers) {
        if hub.refresh_lock(key, &t, secs) {
            return lock_response(&t, segs, false);
        }
    }
    let depth_inf = depth_of(headers) != 0; // LOCK 的默认深度是 infinity
    match hub.lock_path(key, depth_inf, secs) {
        Ok(tok) => lock_response(&tok, segs, depth_inf),
        Err(_owner) => (
            StatusCode::LOCKED,
            "这条路径已经被别人锁着（持有者没解锁或租期未到）。",
        )
            .into_response(),
    }
}

/// `/dav` 这一面的总入口。方法分派在函数体内，因为它们是同一棵树上的同一批路径。
pub async fn handle(State(hub): State<Arc<Hub>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let rel = path.strip_prefix("/dav").unwrap_or("");
    let query: HashMap<String, String> = req
        .uri()
        .query()
        .map(|q| {
            q.split('&')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if !authorized(&hub, req.headers(), &query) {
        return unauthorized();
    }
    let Some(segs) = segments(rel) else {
        return (
            StatusCode::FORBIDDEN,
            "这个路径里有不合法的段（`..`、分隔符、盘符或控制字符）。",
        )
            .into_response();
    };
    let root = hub.inbox().to_path_buf();
    let target = segs.iter().fold(root.clone(), |p, s| p.join(s));

    // `http::Method` 没有 WebDAV 那批常量（PROPFIND/MKCOL/MOVE…），按字符串分派。
    let is_head = req.method() == Method::HEAD;
    // 锁表按"相对路径"记（空串 = 根）。
    let key = segs.join("/");
    match req.method().as_str() {
        "OPTIONS" => (
            StatusCode::OK,
            [
                (header::ALLOW.as_str(), "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, PROPPATCH, MKCOL, MOVE, LOCK, UNLOCK"),
                ("DAV", "1, 2"),
                ("MS-Author-Via", "DAV"),
            ],
        )
            .into_response(),
        "PROPFIND" => propfind(&target, &segs, req.headers()).await,
        "GET" | "HEAD" => get_file(&target, is_head).await,
        "PUT" => {
            if let Some(r) = locked(&hub, &key, req.headers()) {
                r
            } else {
                put_file(hub, segs, req).await
            }
        }
        "DELETE" => {
            if let Some(r) = locked(&hub, &key, req.headers()) {
                r
            } else {
                delete_file(&target, &root).await
            }
        }
        "MKCOL" => {
            if let Some(r) = locked(&hub, &key, req.headers()) {
                r
            } else {
                make_collection(&target).await
            }
        }
        "MOVE" => {
            if let Some(r) = locked(&hub, &key, req.headers()) {
                r
            } else {
                move_entry(&target, &root, req.headers(), &hub).await
            }
        }
        // 锁定是"盘"客户端的习惯动作。我们不做真锁（单人自家网），但必须**回一个
        // 像样的 lock-token**，否则有的客户端会拒绝进入写模式。
        "LOCK" => lock_handler(&hub, &segs, &key, req.headers()),
        "UNLOCK" => {
            let Some(tok) = lock_token_header(req.headers()) else {
                return (StatusCode::BAD_REQUEST, "UNLOCK 要带 Lock-Token 头。").into_response();
            };
            if hub.unlock_path(&key, &tok) {
                StatusCode::NO_CONTENT.into_response()
            } else {
                (StatusCode::CONFLICT, "这把锁不是你的（或已经过期）。").into_response()
            }
        }
        // 客户端改"文件说明"之类：我们不存这些属性，但要说"收到了、没生效"，
        // 否则有的客户端会把 501 当成整个挂载不可写。
        "PROPPATCH" => multistatus(
            "<D:response><D:href>/dav/</D:href><D:propstat><D:prop/><D:status>HTTP/1.1 403 Forbidden</D:status></D:propstat></D:response>".into(),
        ),
        _ => (StatusCode::METHOD_NOT_ALLOWED, "这个方法这一面不做。").into_response(),
    }
}

async fn propfind(target: &std::path::Path, segs: &[String], headers: &HeaderMap) -> Response {
    let Ok(md) = tokio::fs::metadata(target).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut body = entry_xml(segs, &md);
    if md.is_dir() && depth_of(headers) != 0 {
        let mut rd = match tokio::fs::read_dir(target).await {
            Ok(rd) => rd,
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("列不出来：{e}"))
                    .into_response()
            }
        };
        let mut n = 0usize;
        while let Ok(Some(ent)) = rd.next_entry().await {
            if n >= MAX_CHILDREN {
                break;
            }
            let name = ent.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue; // 临时件与隐藏件不给客户端看
            }
            if let Ok(m) = ent.metadata().await {
                let mut child = segs.to_vec();
                child.push(name);
                body.push_str(&entry_xml(&child, &m));
                n += 1;
            }
        }
    }
    multistatus(body)
}

async fn get_file(target: &std::path::Path, head_only: bool) -> Response {
    let Ok(md) = tokio::fs::metadata(target).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if md.is_dir() {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Ok(f) = std::fs::File::open(target) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let len = md.len();
    let builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, len);
    if head_only {
        return builder.body(Body::empty()).unwrap_or_else(|_| {
            (StatusCode::INTERNAL_SERVER_ERROR, "响应头拼不出来").into_response()
        });
    }
    let stream = tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(f));
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "响应头拼不出来").into_response())
}

async fn put_file(hub: Arc<Hub>, segs: Vec<String>, req: Request) -> Response {
    let Some(_name) = segs.last().cloned() else {
        return (StatusCode::METHOD_NOT_ALLOWED, "根上不能直接 PUT。").into_response();
    };
    if segs.len() > 1 {
        // 中间目录必须已经存在（盘客户端会自己先 MKCOL）。这里只落最后一段，
        // 但**不接受**写进一个不存在的目录——那会静默把件落到别处。
        let parent = segs[..segs.len() - 1]
            .iter()
            .fold(hub.inbox().to_path_buf(), |p, s| p.join(s));
        if !parent.is_dir() {
            return StatusCode::CONFLICT.into_response();
        }
    }
    let stream = req.into_body().into_data_stream();
    match crate::ingest_exact(hub, &segs, Box::pin(stream)).await {
        Ok(msg) => (StatusCode::CREATED, msg).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn delete_file(target: &std::path::Path, root: &std::path::Path) -> Response {
    if target == root {
        return (StatusCode::FORBIDDEN, "根目录不能删。").into_response();
    }
    let Ok(md) = tokio::fs::metadata(target).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let res = if md.is_dir() {
        tokio::fs::remove_dir(target).await
    } else {
        tokio::fs::remove_file(target).await
    };
    match res {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            format!("删不掉（目录非空或文件被占用？）：{e}"),
        )
            .into_response(),
    }
}

async fn make_collection(target: &std::path::Path) -> Response {
    if target.exists() {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    match tokio::fs::create_dir(target).await {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(e) => (StatusCode::CONFLICT, format!("建不出来：{e}")).into_response(),
    }
}

async fn move_entry(
    target: &std::path::Path,
    root: &std::path::Path,
    headers: &HeaderMap,
    hub: &Hub,
) -> Response {
    let Some(dest) = headers.get("destination").and_then(|v| v.to_str().ok()) else {
        return (StatusCode::BAD_REQUEST, "MOVE 要带 Destination 头。").into_response();
    };
    // Destination 可能是完整 URL（http://host:port/dav/x），只取路径部分。
    let dest_path = match dest.find("://") {
        Some(i) => {
            let after = &dest[i + 3..];
            match after.find('/') {
                Some(j) => &after[j..],
                None => "/",
            }
        }
        None => dest,
    };
    let Some(mut segs) = dest_path.strip_prefix("/dav").and_then(segments) else {
        return (StatusCode::FORBIDDEN, "Destination 不合法。").into_response();
    };
    let Some(name) = segs.pop() else {
        return (StatusCode::FORBIDDEN, "根上不能当 MOVE 目标。").into_response();
    };
    let dest_path = segs
        .iter()
        .fold(hub.inbox().to_path_buf(), |p, s| p.join(s))
        .join(name);
    if target == root || dest_path == root {
        return (StatusCode::FORBIDDEN, "根目录不能搬。").into_response();
    }
    match tokio::fs::rename(target, &dest_path).await {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(e) => (StatusCode::CONFLICT, format!("搬不动：{e}")).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use tower::ServiceExt;

    pub(super) fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("fmnr-dav-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub(super) fn req(method: &str, uri: &str, token: &str, body: &[u8]) -> Request {
        use base64::Engine;
        let cred = base64::engine::general_purpose::STANDARD.encode(format!("anyone:{token}"));
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Basic {cred}"))
            .body(Body::from(body.to_vec()))
            .unwrap()
    }

    pub(super) async fn call(hub: Arc<Hub>, r: Request) -> (StatusCode, String, HeaderMap) {
        let res = crate::router(hub).oneshot(r).await.unwrap();
        let code = res.status();
        let headers = res.headers().clone();
        let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (code, String::from_utf8_lossy(&bytes).to_string(), headers)
    }

    #[test]
    fn path_segments_reject_escapes_and_accept_chinese() {
        assert_eq!(segments("/").unwrap(), Vec::<String>::new());
        assert_eq!(segments("/a/b.txt").unwrap(), vec!["a", "b.txt"]);
        assert_eq!(segments("/手机 照片.jpg").unwrap(), vec!["手机 照片.jpg"]);
        for bad in [
            "/../x",
            "/a/../../x",
            "/C:/x",
            "/a%5Cb",
            "/a%00b",
            "/%2e%2e/x",
        ] {
            assert!(segments(bad).is_none(), "{bad} 不该被接受");
        }
    }

    #[test]
    fn basic_auth_accepts_the_token_in_either_field() {
        use base64::Engine;
        let hub = Hub::new(std::path::PathBuf::from("x"));
        let tok = hub.token().to_string();
        let mut h = HeaderMap::new();
        let enc = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        h.insert(
            header::AUTHORIZATION,
            format!("Basic {}", enc(&format!("u:{tok}")))
                .parse()
                .unwrap(),
        );
        assert!(authorized(&hub, &h, &HashMap::new()), "密码位给 token 要认");
        h.insert(
            header::AUTHORIZATION,
            format!("Basic {}", enc(&tok)).parse().unwrap(),
        );
        assert!(
            authorized(&hub, &h, &HashMap::new()),
            "只给 token（无冒号）也要认"
        );
        h.insert(
            header::AUTHORIZATION,
            format!("Basic {}", enc("u:wrong")).parse().unwrap(),
        );
        assert!(!authorized(&hub, &h, &HashMap::new()));
        assert!(!authorized(&hub, &HeaderMap::new(), &HashMap::new()));
        let q: HashMap<String, String> = [("k".to_string(), tok.clone())].into();
        assert!(
            authorized(&hub, &HeaderMap::new(), &q),
            "?k= 也要认（curl 手测用）"
        );
    }

    /// 没有凭据 ⇒ 401 + `WWW-Authenticate`（客户端靠它弹登录框）。
    #[tokio::test]
    async fn without_credentials_the_mount_challenges() {
        let hub = Arc::new(Hub::new(tmp("auth")));
        let r = Request::builder()
            .method("PROPFIND")
            .uri("/dav/")
            .body(Body::empty())
            .unwrap();
        let (code, _, headers) = call(hub, r).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert!(
            headers.contains_key(header::WWW_AUTHENTICATE),
            "要给客户端一个登录挑战"
        );
    }

    /// 🔴 挂盘的 PUT 是**覆盖**语义：同一个名字再写一次，内容要变、且不能多出 `(2)`。
    #[tokio::test]
    async fn a_put_overwrites_in_place_instead_of_renaming() {
        let inbox = tmp("put");
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        let (code, _, _) = call(hub.clone(), req("PUT", "/dav/a.txt", &tok, b"first")).await;
        assert_eq!(code, StatusCode::CREATED);
        let (code2, _, _) = call(hub.clone(), req("PUT", "/dav/a.txt", &tok, b"second!")).await;
        assert_eq!(code2, StatusCode::CREATED);
        assert_eq!(
            std::fs::read(inbox.join("a.txt")).ok().as_deref(),
            Some(&b"second!"[..]),
            "盘上的 PUT 必须覆盖同一个名字"
        );
        let mut names: Vec<String> = std::fs::read_dir(&inbox)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["a.txt".to_string()],
            "不该多出 (2) 或临时件：{names:?}"
        );
        std::fs::remove_dir_all(&inbox).ok();
    }

    #[tokio::test]
    async fn get_head_and_propfind_describe_the_same_file() {
        let inbox = tmp("get");
        std::fs::write(inbox.join("有 空格.txt"), b"hello dav").unwrap();
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();

        let (code, body, _) = call(
            hub.clone(),
            req("GET", "/dav/%E6%9C%89%20%E7%A9%BA%E6%A0%BC.txt", &tok, b""),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body, "hello dav");

        let (code, body, headers) = call(
            hub.clone(),
            req("HEAD", "/dav/%E6%9C%89%20%E7%A9%BA%E6%A0%BC.txt", &tok, b""),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.is_empty(), "HEAD 不许带体");
        assert_eq!(headers.get(header::CONTENT_LENGTH).unwrap(), "9");

        let (code, xml, _) = call(hub, req("PROPFIND", "/dav/", &tok, b"")).await;
        assert_eq!(code, StatusCode::MULTI_STATUS);
        assert!(xml.contains("有 空格.txt"), "深度 1 要列出孩子：{xml}");
        assert!(xml.contains("getcontentlength"), "{xml}");
    }

    #[tokio::test]
    async fn depth_zero_lists_only_the_target() {
        let inbox = tmp("depth0");
        std::fs::write(inbox.join("child.txt"), b"x").unwrap();
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        let mut r = req("PROPFIND", "/dav/", &tok, b"");
        r.headers_mut().insert("depth", "0".parse().unwrap());
        let (code, xml, _) = call(hub, r).await;
        assert_eq!(code, StatusCode::MULTI_STATUS);
        assert!(!xml.contains("child.txt"), "depth:0 不该列孩子：{xml}");
        std::fs::remove_dir_all(&inbox).ok();
    }

    #[tokio::test]
    async fn delete_mkcol_and_move_behave_like_a_drive() {
        let inbox = tmp("ops");
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();

        let (code, _, _) = call(hub.clone(), req("MKCOL", "/dav/sub", &tok, b"")).await;
        assert_eq!(code, StatusCode::CREATED);
        assert!(inbox.join("sub").is_dir());
        let (code, _, _) = call(hub.clone(), req("MKCOL", "/dav/sub", &tok, b"")).await;
        assert_eq!(code, StatusCode::METHOD_NOT_ALLOWED, "已存在要拒");

        let (code, _, _) = call(hub.clone(), req("PUT", "/dav/sub/x.txt", &tok, b"1")).await;
        assert_eq!(code, StatusCode::CREATED);
        let (code, _, _) = call(hub.clone(), req("MOVE", "/dav/sub/x.txt", &tok, b"")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "没有 Destination 要明说");

        let mut r = req("MOVE", "/dav/sub/x.txt", &tok, b"");
        r.headers_mut().insert(
            "destination",
            "http://127.0.0.1:9/dav/sub/y.txt".parse().unwrap(),
        );
        let (code, _, _) = call(hub.clone(), r).await;
        assert_eq!(code, StatusCode::CREATED);
        assert!(
            inbox.join("sub").join("y.txt").exists(),
            "搬完要落在新名字上"
        );

        let (code, _, _) = call(hub.clone(), req("DELETE", "/dav/sub/y.txt", &tok, b"")).await;
        assert_eq!(code, StatusCode::NO_CONTENT);
        assert!(!inbox.join("sub").join("y.txt").exists());
        std::fs::remove_dir_all(&inbox).ok();
    }

    /// 路径穿越在这一面同样是**拒**，不是"压平后照收"：客户端拿到的是 403。
    #[tokio::test]
    async fn a_traversal_path_is_refused_not_flattened() {
        let inbox = tmp("trav");
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        let (code, _, _) = call(hub, req("PUT", "/dav/..%2Fevil.txt", &tok, b"x")).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert!(!inbox.join("evil.txt").exists());
        std::fs::remove_dir_all(&inbox).ok();
    }

    /// 临时件（`.part-*`）不是给客户端看的东西：PROPFIND 里不许出现。
    #[tokio::test]
    async fn staged_temp_files_are_hidden_from_listings() {
        let inbox = tmp("hidden");
        std::fs::write(inbox.join(".part-deadbeef"), b"x").unwrap();
        std::fs::write(inbox.join("real.txt"), b"y").unwrap();
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        let (code, xml, _) = call(hub, req("PROPFIND", "/dav/", &tok, b"")).await;
        assert_eq!(code, StatusCode::MULTI_STATUS);
        assert!(!xml.contains(".part-deadbeef"), "临时件漏出来了：{xml}");
        assert!(xml.contains("real.txt"));
        std::fs::remove_dir_all(&inbox).ok();
    }

    #[tokio::test]
    async fn options_advertises_the_dav_classes() {
        let hub = Arc::new(Hub::new(tmp("opt")));
        let tok = hub.token().to_string();
        let (code, _, headers) = call(hub, req("OPTIONS", "/dav/", &tok, b"")).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(headers.get("dav").unwrap(), "1, 2");
        let allow = headers.get(header::ALLOW).unwrap().to_str().unwrap();
        for m in ["PROPFIND", "PUT", "GET", "DELETE", "MKCOL", "MOVE"] {
            assert!(allow.contains(m), "Allow 里少了 {m}：{allow}");
        }
    }
}

#[cfg(test)]
mod lock_tests {
    use super::tests::{call, req, tmp};
    use super::*;

    fn req_if(method: &str, uri: &str, token: &str, body: &[u8], if_tok: Option<&str>) -> Request {
        let mut r = req(method, uri, token, body);
        if let Some(t) = if_tok {
            r.headers_mut()
                .insert("if", format!("(<{t}>)").parse().unwrap());
        }
        r
    }

    fn lock_token(headers: &HeaderMap) -> String {
        headers
            .get("lock-token")
            .unwrap()
            .to_str()
            .unwrap()
            .trim_start_matches('<')
            .trim_end_matches('>')
            .to_string()
    }

    /// 🔴 这是"真锁"的核心：别人锁着的路径，没有令牌写不进去；解锁之后又能写。
    #[tokio::test]
    async fn a_lock_blocks_other_writers_until_it_is_released() {
        let inbox = tmp("lock1");
        std::fs::write(inbox.join("a.txt"), b"orig").unwrap();
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();

        let (code, _, headers) = call(hub.clone(), req("LOCK", "/dav/a.txt", &tok, b"")).await;
        assert_eq!(code, StatusCode::OK, "LOCK 要成功");
        let lock = lock_token(&headers);

        // 没有 If: 的写 → 423
        let (code, _, _) = call(hub.clone(), req("PUT", "/dav/a.txt", &tok, b"clobber")).await;
        assert_eq!(code, StatusCode::LOCKED, "被别人锁着还能写 = 假锁");
        assert_eq!(
            std::fs::read(inbox.join("a.txt")).ok().as_deref(),
            Some(&b"orig"[..]),
            "被拒的写不许动到盘上的旧件"
        );

        // 第二个客户端也锁不上
        let (code, _, _) = call(hub.clone(), req("LOCK", "/dav/a.txt", &tok, b"")).await;
        assert_eq!(code, StatusCode::LOCKED, "同一路径不许两把锁");

        // 带令牌的写 → 放行
        let (code, _, _) = call(
            hub.clone(),
            req_if("PUT", "/dav/a.txt", &tok, b"clobber", Some(&lock)),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED);
        assert_eq!(
            std::fs::read(inbox.join("a.txt")).ok().as_deref(),
            Some(&b"clobber"[..])
        );

        // 错误的 UNLOCK 不许生效
        let mut r = req("UNLOCK", "/dav/a.txt", &tok, b"");
        r.headers_mut()
            .insert("lock-token", "<urn:uuid:not-mine>".parse().unwrap());
        let (code, _, _) = call(hub.clone(), r).await;
        assert_eq!(code, StatusCode::CONFLICT, "别人猜个 token 就想解锁？");

        // 正确的 UNLOCK → 之后不带令牌也能写
        let mut r = req("UNLOCK", "/dav/a.txt", &tok, b"");
        r.headers_mut()
            .insert("lock-token", format!("<{lock}>").parse().unwrap());
        let (code, _, _) = call(hub.clone(), r).await;
        assert_eq!(code, StatusCode::NO_CONTENT);
        let (code, _, _) = call(hub.clone(), req("PUT", "/dav/a.txt", &tok, b"after")).await;
        assert_eq!(code, StatusCode::CREATED, "解锁后应该能写");
        assert_eq!(
            std::fs::read(inbox.join("a.txt")).ok().as_deref(),
            Some(&b"after"[..])
        );
        assert_eq!(hub.live_lock_count(), 0);
        std::fs::remove_dir_all(&inbox).ok();
    }

    /// 目录锁要罩住孩子，但不能误伤兄弟。
    #[tokio::test]
    async fn a_collection_lock_covers_children_but_not_siblings() {
        let inbox = tmp("lock2");
        std::fs::create_dir_all(inbox.join("sub")).unwrap();
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();

        let mut r = req("LOCK", "/dav/sub", &tok, b"");
        r.headers_mut().insert("depth", "infinity".parse().unwrap());
        let (code, _, headers) = call(hub.clone(), r).await;
        assert_eq!(code, StatusCode::OK);
        let lock = lock_token(&headers);

        let (code, _, _) = call(hub.clone(), req("PUT", "/dav/sub/x.txt", &tok, b"1")).await;
        assert_eq!(code, StatusCode::LOCKED, "目录锁没罩住孩子 = 假锁");
        let (code, _, _) = call(
            hub.clone(),
            req_if("PUT", "/dav/sub/x.txt", &tok, b"1", Some(&lock)),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "持有人自己写要放行");

        let (code, _, _) = call(hub.clone(), req("PUT", "/dav/sibling.txt", &tok, b"2")).await;
        assert_eq!(code, StatusCode::CREATED, "兄弟路径不该被目录锁罩住");

        // 祖先的无限锁要让孩子**连锁都加不上**——否则会出现"锁拿到了但写不进去"
        // 这种自相矛盾的状态（L3 那把刀就是因为缺这条才活下来的）。
        let (code, _, _) = call(hub.clone(), req("LOCK", "/dav/sub/x.txt", &tok, b"")).await;
        assert_eq!(code, StatusCode::LOCKED, "祖先的无限锁没挡住孩子加锁");
        std::fs::remove_dir_all(&inbox).ok();
    }

    /// 客户端常常先锁一个还不存在的路径再 PUT（lock-null）。这条必须能用。
    #[tokio::test]
    async fn a_lock_on_a_missing_file_allows_the_following_put() {
        let inbox = tmp("lock3");
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        let (code, _, headers) = call(hub.clone(), req("LOCK", "/dav/new.txt", &tok, b"")).await;
        assert_eq!(code, StatusCode::OK, "lock-null 要允许");
        let lock = lock_token(&headers);
        let (code, _, _) = call(
            hub.clone(),
            req_if("PUT", "/dav/new.txt", &tok, b"new", Some(&lock)),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED);
        assert!(inbox.join("new.txt").exists());
        std::fs::remove_dir_all(&inbox).ok();
    }

    /// 续租：带 If: 的 LOCK 不换 token，也不该被判"重复加锁"。
    #[tokio::test]
    async fn locking_again_with_the_same_token_refreshes_instead_of_conflicting() {
        let inbox = tmp("lock4");
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        let (_, _, headers) = call(hub.clone(), req("LOCK", "/dav/r.txt", &tok, b"")).await;
        let lock = lock_token(&headers);
        let (code, _, headers2) = call(
            hub.clone(),
            req_if("LOCK", "/dav/r.txt", &tok, b"", Some(&lock)),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "续租不该 423");
        assert_eq!(lock_token(&headers2), lock, "续租要沿用同一把 token");
        std::fs::remove_dir_all(&inbox).ok();
    }
}
