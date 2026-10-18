# 沙盒扫 bug 记录（2026-10-18）

在独立 worktree（`scan/bug-sweep`，基线 `4d9e4a9`）里用本机已有工具扫了一遍，
再把确证的缺陷修进 `main`。这份文档的存在理由：**同一把尺不必再量一次**，
以及**证否结论同样是产出**——没跑的东西要写清没跑。

## 用什么扫的

| 工具 | 结论 |
| --- | --- |
| `cargo clippy` 默认门 | 仓内 `scripts/gate.sh` 已经判，本轮**不是**新信息 |
| clippy restriction 级（`unwrap_used` / `expect_used` / `indexing_slicing` / `arithmetic_side_effects`） | 原始命中 1129 条，其中生产代码 250 条（其余 879 条在测试里，测试里 `unwrap` 是正当用法） |
| `cargo audit` | 1 条 vulnerability + 4 条 warning |
| `cargo clippy -W await_holding_lock -W suspicious_operation_groupings -W integer_division -W redundant_clone` | 见「未校准的刀」 |
| 自写尺（`.lnk` 模糊、dead-click 扫描、clippy 输出按 cfg(test) 分档） | 见下 |
| 只读评审代理（silent-failure 专项） | 交回 11 条，逐条复算，**它给的修法方向有一条被我实测推翻** |

本机**没有** `ocr` / `semgrep` / `gitleaks` / `trivy`。
本轮**没跑** `cargo mutants`、`llvm-cov`、`geiger`、`udeps`（要 nightly）、`deny`（无 `deny.toml`）：
物理内存只剩 0.92 GB（commit 头寸 8.49 GB），变异门一轮的成本在这台机器上压不动。

## 确证并已修的缺陷

### 1. jwalk 把「这一层读不了」写在父条目字段里，不是 `Err` 条目

先说**证否过程**：第一版探针拿 `C:\Windows\System32` 当语料，实测 10828 条、
**0 个错误**——那个根目录对这个进程根本不构成拒访，所以它证明不了可达性。
换成自己造的语料（父目录可读、子目录 `icacls /deny *S-1-1-0:(OI)(CI)R`）才量到：

```
迭代器：Ok 3 条 · Err 条目 0 条
被藏在父条目 read_children.error 的失败：1 条（拒绝访问 os error 5）
产品路径 LocalVolume::search("alpha") -> Ok(1 条)，而磁盘真值是 2 条
```

机制在库源码里：`jwalk-0.9.0/src/core/dir_entry_iter.rs:91-93`——嵌套目录的
`read_dir` 失败被写进父条目的 `read_children.error`，父条目照样以 `Ok` 交出。
`read_children` 是公开字段、`ReadChildren::error()` 是公开方法，**数据一直在手边，代码从没读过**。

由此两条推论都要记下来：

- `.filter_map(|e| e.ok())` **不是丢失点**。评审代理建议的"收集 Err"改法在这类目录上
  一行都不会多抓到——这条方向性错误是实测出来的，不是读代码读出来的。
- `crates/fmnr-ui/src/usage.rs` 原来那段 `Err(e) =>` 分支和它的注释
  「jwalk 把这一层读不了作为条目错误报出来」**是错的**：对嵌套目录它永不触发，
  所以方块图 tooltip 那句"这一支有 N 个目录没读到"实际不会涨。
  顺带修掉同一处的第二个错：`err_paths` 是 `HashSet` 而每次发布做 `+= 1`，
  同一个失败目录会被后续每次发布反复加一——扫描器把计数器当成了增量日志。

已修并把门钉住的三个调用点（都自建拒访语料、断言"记账"而不是断言"没崩"）：

- `crates/fmnr-ui/src/usage.rs` — `Bucket.errors` 真涨，`unreadable_subdir_is_counted_instead_of_looking_empty`
- `crates/fmnr-ui/src/search.rs` — 新增 `SearchState.unreadable`，面板改成「N 个结果 · M ms · K 个目录没读到，结果不全」，
  `unreadable_layer_is_reported_as_partial_not_as_no_match`
- `crates/fmnr-ui/src/dupes_run.rs` — 读不到的层进 `skipped` 清单，`unreadable_layer_is_listed_as_skipped_not_as_clean`

**没修的第四处**：`crates/fmnr-fs/src/lib.rs` 的 `LocalVolume::search`（MCP/HTTP/CLI 共用）。
它的签名是 `Result<Vec<VPath>>`，**没有位置**表达"有一支没搜"。要补就得动工具返回的 JSON 形状，
那是契约变更——与「只读卷 `Unsupported` 错误码」是同一类待拍板的事，登记在这里，不动。

### 2. deferred 子视口不随父视口重绘，弹出窗口搬完那句话上不了屏

egui 原话（`egui-0.31.1/src/viewport.rs:22`）：
「Deferred viewports are repainted independently of the parent viewport.」

`popwin.rs` 的搬运收尾只做了 `request_repaint_of(parent)`，而「复制 N 项 → 目录」
这句状态**只画在子窗口里**。无窗口量法：`Context::request_repaint_of` 会同步调用
`set_request_repaint_callback`（`context.rs:200`），所以两个视口 id 各有没有被点名能直接读出来：

```
状态行：复制 1 项 → …\fmnr-pop-repaint-…\to
被催重绘：子窗口=false 主窗口=true
```

修法是同一处补一句 `request_repaint_of(own)`；门是
`popwin::tests::transfer_completion_wakes_the_popped_viewport_too`，变异验证过：
删掉那一行 → `left: 2 right: 3` 判红。

> 探针自己也错过一次：直接调 `start_transfer` 时忘了生产调用方会先置 `busy = true`，
> 于是等待循环立刻退出、读到一个空状态——那次"失败"是探针的竞态，不是产品的缺陷。

### 3. `read_file` 的 `max_bytes` 没有服务端天花板

实测：`usize::MAX` 被 serde 照收（`usize::MAX ACCEPTED, bytes=4096`），
要 1 GiB 就把 2 MiB 文件整份读进 `Vec`。而 `default_max_bytes` 的注释写的理由正是
"别把一个 4 GB 的文件读进内存"——保护性默认值由调用方一句话关掉。

修：加 `READ_CAP = 64 MiB`，按截断处理并在 `truncated` 里说明，工具 schema 同步写上限。
顺手改掉同一分支里的 `String::from_utf8(buf.clone())`：在天花板大小的缓冲上再翻一倍内存。
门：`tools::tests::max_bytes_cannot_exceed_the_server_ceiling`（删掉 `.min(READ_CAP)` 判红，
实测 `left: Some(67174400)`）。

### 4. 远程卷从来不写 `Attr::Hidden`

`crates/fmnr-remote/src/volume.rs` 里 `Attr::` 只有 `Symlink` 和 `ReadOnly` 两处，
点文件只在 `!opts.show_hidden` 那一支按名字 `continue`。形状与本地卷
`1561693` 修掉的**完全同一条**（标注与筛选是两件事）。

修：抽 `remote_attrs(name, is_symlink, perms)` 纯函数（POSIX 约定：点开头即隐藏），
先标注再按显示开关筛。门 `dotfile_carries_hidden_while_write_bit_drives_readonly` 钉住判定本身；
🔴 **整条 listing 路径仍未在真实主机上点过**，这条只有纯函数层的证据。

## 证否（跑了才敢说没有）

| 怀疑 | 尺 | 结果 |
| --- | --- | --- |
| 手写 `.lnk` 解析器面对畸形件会 panic | 137 次截断 × 64 组标志 × 42 组虚报长度 × 2 组字符串 + 3000 个随机字节 | **0 次 panic**。12 条 clippy 命中全是假阳（被 `.get()` / 短路挡住）。门转正为 `crates/fmnr-pkg/tests/lnk_fuzz.rs` |
| egui 的 hover-Sense 死点击（`Label` 上挂 `clicked()`） | 自写尺，扫生产代码 | **0 命中，0 个未解析接收者**。这把尺先用两个诱饵证明过它会响（v1 漏了跨行 `ui.horizontal(...)`，修成取链首变量才两个都抓到） |
| 方块图除零 / NaN 标签 | 读码 + `total <= 0.0` 早退 + `squarify` 过滤非正值 | 挡得住，不可达 |
| 盘根时 `parent()` 为 `None` → 空路径当目录用 | 扫 `parent().unwrap_or_default()` 两个落点 | 都有 `is_empty()` 守卫或走 `Option` |
| 注册表/网络三条硬边界 | `HKLM` 只读、`RegDeleteTreeW` 只在 `HKEY_CURRENT_USER` 自己子键、`bind_addr` loopback 白名单 fail-closed | 成立，无违规 |
| `read_file` 的 `room` 下溢、`assoc.rs` 注册表缓冲、`board.rs` 的三处减法、`ops.rs dir_stats` | 逐个读守卫 | 全部守护到位（假阳） |

## 未校准的刀 / 待拍板

- `clippy::await_holding_lock` 报 0，但**我没有给它种过诱饵**，所以记为"未证"，不记为"没有"。
- `rsa 0.10.0-rc.18` 的 **RUSTSEC-2023-0071（Marvin 时序侧信道，无修复版本）**：
  由 `crates/fmnr-remote/Cargo.toml` 显式开 russh 的 `rsa` 特性引入（`cargo tree -i rsa` 实测：
  `rsa ← russh ← fmnr-remote` 和 `rsa ← ssh-key ← russh`）。
  要不要关掉 `rsa`（代价：RSA 形态的密钥认证没了）属于依赖决策，未动。
  另有 `paste` / `rustybuzz` / `ttf-parser` 三条 unmaintained + `inout` 一条 yanked。
- 跨窗口拖放写死 `ConflictPolicy::Skip`，而主窗口同一步走"让用户显式选"的冲突弹窗
  ⇒ 同一个手势两种政策。补弹窗要给子窗口加一套冲突 UI，属于设计决定，未动；
  现状至少把跳过数报在状态行里。
- 评审代理另给的几条（`fmnr-pkg/registry.rs` 枚举被拒子键静默缺项、
  `startup.rs` 的 `mirror_entries().unwrap_or_default()` 会丢掉恢复入口、
  `app.rs` 白板载入失败静默而后续保存会覆盖、`archive` 的 `size().unwrap_or(0)`）
  复核**成立但影响面小**，本轮没动，记在这里当下轮候选。
