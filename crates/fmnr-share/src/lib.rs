//! 局域网投递：手机 ↔ 电脑之间**单向丢文件**。
//!
//! 边界先写死，免得下一个人把这条读成"跨设备同步做完了"：
//!
//! - 一次一个动作。手机把件投进电脑的收件目录，或电脑把**你手动登记过**的几个件
//!   取回手机。没有目录镜像、没有冲突合并、没有离线队列 ⇒ 不碰
//!   `docs/DESIGN.md` 第 70 行那句「不做文件内容同步」。
//! - 鉴权只有一把挂在 URL 里的随机 token（128 bit），**明文 HTTP**。
//!   同网段抓包就能拿到它，所以这是"自己家/办公室网"档位，不是公网服务。
//!   默认绑回环，对外开放要显式批准（闸在 `fmnr_core::net::bind_addr`，
//!   与 `fmnr-ai` 的 HTTP 面共用同一格）。
//! - 电脑侧只暴露登记过的件：`/get` **只认编号，不认路径**。这条是这一格里
//!   最要紧的不变量——一旦允许调用方给路径，手机就成了整台机器的读取口。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::header;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures::StreamExt;
use tokio::io::AsyncWriteExt;

/// 单个上传件的大小上限。手机拍一段 4K 视频能到几 GB，
/// 没有天花板等于让对面把这台机器的盘写满。
pub const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

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
fn url_escape(s: &str) -> String {
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

/// 收件：请求体**就是文件本身**（不引 multipart 解析器），名字走 `n=`。
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
    let Some(name) = sanitize_name(raw) else {
        return say(
            StatusCode::BAD_REQUEST,
            "这个名字我不能用：只收纯文件名，不带路径、盘符或控制字符。",
        );
    };
    if let Err(e) = std::fs::create_dir_all(&hub.inbox) {
        return say(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("收件目录建不出来：{e}"),
        );
    }
    // 挑名字到写完，整段串行（见 `create_lock`）。
    let _guard = hub.create_lock.lock().await;
    let dest = unique_dest(&hub.inbox, &name);
    let std_file = match fmnr_fs::ops::create_exclusive(&dest) {
        Ok(f) => f,
        Err(e) => {
            return say(
                StatusCode::CONFLICT,
                &format!("这个文件现在写不了（可能被别的程序开着）：{e}"),
            )
        }
    };
    let mut file = tokio::fs::File::from_std(std_file);
    let mut stream = req.into_body().into_data_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                discard(&dest).await;
                return say(StatusCode::BAD_REQUEST, &format!("读到一半断了：{e}"));
            }
        };
        written += chunk.len() as u64;
        if written > hub.max_bytes {
            discard(&dest).await;
            return say(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!(
                    "超过上限 {}，这份没收（半截的已删掉）。",
                    human_bytes(hub.max_bytes)
                ),
            );
        }
        if let Err(e) = file.write_all(&chunk).await {
            discard(&dest).await;
            return say(StatusCode::INTERNAL_SERVER_ERROR, &format!("写盘失败：{e}"));
        }
    }
    if let Err(e) = file.flush().await {
        discard(&dest).await;
        return say(StatusCode::INTERNAL_SERVER_ERROR, &format!("收尾失败：{e}"));
    }
    drop(file);
    let saved = dest
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or(name);
    hub.note_received(saved.clone(), written);
    (
        StatusCode::OK,
        format!("已存为 {saved}（{}）", human_bytes(written)),
    )
        .into_response()
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

    /// 一行 HTTP/1.1 POST。返回（状态行, 响应体）。
    fn http_post(port: u16, path: &str, body: &[u8]) -> (String, String) {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("连不上");
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\
             Content-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
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
