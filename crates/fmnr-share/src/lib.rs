//! 局域网投递：手机 ↔ 电脑之间**单向丢文件**。
//!
//! 边界先写死，免得下一个人把这条读成"跨设备同步做完了"：
//!
//! - 一次一个动作。手机把件投进电脑的收件目录，或电脑把**你手动登记过**的几个件
//!   取回手机。没有目录镜像、没有冲突合并、没有离线队列 ⇒ 不碰
//!   `docs/DESIGN.md` 那句「不做文件内容同步」。
//! - 收件有**两条腿**：`/put`（请求体就是文件，流式）和 `/send`（原生 `<form>` 的
//!   `multipart/form-data`，**不需要 JS**）。两条腿必须都进 `ingest`，
//!   新通道自己 `inbox.join(name)` 就是在绕开 `#61` 那道闸。
//! - 鉴权只有一把挂在 URL 里的随机 token（128 bit），**明文 HTTP**。
//!   同网段抓包就能拿到它，所以这是"自己家/办公室网"档位，不是公网服务。
//!   默认绑回环，对外开放要显式批准（闸在 `fmnr_core::net::bind_addr`，
//!   与 `fmnr-ai` 的 HTTP 面共用同一格）。
//! - 电脑侧只暴露登记过的件：`/get` **只认编号，不认路径**。这条是这一格里
//!   最要紧的不变量——一旦允许调用方给路径，手机就成了整台机器的读取口。

pub mod config;
pub mod dav;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::header;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Router;
use futures::StreamExt;
use tokio::io::AsyncWriteExt;

/// 单个上传件的大小上限。手机拍一段 4K 视频能到几 GB，
/// 没有天花板等于让对面把这台机器的盘写满。
pub const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// 无脚本那条腿（`/send`）的整单上限。它**整块进内存**再切，所以不能沿用
/// `DEFAULT_MAX_BYTES`——那等于允许对面一句 POST 让本机缓冲 2 GiB。
/// 这条腿是给"壳子浏览器禁了 fetch"时送文档/图片兜底的，大文件走 `/put`（流式）。
pub const SEND_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// 一个登记给手机取的件。`id` 是它在这份清单里的编号——**外部只能引用编号**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutEntry {
    pub id: u32,
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
}

/// 已经收到的一个件（给界面显示"手机刚投了什么"）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    pub name: String,
    pub bytes: u64,
}

#[derive(Default)]
struct Shared {
    outbox: Vec<OutEntry>,
    next_id: u32,
    received: Vec<Received>,
}

pub struct Hub {
    token: String,
    inbox: PathBuf,
    max_bytes: u64,
    shared: Mutex<Shared>,
    /// 「找一个没人占的名字」到「写完这份」整段串行：两个手机同时投 `a.txt` 时，
    /// 后一个必须看到前一个已经落下的名字，否则 `create_exclusive` 的
    /// `truncate(true)` 会把先到的那份洗掉。
    /// 用 tokio 的锁而不是 std 的：这段里有 `.await`，std 的守卫跨 await 会让
    /// 整个 handler 的 future 变 `!Send`，axum 直接不认这个 handler。
    create_lock: tokio::sync::Mutex<()>,
}

impl Hub {
    pub fn new(inbox: impl Into<PathBuf>) -> Self {
        Hub {
            token: new_token(),
            inbox: inbox.into(),
            max_bytes: DEFAULT_MAX_BYTES,
            shared: Mutex::new(Shared::default()),
            create_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn with_max_bytes(mut self, max: u64) -> Self {
        self.max_bytes = max;
        self
    }

    /// 用一把**外部给的** token（常驻时从 `share.json` 读回来的那把）。
    /// 形状不对的直接忽略、退回自己生成的——弱 token 比没有更糟，
    /// 见 `config::token_is_well_formed`。
    pub fn with_token(mut self, token: &str) -> Self {
        if config::token_is_well_formed(token) {
            self.token = token.to_string();
        }
        self
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn inbox(&self) -> &Path {
        &self.inbox
    }

    /// 这格服务对外的地址。**必须用真正绑上的那个 host 拼**——
    /// 拿局域网 IP 去配一个绑在回环上的端口，印出来就是一条打不开的死链
    /// （真窗口第一帧就是这么错的）。
    pub fn link(&self, host: &str, port: u16) -> String {
        let shown = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        format!("http://{shown}:{port}/?k={}", self.token)
    }

    /// 登记一个**已存在的普通文件**给手机取。目录、不存在的路径一律不收——
    /// 收了就等于把整棵子树挂出去。
    pub fn add_outbox(&self, path: &Path) -> Option<OutEntry> {
        let md = std::fs::metadata(path).ok()?;
        if !md.is_file() {
            return None;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "未命名".into());
        let mut s = self.shared.lock().ok()?;
        let id = s.next_id;
        s.next_id += 1;
        let e = OutEntry {
            id,
            name,
            path: path.to_path_buf(),
            bytes: md.len(),
        };
        s.outbox.push(e.clone());
        Some(e)
    }

    pub fn outbox(&self) -> Vec<OutEntry> {
        self.shared
            .lock()
            .map(|s| s.outbox.clone())
            .unwrap_or_default()
    }

    pub fn find_out(&self, id: u32) -> Option<OutEntry> {
        self.shared
            .lock()
            .ok()
            .and_then(|s| s.outbox.iter().find(|e| e.id == id).cloned())
    }

    pub fn clear_outbox(&self) {
        if let Ok(mut s) = self.shared.lock() {
            s.outbox.clear();
        }
    }

    pub fn received(&self) -> Vec<Received> {
        self.shared
            .lock()
            .map(|s| s.received.clone())
            .unwrap_or_default()
    }

    fn note_received(&self, name: String, bytes: u64) {
        if let Ok(mut s) = self.shared.lock() {
            s.received.push(Received { name, bytes });
        }
    }
}

/// 16 字节随机 ⇒ 32 位十六进制。没有可用随机源就直接说清楚，
/// 不许退化成"拿时间凑一个"——那等于把鉴权变成可猜的。
pub fn new_token() -> String {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect("这台机器没有可用的随机源，不能开局域网投递");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 每个入口都先过这一格。明文比较：token 是 128 bit 随机值，
/// 局域网里能读到响应的timing差的人本来就已经能读到 token 本身。
pub fn token_ok(wanted: &str, given: Option<&String>) -> bool {
    given.map(|g| g == wanted).unwrap_or(false)
}

/// 把对面给的"文件名"削成一个**真的能当名字用**的串。
///
/// 🔴 这是这一格里最要紧的函数：`n=` 是外部输入，直接 `inbox.join(raw)` 的话
/// `..\\..\\Windows\\System32\\drivers\\etc\\hosts` 就是往系统目录写。
/// 做法是**只取最后一段**（两种分隔符都算），再拒掉剩下的危险形态；
/// 认不出的回 `None`（调用方回 400），**绝不"清洗一下凑合用"**。
pub fn sanitize_name(raw: &str) -> Option<String> {
    let last = raw.rsplit(['/', '\\']).next().filter(|s| !s.is_empty())?;
    if matches!(last, "." | "..") || last.contains(':') {
        return None;
    }
    if last.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f) {
        return None;
    }
    // Windows 会静默吃掉尾部的点和空格：`a.txt` 与 `a.txt.` 在它眼里是同一个文件，
    // 留着会让"同名让路"那格判错。
    let trimmed = last.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        return None;
    }
    // 名字过长会撞路径上限，截**主干**、留扩展名。
    const CAP: usize = 120;
    let name = if trimmed.chars().count() > CAP {
        let dot = trimmed.rfind('.').filter(|i| *i > 0);
        let (stem, ext) = match dot {
            Some(i) => (&trimmed[..i], &trimmed[i..]),
            None => (trimmed, ""),
        };
        let keep = CAP - ext.chars().count();
        let head: String = stem.chars().take(keep.saturating_sub(1)).collect();
        format!("{head}~{ext}")
    } else {
        trimmed.to_string()
    };
    Some(name)
}

/// 同名不让覆盖：`a.txt` 占了就给 `a (2).txt`。命名规则与全仓同一格
/// （`fmnr_fs::ops::unique_name` → `fmnr_core::rename`），
/// 所以"从手机投进来"和"从栏里拖进来"得到的续号长得一样。
pub fn unique_dest(dir: &Path, name: &str) -> PathBuf {
    fmnr_fs::ops::unique_name(&dir.join(name))
}

/// 手机上的那一页。`outbox` 只在 token 对的时候才传进来（错的时候调用方给空表）。
pub fn page_html(token: &str, outbox: &[OutEntry]) -> String {
    let mut list = String::new();
    for e in outbox {
        list.push_str(&format!(
            "<li><a href=\"/get?k={}&i={}\">{}</a> <span>{}</span></li>",
            url_escape(token),
            e.id,
            html_escape(&e.name),
            human_bytes(e.bytes)
        ));
    }
    let out = if outbox.is_empty() {
        "<p class=muted>还没登记要给的件。在电脑上打开「手机传文件」那一栏，\
         选中文件后点「把当前栏选中的登记为待取」。</p>"
            .to_string()
    } else {
        format!("<h2>从电脑取</h2><ul>{list}</ul>")
    };
    format!(
        "<!doctype html><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>fmnr 局域网投递</title>\
         <style>body{{font:16px/1.5 system-ui;margin:20px;max-width:34em}}\
         h1{{font-size:20px}}h2{{font-size:16px;margin-top:28px}}\
         li{{margin:6px 0}}button{{padding:10px 16px;margin-top:10px}}\
         .muted{{color:#777}}#log{{white-space:pre-wrap;font-size:14px;margin-top:14px}}\
         </style>\
         <h1>往这台电脑丢文件</h1>\
         <input id=f type=file multiple>\
         <br><button onclick=up()>发送</button>\
         <div id=log></div>\
         <form method=post action=\"/send?k={}\" enctype=multipart/form-data>\
         <input id=f2 type=file name=f multiple>\
         <button type=submit>不用脚本发送</button></form>\
         <p class=muted>上面两个按钮同一个去处：都进电脑的收件目录，同名都让路。\
         第二个不跑脚本，浏览器坏了 JS、套壳浏览器禁了 fetch 也能送；\
         但它要整单收进内存，所以上限低——手机拍的长视频请走第一个，它边收边写盘。</p>\
         {out}\
         <p class=muted>关掉电脑上的开关，这个链接立刻失效。</p>\
         <script>var K='{}';\
         async function up(){{var fs=f.files,log='';\
         for(const fl of fs){{log+='上传 '+fl.name+' …\\n';d().textContent=log;\
         try{{var r=await fetch('/put?k='+K+'&n='+encodeURIComponent(fl.name),{{method:'POST',body:fl}});\
         var t=await r.text();log+=(r.ok?'✓ ':'✗ ')+fl.name+' → '+t+'\\n';}}\
         catch(e){{log+='✗ '+fl.name+' → '+e+'\\n';}}d().textContent=log;}}}}\
         function d(){{return document.getElementById('log')}}\
         </script>",
        url_escape(token),
        url_escape(token)
    )
}

/// 只给一句人话的错误页。不带堆栈、不带路径。
fn say(code: StatusCode, msg: &str) -> Response {
    (
        code,
        Html(format!(
            "<!doctype html><meta charset=utf-8><p>{}",
            html_escape(msg)
        )),
    )
        .into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// 够用的 percent-encode：token 是十六进制、编号是数字，
/// 只有文件名要进 URL，而文件名走的是 `encodeURIComponent`（浏览器侧）。
pub(crate) fn url_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} {}", UNITS[0])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

pub fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/put", post(put))
        .route("/send", post(send))
        .route("/dav", any(dav::handle))
        .route("/dav/", any(dav::handle))
        .route("/dav/{*rest}", any(dav::handle))
        .route("/get", get(download))
        .with_state(hub)
}

fn qtoken(q: &HashMap<String, String>) -> Option<&String> {
    q.get("k")
}

async fn page(State(hub): State<Arc<Hub>>, Query(q): Query<HashMap<String, String>>) -> Response {
    if !token_ok(&hub.token, qtoken(&q)) {
        return say(
            StatusCode::UNAUTHORIZED,
            "链接不对：这一格服务只认带 token 的地址。",
        );
    }
    Html(page_html(&hub.token, &hub.outbox())).into_response()
}

/// 收件（流式那条腿）：请求体**就是文件本身**，名字走 `n=`。
/// 页面里有 JS 时走这条；没有 JS 时走 `/send`。两条腿最后同进 `ingest`。
async fn put(
    State(hub): State<Arc<Hub>>,
    Query(q): Query<HashMap<String, String>>,
    req: Request,
) -> Response {
    if !token_ok(&hub.token, qtoken(&q)) {
        return say(StatusCode::UNAUTHORIZED, "没有 token，不收。");
    }
    let Some(raw) = q.get("n") else {
        return say(StatusCode::BAD_REQUEST, "没给文件名（n=）。");
    };
    let stream = req.into_body().into_data_stream();
    match ingest(hub, raw, Box::pin(stream)).await {
        Ok(msg) => (StatusCode::OK, msg).into_response(),
        Err(e) => e.into_response(),
    }
}

/// 🔴 **所有投递通道共用的那一个收口**。浏览器页面、不带 JS 的表单、任何第三方
/// app 的 POST，最后都必须走这里——因为「同名让路 + 拒写共享 + 不留半截 + 报给
/// 界面」这四条只在这一处实现。新通道若绕过它自己 `inbox.join(name)`，
/// 就把 `#61` 那道闸和全仓唯一那份命名规则废掉了。
///
/// 流式写入：`/put` 一路从 socket 读，不在内存里攒整份。
pub(crate) async fn ingest(
    hub: Arc<Hub>,
    raw_name: &str,
    stream: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send + 'static>,
    >,
) -> Result<String, Box<Response>> {
    ingest_by(hub, raw_name, NameRule::Unique, stream).await
}

/// 挂盘那条腿（WebDAV PUT）：**写到这个相对路径上**，已存在就整体替换。
/// 与"丢件"的区别只在这一处：丢件同名让路，盘就是盘。
///
/// `rel` 的每一段都必须**已经**由调用方（`dav::segments`）验过。
/// 这里再验一遍是为了"绕过 dav 直接调"的那条路也不至于跑出收件目录。
pub(crate) async fn ingest_exact(
    hub: Arc<Hub>,
    rel: &[String],
    stream: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send + 'static>,
    >,
) -> Result<String, Box<Response>> {
    let Some(last) = rel.last() else {
        return Err(Box::new(say(StatusCode::BAD_REQUEST, "没有文件名。")));
    };
    if rel
        .iter()
        .any(|s| sanitize_name(s).as_deref() != Some(s.as_str()))
    {
        return Err(Box::new(say(
            StatusCode::FORBIDDEN,
            "路径里有一段不是合法名字。",
        )));
    }
    let dest = rel.iter().fold(hub.inbox.clone(), |p, s| p.join(s));
    write_into(hub, last.clone(), dest, NameRule::Exact, stream).await
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameRule {
    /// 丢件：同名让路（浏览器页面那两条腿）。
    Unique,
    /// 盘：写到这个名字上（WebDAV）。已存在的件在改名那一刻才被替换掉。
    Exact,
}

async fn ingest_by(
    hub: Arc<Hub>,
    raw_name: &str,
    rule: NameRule,
    stream: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send + 'static>,
    >,
) -> Result<String, Box<Response>> {
    let name = match sanitize_name(raw_name) {
        Some(n) => n,
        None => {
            return Err(Box::new(say(
                StatusCode::BAD_REQUEST,
                "这个名字我不能用：只收纯文件名，不带路径、盘符或控制字符。",
            )))
        }
    };
    let dest = unique_dest(&hub.inbox, &name);
    write_into(hub, name, dest, rule, stream).await
}

/// 真正的写入：调用方已经把名字与目标路径定好（丢件那条走 `unique_dest`，
/// 盘那条走 `inbox/<相对路径>`）。
async fn write_into(
    hub: Arc<Hub>,
    name: String,
    dest: PathBuf,
    rule: NameRule,
    mut stream: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send + 'static>,
    >,
) -> Result<String, Box<Response>> {
    std::fs::create_dir_all(&hub.inbox).map_err(|e| {
        Box::new(say(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("收件目录建不出来：{e}"),
        ))
    })?;
    // 挑名字到写完，整段串行（见 `create_lock`）。
    let _guard = hub.create_lock.lock().await;
    // 盘那条腿先写临时件、最后改名：直接截断目标再写，写到一半失败旧件就没了。
    let (write_path, staged) = match rule {
        NameRule::Unique => (dest.clone(), false),
        NameRule::Exact => (
            dest.parent()
                .unwrap_or(&hub.inbox)
                .join(format!(".part-{}", new_token())),
            true,
        ),
    };
    let std_file = fmnr_fs::ops::create_exclusive(&write_path).map_err(|e| {
        Box::new(say(
            StatusCode::CONFLICT,
            &format!("这个文件现在写不了（可能被别的程序开着）：{e}"),
        ))
    })?;
    let mut file = tokio::fs::File::from_std(std_file);
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                discard(&write_path).await;
                return Err(Box::new(say(
                    StatusCode::BAD_REQUEST,
                    &format!("读到一半断了：{e}"),
                )));
            }
        };
        written += chunk.len() as u64;
        if written > hub.max_bytes {
            discard(&write_path).await;
            return Err(Box::new(say(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!(
                    "超过上限 {}，这份没收（半截的已删掉）。",
                    human_bytes(hub.max_bytes)
                ),
            )));
        }
        if let Err(e) = file.write_all(&chunk).await {
            discard(&write_path).await;
            return Err(Box::new(say(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("写盘失败：{e}"),
            )));
        }
    }
    if let Err(e) = file.flush().await {
        discard(&write_path).await;
        return Err(Box::new(say(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("收尾失败：{e}"),
        )));
    }
    drop(file);
    if staged {
        if let Err(e) = std::fs::rename(&write_path, &dest) {
            discard(&write_path).await;
            return Err(Box::new(say(
                StatusCode::CONFLICT,
                &format!("转换不了（目标是不是正被别的程序开着？）：{e}"),
            )));
        }
    }
    let saved = dest
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or(name);
    hub.note_received(saved.clone(), written);
    Ok(format!("已存为 {saved}（{}）", human_bytes(written)))
}

/// 失败时**不许留半截文件**：留了下一轮同名让路会把它当成"已存在的件"。
/// 调用方（`put`）全程持有 `create_lock`，这里不再取锁——tokio 的锁不可重入。
async fn discard(dest: &Path) {
    if let Err(e) = tokio::fs::remove_file(dest).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            // 删不掉是"用户以为收到了、其实盘上留了个坏件"的源头，必须说出来。
            eprintln!("fmnr-share: 半截文件删不掉 {}: {e}", dest.display());
        }
    }
}

/// 从 `Content-Type` 里取 multipart 的 boundary。浏览器给的是
/// `multipart/form-data; boundary=----WebKitFormBoundaryXX`，带引号的形式也要认。
pub fn multipart_boundary(content_type: &str) -> Option<String> {
    let lower = content_type.to_ascii_lowercase();
    if !lower.starts_with("multipart/form-data") {
        return None;
    }
    let (_, rest) = content_type.split_once(';')?;
    let rest = rest.trim_start();
    let after = match rest.strip_prefix("boundary=") {
        Some(a) => a.trim_start(),
        None => {
            let rest_lower = rest.to_ascii_lowercase();
            let idx = rest_lower.find("boundary=")?;
            rest.get(idx + "boundary=".len()..)?
        }
    };
    if after.starts_with('"') {
        let inner = after.get(1..)?.split_once('"')?.0;
        Some(inner.to_string())
    } else {
        Some(after.split(';').next()?.trim().to_string())
    }
}

/// 解析出来的一块。有 `filename` 的才是文件，其余（隐藏域）`filename` 为 `None`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormPart {
    pub filename: Option<String>,
    pub data: Vec<u8>,
}

/// **不引第三方 multipart 库**的严格解析器：按 RFC 7578 的分隔符切。
///
/// 刻意做成"整块进内存再切"（`/put` 那条才是流式）：这条通道是给**没有 JS 的
/// 原生表单**兜底的，一次一两兆的文档，换掉一个下载依赖值得。天花板仍由
/// `max_bytes` 卡在解析之前。
///
/// 只信分隔符，不信长度字段；看不懂的块直接丢，不猜。
pub fn parse_multipart(boundary: &str, body: &[u8]) -> Vec<FormPart> {
    let dash = format!("--{boundary}");
    let dash = dash.as_bytes();
    let mut parts = Vec::new();
    let mut at = 0usize;
    while at <= body.len() && parts.len() < 64 {
        // 分隔符必须出现在一行的开头：串首，或紧跟在一个 CRLF 之后。
        // 🔴 只找 `\r\n--` 是不够的——文件内容里完全可能出现 `--B` 这样的行
        // （测试就是这么打出来的）。后面必须是 CRLF、或 `--`（收尾），才算分隔符。
        let start = if body[at..].starts_with(dash) {
            at
        } else {
            match find_crlf_delim(&body[at..], dash) {
                // 返回的是分隔符**前面那个 CRLF** 的偏移，+2 才落到 `--` 上。
                Some(k) => at + k + 2,
                None => break,
            }
        };
        let after = start + dash.len();
        match &body[after..] {
            [b'-', b'-', ..] => break, // 结尾的 `--boundary--`
            [b'\r', b'\n', ..] => {}
            _ => break, // 看不懂的形状：不猜，停
        }
        let nl = match find_bytes(&body[after..], b"\r\n") {
            Some(k) => after + k + 2,
            None => break,
        };
        let next = match find_crlf_delim(&body[nl..], dash) {
            Some(k) => nl + k,
            None => body.len(), // 收尾被截断：收下这一块，别丢
        };
        let block = &body[nl..next];
        let head_end = match find_bytes(block, b"\r\n\r\n") {
            Some(k) => k,
            None => block.len(),
        };
        let head = String::from_utf8_lossy(&block[..head_end]).to_string();
        let data = if head_end + 4 <= block.len() {
            block[head_end + 4..].to_vec()
        } else {
            Vec::new()
        };
        parts.push(FormPart {
            filename: quoted_value(&head, "filename"),
            data,
        });
        // 停在下一个分隔符**前面的那个 CRLF** 上（不是 `--` 上）：
        // 🔴 多走 2 个字节就会从分隔符中间起跑，第二块永远认不出来（真红过）。
        at = next + 2;
    }
    parts
}

/// 从 `hay` 里找下一个「真分隔符」的偏移：`\r\n` + `--boundary`，
/// 且其后紧跟 CRLF 或 `--`。返回的是那个 `\r\n` 的偏移。
fn find_crlf_delim(hay: &[u8], dash: &[u8]) -> Option<usize> {
    let mut i = 0usize;
    while let Some(k) = find_bytes(&hay[i..], b"\r\n") {
        let at = i + k;
        if hay[at + 2..].starts_with(dash) {
            match &hay[at + 2 + dash.len()..] {
                [b'\r', b'\n', ..] | [b'-', b'-', ..] => return Some(at),
                _ => {}
            }
        }
        i = at + 1;
    }
    None
}

/// 在 `hay` 里从前往后找 `needle`，返回偏移。
fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// 从一段头部里取 `key="value"` 的值（只认带引号那种，够浏览器用）。
fn quoted_value(head: &str, key: &str) -> Option<String> {
    let at = head.to_ascii_lowercase().find(&format!("{key}=\""))?;
    let rest = head[at + key.len() + 2..].split('"').next()?;
    (!rest.is_empty()).then(|| rest.to_string())
}

/// 不带 JS 的那条腿：原生表单 `POST /send?k=`，体是 `multipart/form-data`。
/// 每一个带 `filename` 的块都走 `ingest`——**包括同名让路、拒写共享、不留半截**。
async fn send(
    State(hub): State<Arc<Hub>>,
    Query(q): Query<HashMap<String, String>>,
    req: Request,
) -> Response {
    if !token_ok(&hub.token, qtoken(&q)) {
        return say(StatusCode::UNAUTHORIZED, "没有 token，不收。");
    }
    let Some(ct) = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    else {
        return say(
            StatusCode::BAD_REQUEST,
            "这个表单没带 Content-Type，切不开。",
        );
    };
    let ct = ct.to_string();
    let Some(boundary) = multipart_boundary(&ct) else {
        return say(
            StatusCode::BAD_REQUEST,
            "不是 multipart/form-data，这一条腿走不通。",
        );
    };
    // 整块进内存的那条腿用自己的天花板（见 `SEND_MAX_BYTES`）。
    let cap = hub.max_bytes.min(SEND_MAX_BYTES);
    let body = match axum::body::to_bytes(req.into_body(), cap as usize).await {
        Ok(b) => b,
        Err(_) => {
            return say(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!(
                    "这一单超过 {}，没收。这条腿要整单收进内存，所以上限比另一条低：\
                     大文件请用页面上那个「发送」按钮（它是边收边写盘的）。",
                    human_bytes(cap)
                ),
            )
        }
    };
    let mut lines = String::new();
    let mut landed = 0usize;
    for part in parse_multipart(&boundary, &body) {
        let Some(raw) = part.filename else { continue };
        let stream = futures::stream::once(async move { Ok(axum::body::Bytes::from(part.data)) });
        match ingest(hub.clone(), &raw, Box::pin(stream)).await {
            Ok(msg) => {
                landed += 1;
                // 🔴 成功分支也要转义：`msg` 里带着**落地后的文件名**，而
                // `sanitize_name` 只挡路径/控制字符，挡不住 `<`（`<img src=x onerror=...>.txt`
                // 是合法文件名）。上一版只转义了失败分支，而门里的载荷带 `/`，
                // 被 sanitize 从最后一段截掉了 ⇒ 门是**碰巧绿**的。
                lines.push_str(&format!("✓ {}<br>", html_escape(&msg)));
            }
            Err(e) => {
                let why = e.status().as_u16();
                lines.push_str(&format!("✗ {} 没收下（{why}）<br>", html_escape(&raw)));
            }
        }
    }
    if landed == 0 && lines.is_empty() {
        return say(StatusCode::BAD_REQUEST, "这一单里没有文件。");
    }
    Html(format!(
        "<!doctype html><meta charset=utf-8><title>fmnr 收件</title>\
         <body style=\"font:16px/1.6 system-ui;margin:20px\">{lines}\
         <p><a href=\"?k={}\">再来一次</a></p>",
        url_escape(hub.token())
    ))
    .into_response()
}

/// 取件：只认编号。这里**没有**任何接受路径的入口。
async fn download(
    State(hub): State<Arc<Hub>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !token_ok(&hub.token, qtoken(&q)) {
        return say(StatusCode::UNAUTHORIZED, "链接不对。");
    }
    let Some(raw) = q.get("i").and_then(|s| s.parse::<u32>().ok()) else {
        return say(StatusCode::BAD_REQUEST, "没给编号（i=）。");
    };
    let Some(e) = hub.find_out(raw) else {
        return say(StatusCode::NOT_FOUND, "电脑上没有登记过这个编号的件。");
    };
    // 登记之后那个件也可能被删/被移走。这要说成"已经不在了"，
    // 不许 panic、也不许回一个空文件——空文件在手机上看着像"下载成功但内容是空的"。
    let std_file = match std::fs::File::open(&e.path) {
        Ok(f) => f,
        Err(err) => {
            return say(
                StatusCode::GONE,
                &format!("那个件已经不在了（{}）：{err}", e.path.display()),
            )
        }
    };
    let stream = tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(std_file));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename*=UTF-8''{}", url_escape(&e.name)),
        )
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| say(StatusCode::INTERNAL_SERVER_ERROR, "响应头拼不出来。"))
}

/// 起服务并阻塞。`addr` 形如 `127.0.0.1:8799`；对外开的那道闸由调用方先过
/// （`fmnr_core::net::bind_addr`），这里不再判一次。
pub async fn run(hub: Arc<Hub>, addr: &str) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(hub)).await
}

/// 一个开着的投递口。`Drop` 即关：发停止信号并等线程收摊——
/// 界面点「关掉」之后端口还在听，是那种"看着关了其实没关"的缺陷。
#[derive(Debug)]
pub struct Server {
    pub port: u16,
    /// 真正绑上的那个 host（拼链接要用它，不能用"看起来能对外"的那个）。
    pub host: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// 起一个后台线程跑服务。`host` 给 `127.0.0.1` 之外的一律要 `allow_remote`，
/// 判据共用 `fmnr_core::net::bind_addr`（与 `fmnr-ai` 的 HTTP 面同一格）。
///
/// 端口给 0 让系统挑，回包里带真实端口——手机要扫这个链接，猜不得。
pub fn start(hub: Arc<Hub>, host: &str, port: u16, allow_remote: bool) -> Result<Server, String> {
    let addr = fmnr_core::net::bind_addr(host, port, allow_remote)?;
    let bound_host = host.to_string();
    let (tx, rx) =
        std::sync::mpsc::channel::<Result<(u16, tokio::sync::oneshot::Sender<()>), String>>();
    let thread = std::thread::Builder::new()
        .name("fmnr-share".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(format!("起不了运行时：{e}")));
                    return;
                }
            };
            rt.block_on(async move {
                let listener = match tokio::net::TcpListener::bind(&addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        let _ = tx.send(Err(format!("绑不上 {addr}：{e}")));
                        return;
                    }
                };
                let bound = listener.local_addr().map(|a| a.port()).unwrap_or(0);
                let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
                // 调用方已经不等了（超时/被丢），那就别起了。
                if tx.send(Ok((bound, stop_tx))).is_err() {
                    return;
                }
                let served = axum::serve(listener, router(hub))
                    .with_graceful_shutdown(async move {
                        let _ = stop_rx.await;
                    })
                    .await;
                if let Err(e) = served {
                    eprintln!("fmnr-share: 服务退出：{e}");
                }
            });
        })
        .map_err(|e| format!("起不了投递线程：{e}"))?;
    match rx.recv() {
        Ok(Ok((bound, stop))) => Ok(Server {
            port: bound,
            host: bound_host,
            stop: Some(stop),
            thread: Some(thread),
        }),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(format!("投递线程没报到就退了：{e}")),
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// 这一格的门。全部走 `oneshot` 打真 handler（不起端口），
/// 但**落盘是真的**——收件目录用临时目录，断言读的是盘上的字节。
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use tower::ServiceExt;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fmnr-share-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    async fn ask(hub: Arc<Hub>, uri: &str, body: Vec<u8>) -> (StatusCode, String) {
        let req = Request::builder()
            .uri(uri)
            .method(if body.is_empty() { "GET" } else { "POST" })
            .body(Body::from(body))
            .unwrap();
        let res = router(hub).oneshot(req).await.unwrap();
        let code = res.status();
        let text =
            String::from_utf8_lossy(&to_bytes(res.into_body(), 1 << 20).await.unwrap()).to_string();
        (code, text)
    }

    /// 没有 token 什么都拿不到，而且**收件目录里一根毛都不许多出来**。
    #[tokio::test]
    async fn without_the_token_there_is_no_page_no_write_and_no_list() {
        let inbox = tmp("token");
        let hub = Arc::new(Hub::new(inbox.clone()));
        hub.add_outbox(&{
            let f = inbox.join("要取的件.txt");
            std::fs::write(&f, b"secret").unwrap();
            f
        });

        let (code, page) = ask(hub.clone(), "/", vec![]).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "没 token 不许给页面");
        assert!(!page.contains("要取的件"), "错误页里漏出了待取清单：{page}");

        let (code, _) = ask(
            hub.clone(),
            "/put?n=偷投.txt",
            b"nobody authorized this".to_vec(),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert!(
            !inbox.join("偷投.txt").exists(),
            "没 token 的 POST 照样落了盘 ⇒ 任何人都能往这台机器投文件"
        );

        let (code, _) = ask(hub.clone(), "/get?i=0", vec![]).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
    }

    /// 名字是外部输入。`inbox.join(raw)` 那种写法会把系统目录当成收件目录。
    #[test]
    fn a_name_is_flattened_to_its_last_segment_or_refused() {
        assert_eq!(
            sanitize_name("../../Windows/System32/drivers/etc/hosts").as_deref(),
            Some("hosts")
        );
        assert_eq!(
            sanitize_name("C:\\Users\\someone\\Desktop\\x.exe").as_deref(),
            Some("x.exe")
        );
        assert_eq!(sanitize_name("照片 1.jpg").as_deref(), Some("照片 1.jpg"));
        // 认不出的一律拒，不"清洗一下凑合用"。
        for bad in ["..", ".", "", "a:b", "C:\\", "\u{1}x"] {
            assert_eq!(sanitize_name(bad), None, "{bad:?} 不该被当成文件名");
        }
        assert_eq!(sanitize_name("..namedots").as_deref(), Some("..namedots"));
        // Windows 会静默吃掉尾部的点和空格 ⇒ 先削掉，否则"同名让路"会判错。
        assert_eq!(sanitize_name("a.txt. ").as_deref(), Some("a.txt"));
    }

    /// 超长名截主干、留扩展名（撞路径上限是真实故障，不是理论）。
    #[test]
    fn a_long_name_keeps_its_extension() {
        let long = format!("{}.jpg", "x".repeat(400));
        let got = sanitize_name(&long).unwrap();
        assert!(
            got.chars().count() <= 121,
            "还是太长：{}",
            got.chars().count()
        );
        assert!(got.ends_with(".jpg"), "扩展名被截掉了：{got}");
    }

    /// 走真 handler 确认削平之后**确实落在收件目录里**，而不是父目录。
    #[tokio::test]
    async fn a_flattened_name_lands_in_the_inbox_not_above_it() {
        let root = tmp("traversal");
        let inbox = root.join("inbox");
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        let (code, _) = ask(
            hub,
            &format!("/put?k={tok}&n=..\\..\\evil.txt"),
            b"payload".to_vec(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        // 判据要红在**断言消息**上：原先这里直接 `.unwrap()`，变异时红成
        // "Result::unwrap() on an Err: NotFound"——那是探针自己崩，不算把机制钉住。
        let landed = std::fs::read(inbox.join("evil.txt"));
        assert!(
            landed.is_ok(),
            "削平后的名字没落进收件目录 {}，那里只有：{:?}",
            inbox.join("evil.txt").display(),
            std::fs::read_dir(&inbox).map(|rd| rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name())
                .collect::<Vec<_>>())
        );
        assert_eq!(landed.unwrap(), b"payload");
        assert!(
            !root.join("evil.txt").exists(),
            "写到收件目录外面去了：{}",
            root.join("evil.txt").display()
        );
    }

    /// 同名不许覆盖：让路成 `a (2).txt`，原来那份字节一个都不动。
    #[tokio::test]
    async fn a_second_upload_with_the_same_name_does_not_replace_the_first() {
        let inbox = tmp("dupes");
        std::fs::write(inbox.join("a.txt"), b"first").unwrap();
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        let (code, msg) = ask(hub, &format!("/put?k={tok}&n=a.txt"), b"second".to_vec()).await;
        assert_eq!(code, StatusCode::OK, "被拒了：{msg}");
        assert_eq!(std::fs::read(inbox.join("a.txt")).unwrap(), b"first");
        assert_eq!(std::fs::read(inbox.join("a (2).txt")).unwrap(), b"second");
        std::fs::remove_dir_all(&inbox).ok();
    }

    /// 超上限：**不许留半截文件**。留了下一轮同名让路会把它当成已存在的件。
    #[tokio::test]
    async fn an_oversized_upload_leaves_nothing_behind() {
        let inbox = tmp("cap");
        let hub = Arc::new(Hub::new(inbox.clone()).with_max_bytes(8));
        let tok = hub.token().to_string();
        let (code, msg) = ask(hub, &format!("/put?k={tok}&n=big.bin"), vec![7u8; 4096]).await;
        assert_eq!(code, StatusCode::PAYLOAD_TOO_LARGE, "没拦住：{msg}");
        let left: Vec<_> = std::fs::read_dir(&inbox)
            .map(|rd| rd.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "上限外的件留下了：{left:?}");
        std::fs::remove_dir_all(&inbox).ok();
    }

    /// 取件只认编号：没登记的编号拿不到东西，路径参数压根不是入口。
    #[tokio::test]
    async fn downloads_are_addressed_by_id_never_by_path() {
        let dir = tmp("outbox");
        let keep = dir.join("给我手机.txt");
        std::fs::write(&keep, b"payload-bytes").unwrap();
        let other = dir.join("不给.txt");
        std::fs::write(&other, b"private").unwrap();

        let hub = Arc::new(Hub::new(dir.join("inbox")));
        let tok = hub.token().to_string();
        let e = hub.add_outbox(&keep).expect("登记要成功");
        assert_eq!(hub.add_outbox(&dir), None, "目录不能登记成待取件");

        let (code, body) = ask(hub.clone(), &format!("/get?k={tok}&i={}", e.id), vec![]).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body, "payload-bytes");

        let (code, _) = ask(hub.clone(), &format!("/get?k={tok}&i=999"), vec![]).await;
        assert_eq!(code, StatusCode::NOT_FOUND, "没登记的编号要给东西");

        // 想拿路径当参数：`i` 缺失就是 400，不会退化成"按路径给"。
        let (code, _) = ask(
            hub.clone(),
            &format!("/get?k={tok}&path={}", other.display()),
            vec![],
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 登记的名字要转义再进 HTML——手机页面上那是别人可控的字符串。
    #[test]
    fn the_page_escapes_a_registered_name() {
        let html = page_html(
            "tok",
            &[OutEntry {
                id: 0,
                name: "<script>alert(1)</script>".into(),
                path: PathBuf::from("x"),
                bytes: 1234,
            }],
        );
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "名字没转义：{html}"
        );
        assert!(
            !html.contains("><script>alert(1)"),
            "转义没挡住可执行片段：{html}"
        );
    }

    /// 链接里的地址**必须就是绑上的那个**：真窗口第一帧的错法是端口绑在回环、
    /// 链接却印局域网 IP，用户复制走一条打不开的死链。
    #[test]
    fn the_link_echoes_the_host_that_was_bound() {
        let hub = Hub::new(std::env::temp_dir());
        let loop_back = hub.link("127.0.0.1", 54531);
        assert!(
            loop_back.contains("127.0.0.1:54531"),
            "绑的是回环就该印回环：{loop_back}"
        );
        assert!(
            loop_back.contains(hub.token()),
            "链接里没带 token：{loop_back}"
        );
        let lan = hub.link("192.168.1.3", 8799);
        assert_eq!(lan, format!("http://192.168.1.3:8799/?k={}", hub.token()));
        // IPv6 字面量不带方括号拼不进 URL。
        assert!(hub.link("::1", 80).starts_with("http://[::1]:80/"));
    }

    /// 两把 token 之间要真的不一样（同一格服务开两次不能共用一把钥匙）。
    #[test]
    fn tokens_are_fresh_per_hub() {
        let a = Hub::new(PathBuf::from("a"));
        let b = Hub::new(PathBuf::from("b"));
        assert_ne!(a.token(), b.token());
        assert_eq!(a.token().len(), 32, "16 字节的十六进制应该是 32 位");
        assert!(!token_ok(a.token(), Some(&b.token().to_string())));
        assert!(token_ok(a.token(), Some(&a.token().to_string())));
        assert!(!token_ok(a.token(), None));
    }

    /// 常驻要能把上次那把 token 带回来；但**坏形状的一律不认**，退回自己生成的。
    #[test]
    fn a_supplied_token_is_used_only_when_it_looks_right() {
        let good = "0123456789abcdef0123456789abcdef";
        let hub = Hub::new(PathBuf::from("x")).with_token(good);
        assert_eq!(hub.token(), good, "好 token 没被用上");
        assert!(hub.link("192.168.1.3", 8787).contains(good));

        for bad in ["", "abc", "0123456789ABCDEF0123456789ABCDEF"] {
            let h = Hub::new(PathBuf::from("x")).with_token(bad);
            assert_ne!(h.token(), bad, "{bad:?} 这种形状不该被当成钥匙");
            assert_eq!(h.token().len(), 32, "该退回自己生成的那把");
        }
    }

    /// 真端口跑一遍：绑得上、答得出、关掉之后**不再应答**。
    /// 用 std 的 `TcpStream` 手写一行请求，不为这一条引 HTTP 客户端。
    #[test]
    fn a_started_server_answers_then_stops_answering() {
        let dir = tmp("live");
        let hub = Arc::new(Hub::new(dir.clone()));
        let tok = hub.token().to_string();
        let srv = start(hub.clone(), "127.0.0.1", 0, false).expect("测试前提：能起在回环上");
        assert!(srv.port > 0, "端口给 0 时要回真实端口，拿到 {}", srv.port);

        let page = http_get(srv.port, &format!("/?k={tok}"));
        assert!(
            page.starts_with("HTTP/1.1 200"),
            "带 token 的页面没回来：{}",
            &page[..page.len().min(80)]
        );
        assert!(page.contains("往这台电脑丢文件"), "回来的不是那一页");
        assert!(
            http_get(srv.port, "/").starts_with("HTTP/1.1 401"),
            "没 token 也照样答了"
        );

        let port = srv.port;
        drop(srv);
        let after = http_get(port, &format!("/?k={tok}"));
        assert!(
            !after.starts_with("HTTP/1.1 200"),
            "关掉之后那个端口还在给页面：{}",
            &after[..after.len().min(80)]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 这一格也不能对外绑——闸与 `fmnr-ai` 的 HTTP 面共用同一格，
    /// 所以这里钉的是"调用方漏传批准时，它真的会拒"。
    #[test]
    fn starting_on_a_public_interface_needs_explicit_approval() {
        let hub = Arc::new(Hub::new(tmp("bind")));
        let e = start(hub, "0.0.0.0", 0, false).unwrap_err();
        assert!(e.contains("--allow-remote"), "要告诉调用方怎么放行：{e}");
    }

    /// 一次极简 HTTP/1.1 往返。连不上回空串（调用方按"没应答"判）。
    fn http_get(port: u16, path: &str) -> String {
        use std::io::{Read, Write};
        let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
            return String::new();
        };
        let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        if s.write_all(req.as_bytes()).is_err() {
            return String::new();
        }
        s.flush().ok();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).to_string()
    }

    /// 真端口 + 真 POST：字节要落进收件目录，回话要说清存成了什么名。
    /// 这一条替代"用浏览器点一下"——协议本身比手点可靠，而且它留在门里。
    #[test]
    fn a_real_post_lands_the_bytes_in_the_inbox() {
        let dir = tmp("post");
        let inbox = dir.join("inbox");
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        let srv = start(hub.clone(), "127.0.0.1", 0, false).expect("测试前提：要起得来");

        let (status, body) = http_post(
            srv.port,
            &format!("/put?k={tok}&n=手机拍的照片.txt"),
            b"the bytes from the phone",
        );
        assert!(status.starts_with("HTTP/1.1 200"), "上传没被收：{status}");
        assert!(
            body.contains("手机拍的照片.txt"),
            "回执没报存成什么：{body}"
        );
        assert_eq!(
            std::fs::read(inbox.join("手机拍的照片.txt"))
                .ok()
                .as_deref(),
            Some(&b"the bytes from the phone"[..]),
            "盘上的字节与送来的不等"
        );
        assert_eq!(hub.received().len(), 1, "收到的那一件要报给界面");

        // 同一张图再投一次：不许洗掉第一份（回执里也得是第二个名字）。
        let (status2, body2) = http_post(
            srv.port,
            &format!("/put?k={tok}&n=手机拍的照片.txt"),
            b"a different photo",
        );
        assert!(status2.starts_with("HTTP/1.1 200"), "{status2}");
        assert!(body2.contains("(2)"), "第二份没让路：{body2}");
        assert_eq!(
            std::fs::read(inbox.join("手机拍的照片.txt")).unwrap(),
            b"the bytes from the phone",
            "第二份把第一份覆盖了"
        );
        drop(srv);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 手机页上的 JS 要 POST 到 `/put` 并带上 `n=`，否则点了发送也是死的。
    /// （真浏览器里跑这段 JS 没做——`fetch` 与 `File` 是平台保证，
    /// 但"页面指向的路径与参数对不对"这件事可以在这里钉。）
    #[test]
    fn the_page_posts_to_the_route_that_exists() {
        let html = page_html("TOK", &[]);
        assert!(
            html.contains("/put?k=") && html.contains("&n="),
            "页面没往 /put 带 n= 发：{html}"
        );
        assert!(
            html.contains("method:'POST'"),
            "页面的发送不是 POST：{html}"
        );
        assert!(html.contains("var K='TOK'"), "token 没递给页面脚本：{html}");
    }

    /// 没有 JS 的那条腿要能落。这是本轮的核心断言：原生表单送的多部分体，
    /// 必须和 `/put` 走**同一个收口**（同名让路、报给界面都成立）。
    #[tokio::test]
    async fn a_plain_form_without_javascript_lands_in_the_same_inbox() {
        let inbox = tmp("nojs");
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        let body = concat!(
            "--B\r\n",
            "Content-Disposition: form-data; name=\"f\"; filename=\"手机里的文档.txt\"\r\n",
            "Content-Type: text/plain\r\n\r\n",
            "bytes from the form\r\n",
            "--B--\r\n",
        );
        let (code, text) = ask_form(hub.clone(), &format!("/send?k={tok}"), body.as_bytes()).await;
        assert_eq!(code, StatusCode::OK, "原生表单没被收：{text}");
        assert_eq!(
            std::fs::read(inbox.join("手机里的文档.txt"))
                .ok()
                .as_deref(),
            Some(&b"bytes from the form"[..]),
            "落盘的字节与表单里的不等"
        );
        assert_eq!(hub.received().len(), 1, "这条腿也要报给界面");

        // 同名再来一份：让路，不许洗掉第一份。
        let (code2, _) = ask_form(hub.clone(), &format!("/send?k={tok}"), body.as_bytes()).await;
        assert_eq!(code2, StatusCode::OK, "第二份没让路：{code2}");
        assert_eq!(
            std::fs::read(inbox.join("手机里的文档.txt"))
                .ok()
                .as_deref(),
            Some(&b"bytes from the form"[..]),
            "第二份把第一份洗掉了"
        );
        assert_eq!(hub.received().len(), 2, "两份都要报给界面");
        std::fs::remove_dir_all(&inbox).ok();
    }

    /// 两个文件一块送。分隔符的搜索必须**从每一块自己的位置**起——
    /// 写成从 0 起的话第二块永远找不到，还悄悄只收第一块。
    #[tokio::test]
    async fn two_files_in_one_form_both_land() {
        let inbox = tmp("two");
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        let body = concat!(
            "--B\r\nContent-Disposition: form-data; name=\"f\"; filename=\"一.txt\"\r\n\r\n1\r\n",
            "--B\r\nContent-Disposition: form-data; name=\"f\"; filename=\"二.txt\"\r\n\r\n2\r\n",
            "--B--\r\n",
        );
        let (code, _) = ask_form(hub.clone(), &format!("/send?k={tok}"), body.as_bytes()).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            std::fs::read(inbox.join("一.txt")).ok().as_deref(),
            Some(&b"1"[..])
        );
        assert_eq!(
            std::fs::read(inbox.join("二.txt")).ok().as_deref(),
            Some(&b"2"[..]),
            "一单里第二个文件没落地"
        );
        assert_eq!(hub.received().len(), 2, "两份都要报给界面");
        std::fs::remove_dir_all(&inbox).ok();
    }

    #[test]
    fn the_form_posts_to_the_route_that_exists() {
        let html = page_html("TOK", &[]);
        assert!(
            html.contains("enctype=multipart/form-data"),
            "这条腿不是原生表单发的 enctype：{html}"
        );
        assert!(
            html.contains("action=\"/send?k=TOK\""),
            "表单没指向 /send，或没带 token：{html}"
        );
        // 没有 name= 的 input 在 multipart 里根本不会出现，等于静默丢件。
        assert!(
            html.contains("name=f"),
            "表单的文件框没有 name，浏览器不会把它放进单子里：{html}"
        );
    }

    /// 路由挂没挂上：挂错的地方会回 404，而不是我们对单子内容的抱怨。
    #[tokio::test]
    async fn the_send_route_is_mounted() {
        let hub = Arc::new(Hub::new(tmp("mount")));
        let tok = hub.token().to_string();
        let (code, _) = ask_form(hub, &format!("/send?k={tok}"), b"--B--\r\n").await;
        assert_ne!(
            code,
            StatusCode::NOT_FOUND,
            "/send 没挂上路由（404 就是没挂）"
        );
    }

    /// 这条腿同样得先过 token：不然"不装 app 也能送"就变成了"不装 app 也能偷投"。
    #[tokio::test]
    async fn the_form_leg_still_needs_the_token() {
        let inbox = tmp("formtoken");
        let hub = Arc::new(Hub::new(inbox.clone()));
        let body = concat!(
            "--B\r\nContent-Disposition: form-data; name=\"f\"; filename=\"偷投.txt\"\r\n\r\nx\r\n",
            "--B--\r\n",
        );
        let (code, _) = ask_form(hub.clone(), "/send", body.as_bytes()).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "没 token 的表单居然收了");
        assert!(
            std::fs::read_dir(&inbox).map(|rd| rd.count()).unwrap_or(0) == 0,
            "没收下却动了收件目录"
        );
        std::fs::remove_dir_all(&inbox).ok();
    }

    /// 文件名是 `Content-Disposition` 里的外部输入，必须走同一个 `sanitize_name`。
    #[tokio::test]
    async fn a_traversal_filename_from_the_form_cannot_escape_the_inbox() {
        let inbox = tmp("formpath");
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        let body = concat!(
            "--B\r\nContent-Disposition: form-data; name=\"f\"; filename=\"..\\\\win.txt\"\r\n\r\nx\r\n",
            "--B--\r\n",
        );
        let (code, _) = ask_form(hub.clone(), &format!("/send?k={tok}"), body.as_bytes()).await;
        assert_eq!(code, StatusCode::OK, "带路径的名字应被压平后收下：{code}");
        assert!(inbox.join("win.txt").exists(), "压平后要落在收件目录里");
        let mut names: Vec<String> = std::fs::read_dir(&inbox)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["win.txt".to_string()],
            "收件目录里只该有那一个压平后的件，多一个就说明名字跑出去了"
        );
        std::fs::remove_dir_all(&inbox).ok();
    }

    #[test]
    fn boundary_survives_the_shapes_browsers_actually_send() {
        assert_eq!(
            multipart_boundary("multipart/form-data; boundary=B").as_deref(),
            Some("B")
        );
        assert_eq!(
            multipart_boundary("multipart/form-data; boundary=\"QQ\"").as_deref(),
            Some("QQ")
        );
        // 安卓上常见的就是这种又长又带额外参数的串
        assert_eq!(
            multipart_boundary(
                "Multipart/Form-Data; charset=utf-8; boundary=----WebKitFormBoundaryAb12"
            )
            .as_deref(),
            Some("----WebKitFormBoundaryAb12")
        );
        assert_eq!(multipart_boundary("text/plain"), None);
        assert_eq!(multipart_boundary("multipart/form-data"), None);
    }

    /// 纯函数层的那把锁：body 里出现 `--B` 但不带前导 CRLF，不许被当成分隔符切开。
    #[test]
    fn content_that_looks_like_a_boundary_does_not_split_early() {
        let body = concat!(
            "--B\r\nContent-Disposition: form-data; name=\"f\"; filename=\"a.txt\"\r\n\r\n",
            "line one\r\n--B is inside the file\r\nline two\r\n",
            "--B--\r\n",
        );
        let parts = parse_multipart("B", body.as_bytes());
        assert_eq!(parts.len(), 1, "内容里的伪分隔符把一块切开了：{parts:?}");
        assert_eq!(
            String::from_utf8_lossy(&parts[0].data),
            "line one\r\n--B is inside the file\r\nline two",
        );
    }

    #[tokio::test]
    async fn a_non_multipart_body_on_the_form_leg_is_refused_not_guessed() {
        let hub = Arc::new(Hub::new(tmp("ct")));
        let tok = hub.token().to_string();
        let req = Request::builder()
            .uri(format!("/send?k={tok}"))
            .method("POST")
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from("x"))
            .unwrap();
        let res = router(hub).oneshot(req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "不是 multipart 要明说"
        );
    }

    /// 这条腿是**整块进内存**的，所以天花板必须生效——写成 `usize::MAX`
    /// 就等于让对面一句 POST 把这台机器撑爆。
    #[tokio::test]
    async fn the_form_leg_refuses_a_whole_order_over_the_cap() {
        let inbox = tmp("formcap");
        let hub = Arc::new(Hub::new(inbox.clone()).with_max_bytes(8));
        let tok = hub.token().to_string();
        let body = concat!(
            "--B\r\nContent-Disposition: form-data; name=\"f\"; filename=\"大.txt\"\r\n\r\n",
            "0123456789ABCDEF\r\n--B--\r\n",
        );
        let (code, _) = ask_form(hub.clone(), &format!("/send?k={tok}"), body.as_bytes()).await;
        assert_eq!(
            code,
            StatusCode::PAYLOAD_TOO_LARGE,
            "整单超上限必须当场拒，不许缓冲下来再写"
        );
        assert!(
            std::fs::read_dir(&inbox).map(|rd| rd.count()).unwrap_or(0) == 0,
            "拒了却还在收件目录里留了东西"
        );
        std::fs::remove_dir_all(&inbox).ok();
    }

    /// 浏览器与手机之间那一跳随时断。收尾的 `--B--` 没送到时，**已经到达的那份**
    /// 必须收下——写成"整单作废"会把手机已经发完的件吞掉。
    #[tokio::test]
    async fn a_form_cut_off_before_the_closing_delimiter_keeps_the_arrived_file() {
        let inbox = tmp("trunc");
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        // 注意：没有结尾的 `--B--`，第二块直接断在体里。
        let body = concat!(
            "--B\r\nContent-Disposition: form-data; name=\"f\"; filename=\"半截.txt\"\r\n\r\n",
            "已经到齐的字节",
        );
        let (code, _) = ask_form(hub.clone(), &format!("/send?k={tok}"), body.as_bytes()).await;
        assert_eq!(
            code,
            StatusCode::OK,
            "收尾没送到不该作废已经到齐的件：{code}"
        );
        assert_eq!(
            std::fs::read(inbox.join("半截.txt")).ok().as_deref(),
            Some("已经到齐的字节".as_bytes()),
            "半截判定要的是**内容整份在**，不是文件在不在"
        );
        std::fs::remove_dir_all(&inbox).ok();
    }

    /// `filename=""` 不是文件。把它当文件收，就会往收件目录写一个空名字件。
    #[tokio::test]
    async fn an_empty_filename_in_the_form_is_not_treated_as_a_file() {
        let inbox = tmp("emptyname");
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        let body = concat!(
            "--B\r\nContent-Disposition: form-data; name=\"k\"; filename=\"\"\r\n\r\nv\r\n",
            "--B--\r\n",
        );
        let (code, _) = ask_form(hub.clone(), &format!("/send?k={tok}"), body.as_bytes()).await;
        assert_eq!(
            code,
            StatusCode::BAD_REQUEST,
            "整单里只有非文件块，要说\"这一单里没有文件\"，不许当收下了"
        );
        assert!(
            std::fs::read_dir(&inbox).map(|rd| rd.count()).unwrap_or(0) == 0,
            "空名字那块被当成文件落盘了"
        );
        std::fs::remove_dir_all(&inbox).ok();
    }

    /// 游标推进的口径：停在下一个分隔符**前面的 CRLF** 上。
    /// 多走一步就从分隔符中间起跑，后面的块全瞎——单块样本看不出来，所以单独钉。
    #[test]
    fn the_cursor_lands_where_the_next_delimiter_is_recognisable() {
        let crlf = "\r\n";
        let h1 = "Content-Disposition: form-data; name=\"f\"; filename=\"c1.txt\"\r\n\r\n";
        let h2 = "Content-Disposition: form-data; name=\"f\"; filename=\"c2.txt\"\r\n\r\n";
        let one = format!("--B{crlf}{h1}A{crlf}--B{crlf}{h2}B{crlf}--B--{crlf}");
        assert_eq!(
            parse_multipart("B", one.as_bytes()).len(),
            2,
            "两块都得认出来"
        );
        // 同一段内容但分隔符长不一样：推进量若是写死的常数，这里就会错位。
        let two = format!("--LONG-B{crlf}{h1}A{crlf}--LONG-B{crlf}{h2}B{crlf}--LONG-B--{crlf}");
        let got = parse_multipart("LONG-B", two.as_bytes());
        assert_eq!(got.len(), 2, "长分隔符下推进量算错就只剩一块：{got:?}");
        assert_eq!(got[1].data, b"B", "第二块的内容错位了：{:?}", got[1].data);
    }

    /// 分隔符偏移 `+2` 的判决性样本：**带前言的**单子才走得到那条分支
    /// （游标正常时总是正好停在 `--` 上，另一条支路被 if 吃掉——所以单靠
    /// 两块的样本杀不掉这把刀，这是本轮补的）。
    #[test]
    fn a_body_with_a_preamble_still_splits_at_the_first_real_delimiter() {
        let crlf = "\r\n";
        let b = format!(
            "this is the preamble a mailer may prepend{crlf}--B{crlf}\
             Content-Disposition: form-data; name=\"f\"; filename=\"q1.txt\"{crlf}{crlf}\
             7{crlf}--B--{crlf}"
        );
        let got = parse_multipart("B", b.as_bytes());
        assert_eq!(got.len(), 1, "前言之后的第一个分隔符没被认出：{got:?}");
        assert_eq!(got[0].filename.as_deref(), Some("q1.txt"));
        assert_eq!(got[0].data, b"7", "内容错位了：{:?}", got[0].data);
    }

    /// 分隔符后面必须紧跟 CRLF 或 `--`。只有裸 LF 的（有人手工改过的、非 RFC 的）
    /// 单子要**判死**，不许靠后面的碰巧对齐蒙出一块来。
    #[test]
    fn a_malformed_delimiter_tail_is_refused_not_resynthesised() {
        let b = "--B\nContent-Disposition: form-data; name=\"f\"; filename=\"m1.txt\"\n\ndata\n";
        assert!(
            parse_multipart("B", b.as_bytes()).is_empty(),
            "非 RFC 的换行形状不该被切成块：{:?}",
            parse_multipart("B", b.as_bytes())
        );
    }

    /// 前言（有的邮件器/壳子浏览器会塞一行说明）之后收尾又断了：
    /// 这时**不许**把前言当成一块收下来——判红的事要报 400，不是静默写个怪件。
    #[tokio::test]
    async fn a_preamble_followed_by_a_cut_off_tail_is_not_a_file() {
        let inbox = tmp("pretrunc");
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        let body = "this is a preamble\r\n--B\r\nContent-Disposition: form-data; name=\"f\"; filename=\"q.txt\"\r\n\r\nhello";
        let (code, _) = ask_form(hub.clone(), &format!("/send?k={tok}"), body.as_bytes()).await;
        assert_eq!(code, StatusCode::OK, "到了尾块的件该收下：{code}");
        assert_eq!(
            std::fs::read(inbox.join("q.txt")).ok().as_deref(),
            Some("hello".as_bytes()),
            "前言被当成内容吃进来了，或者件没落地"
        );
        std::fs::remove_dir_all(&inbox).ok();
    }

    /// 真 socket 上的原生表单：这一条替掉了"手机浏览器点一次"里**能在本机造出来的
    /// 那一半**（分块传输、Content-Type 头、 multipart 体走完整个 HTTP 栈）。
    /// 真机上还剩的只有：ROM 自带的壳子浏览器行为、以及第二台机器 + 防火墙。
    #[test]
    fn a_real_form_over_the_socket_lands_in_the_inbox() {
        let dir = tmp("sockform");
        let inbox = dir.join("inbox");
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        let srv = start(hub.clone(), "127.0.0.1", 0, false).expect("测试前提：要起得来");
        let boundary = "----QoderFormBoundary";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"f\"; \
             filename=\"真表单.txt\"\r\nContent-Type: text/plain\r\n\r\n\
             over the socket\r\n--{boundary}--\r\n"
        );
        let (status, resp) = http_post_with(
            srv.port,
            &format!("/send?k={tok}"),
            body.as_bytes(),
            &format!("multipart/form-data; boundary={boundary}"),
        );
        assert!(
            status.starts_with("HTTP/1.1 200"),
            "真表单没被收：{status} {resp}"
        );
        assert_eq!(
            std::fs::read(inbox.join("真表单.txt")).ok().as_deref(),
            Some("over the socket".as_bytes()),
            "落盘的字节与表单里的不等"
        );
        // 没有 token 的那一次：当场拒，而且收件目录里不许多出一个件。
        let (status2, _) = http_post_with(
            srv.port,
            "/send",
            body.as_bytes(),
            &format!("multipart/form-data; boundary={boundary}"),
        );
        assert!(
            status2.starts_with("HTTP/1.1 401"),
            "没 token 的表单要拒：{status2}"
        );
        assert_eq!(
            std::fs::read_dir(&inbox).map(|rd| rd.count()).unwrap_or(0),
            1,
            "拒掉的那一单不该在收件目录里留东西"
        );
        drop(srv);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 回执页会把文件名拼进 HTML，两个分支都必须转义。
    ///
    /// 🔴 两个坑都在这条门里量过：
    /// ① 上一版只转义了失败分支；成功分支是**碰巧安全**的——Windows 文件名不允许
    ///    `< > " | ? *`，带 `<script>` 的名字根本建不出文件，于是走的是失败分支。
    ///    换个平台（Linux）就不成立了，所以转义照加。
    /// ② 第一版载荷 `<script>a</script>.txt` 带 `/`，被 `sanitize_name` 从最后一段截成
    ///    `script>.txt`，载荷自己先死 ⇒ 门看着绿、其实什么都没证。
    /// 所以这里用**Windows 合法**的 `&` 证明成功分支确实转义了，再用带 `:` 的名字
    /// （会被 sanitize 拒）打失败分支。
    #[tokio::test]
    async fn the_receipt_page_escapes_the_reported_name() {
        let inbox = tmp("xss");
        let hub = Arc::new(Hub::new(inbox.clone()));
        let tok = hub.token().to_string();
        // `&` 在 Windows 文件名里合法 ⇒ 真能落地；转义后必须是 `&amp;`。
        let body = concat!(
            "--B\r\nContent-Disposition: form-data; name=\"f\"; filename=\"a&b.txt\"\r\n\r\nx\r\n",
            "--B--\r\n",
        );
        let (code, html) = ask_form(hub.clone(), &format!("/send?k={tok}"), body.as_bytes()).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            std::fs::read_dir(&inbox).map(|rd| rd.count()).unwrap_or(0),
            1,
            "载荷没落地 ⇒ 这条门证不到东西，别当它绿了"
        );
        assert!(
            !html.contains("a&b.txt"),
            "成功分支把文件名原样拼进了 HTML：{html}"
        );
        assert!(
            html.contains("a&amp;b.txt"),
            "成功分支要的是转义过的名字：{html}"
        );

        // 失败分支：带 `:` 的名字会被 sanitize 拒，而 `raw` 在这里被原样打印过。
        let body2 = concat!(
            "--B\r\nContent-Disposition: form-data; name=\"f\"; filename=\"<b>x</b>:bad\"\r\n\r\nx\r\n",
            "--B--\r\n",
        );
        let (_, html2) = ask_form(hub, &format!("/send?k={tok}"), body2.as_bytes()).await;
        assert!(
            !html2.contains("<b>x"),
            "失败分支把原样的名字拼进了 HTML：{html2}"
        );
        assert!(
            html2.contains("&lt;b&gt;"),
            "失败分支也要给转义过的名字：{html2}"
        );
        std::fs::remove_dir_all(&inbox).ok();
    }

    /// 这条腿的上限**不跟着** `/put` 的 2 GiB 走：它是整块进内存的。
    /// 用真常数打一发，别让 `min(SEND_MAX_BYTES)` 写成一句注释。
    /// （不另写 `SEND_MAX_BYTES < DEFAULT_MAX_BYTES` 那种断言：clippy 会说它是常量断言，
    /// 而且这条行为门本身就能把三个方向的变异都判红。）
    #[tokio::test]
    async fn the_form_leg_has_its_own_lower_ceiling() {
        let hub = Arc::new(Hub::new(tmp("sendcap")));
        let tok = hub.token().to_string();
        let req = Request::builder()
            .uri(format!("/send?k={tok}"))
            .method("POST")
            .header(header::CONTENT_TYPE, "multipart/form-data; boundary=B")
            .body(Body::from(axum::body::Bytes::from(vec![
                b'x';
                SEND_MAX_BYTES
                    as usize
                    + 1
            ])))
            .unwrap();
        let res = router(hub).oneshot(req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "超过这条腿的上限要当场拒，不许先缓冲下来"
        );
    }

    /// 一条带 Content-Type 的 POST（模拟原生表单提交）。
    async fn ask_form(hub: Arc<Hub>, uri: &str, body: &[u8]) -> (StatusCode, String) {
        let req = Request::builder()
            .uri(uri)
            .method("POST")
            .header(header::CONTENT_TYPE, "multipart/form-data; boundary=B")
            .body(Body::from(body.to_vec()))
            .unwrap();
        let res = router(hub).oneshot(req).await.unwrap();
        let code = res.status();
        let text =
            String::from_utf8_lossy(&to_bytes(res.into_body(), 1 << 20).await.unwrap()).to_string();
        (code, text)
    }

    /// 一行 HTTP/1.1 POST。返回（状态行, 响应体）。
    fn http_post(port: u16, path: &str, body: &[u8]) -> (String, String) {
        http_post_with(port, path, body, "application/octet-stream")
    }

    /// 同上，但 Content-Type 自己给——原生表单那条腿必须是 `multipart/form-data`，
    /// 这一条走的是真 socket，不经 tower 的 `oneshot`。
    fn http_post_with(port: u16, path: &str, body: &[u8], content_type: &str) -> (String, String) {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("连不上");
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\
             Content-Type: {content_type}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body).unwrap();
        s.flush().unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).unwrap();
        let all = String::from_utf8_lossy(&buf).to_string();
        let mut parts = all.splitn(2, "\r\n\r\n");
        let head_part = parts.next().unwrap_or("");
        let body_part = parts.next().unwrap_or("");
        let status = head_part.lines().next().unwrap_or("").to_string();
        (status, body_part.to_string())
    }
}
