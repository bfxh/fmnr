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

**真窗口验到的文案**（2026-10-18，22:37 前后，语料 = `scan-demo` 里一个可读的
`visible-note.txt` + 一个 DACL 拒访的 `locked/hidden-note.txt`）：

- 搜 `note` → 「1 个结果 · 92 ms · 1 个目录没读到，结果不全」，列表只给可读那一条。
- 搜 `hidden`（只存在于拒访分支）→ 「0 个结果 · 6 ms · 1 个目录没读到，结果不全」
  加一句「「hidden」在能读到的部分里没有匹配项，但有 1 个目录没读到」。
  修之前这里说的是"在这个子树里没有匹配项"——会把用户引去改关键词，
  而真相是有一支压根没搜。

顺带结掉一个误判：程序起手目录是 **`USERPROFILE`（home）**，不是进程 CWD。
第一次观察看到 `local:E:\` 让我以为"启动不遵守 CWD 是缺陷"，把子进程的
`USERPROFILE` 指到语料后它精确开在那里 ⇒ 机制是 home 驱动、可确定的。

### §8 观感批补到的两张图（同日晚上，语料 = 三个长目录名 + 悬殊大小）

- **方块图标签裁切与三档降级**：`beta-another-extremely-long-branch-name` 那块画成
  「beta-another-extremely…」（裁在块内，不溢到隔壁），大块 `alpha-…` 给全名 +
  「4.8 MB · 70.2%」，最小的 `c`（约 1.7%）是底部一条细带**完全不写字** ⇒ 三档按设计走。
- **状态栏左段**：有状态时左段是正常色的「在 D:\…\scan-demo 中搜索」、右段
  「剪贴板为空」转弱色；无状态时左段整段不渲染 ⇒ 上一轮"状态为空时竖线孤零零"确实没了。

🔴 **一条自动化的边界，不是产品的**：Computer Use 这套**没有 hover 原语**
（只有 click / drag / scroll / press_key / type_text / set_value / perform_secondary_action），
所以方块图那句"挤不下名字的格子悬停能看到它是谁"**用现有自动化验不了**。
要验它得换路子（比如把 tooltip 文案抽成可单测的纯函数）——
别把"没拍到"读成"没做"，也别读成"做错了"。

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

🔴 这条**只有无窗口证据**：催绘位图与状态文本都判过，但"搬完之后弹出窗口自己
刷新了"没有在屏幕上拍到——要拍它得再走一遍跨窗口拖放，而那条路上一轮造成过
一次误投递。方块图 tooltip 里"这一支有 N 个目录没读到"同样只有数据层的门
（那个语料里拒访分支字节数为 0，画不出格子，hover 不到）。

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

### 5. `.lnk` 的目标在 `LinkInfo` 里，而字段顺序与常见转述相反

**怎么撞上的**：看桌面时随手点了个快捷方式，回头拿本机的 `WScript.Shell`
（Windows 自己的解析器）当 oracle 逐个对质我们的 `target_of`，才发现这个解析器
在真件上基本是错的。

**语料**：桌面 + 公共桌面 + 两个开始菜单（递归）+ `Recent` 去重后 235 个 .lnk，其中 **230 个当场可读**
（3 个在两次快照之间被卸载掉了，`UU远程.lnk`×2、`TRAE CN.lnk`），**183 个带结构完整的 LinkInfo**。
下表**三档都在同一个快照、同一份 oracle 上跑**（尺：沙盒 `crates/fmnr-pkg/tests/lnk_oracle_probe.rs`
+ 三把 Python 尺，`D:\KF\fmnr-sbx\.scratch\`）。

| 取法 | 对 | 错 | 报"没有" |
| --- | --- | --- | --- |
| 修前（只读 StringData 我以为的 DrivePath 那一格） | 26 | **155** | 49 |
| 只读 LinkInfo 的 LocalBasePath（不接后缀） | 154 | 26 | 50 |
| 本实现（LinkInfo + 后缀拼接规则） | **176** | 4 | 50 |
| 本实现再拼 IDList 最后一个 FileEntryItem 的文件名 | 175 | 5 | 50 |

修前那 155 个错答案的分类：**107 个返回的就是盘上一个真实存在的目录**、
20 个返回 `.ico`（IconLocation 那一格）、17 个给成前缀（oracle 在它下面）、其他 11 个。

**三条实测结论**（都由这几把尺钉住，不是照文档抄的）：

1. **StringData 根本没有 DrivePath 那一格**。五格是 Name / RelativePath /
   WorkingDir / CommandLineArguments / IconLocation。旧代码把 `0x80` 当
   "有 LocalBasePath"用，而 `0x80` 是 `IsUnicode`；`0x40` 是 HasIcon，
   旧代码把它当 EnableTargetMetadata。这一格错位就是把"解不出"变成"返回一个错
   答案"的元凶——155 个错答案里 107 个返回的就是盘上一个真目录。
2. **LinkInfo 头部的 `+8` 是 LinkInfoFlags、`+12` 是 LinkInfoHeaderSize**，
   与常见转述的先后相反。证据：183 个件里 `+8` 恒为小位掩码（1 有 160 个、3 有 23 个）、
   `+12` 恒为 28；`+16` 当偏移去指串时取出的正是 oracle 给的路径。
   按转述的顺序解，183 个一个都取不出。同一批件里 `+16`/`+24` 的偏移 0 个越出
   `LinkInfoSize`，所以按块尾收口不误伤真件。
3. **`CommonPathSuffix` 只在"不是尾巴"时拼上去**：`C:\Users\` + `dev\AppData\…\GitHubDesktop.exe`
   这种要拼（GitHub Desktop 那类），而 Blender 那种 LocalBasePath 已含整条路径、
   后缀只是末尾的 `exe`，拼了就错。这一条把只读 LocalBasePath 的 154 对提到 **176 对**。

**两条被推翻的说法（原文留着）**：
- "目标文件名只在 IDList 里，必须解 `LinkTargetIDList`" —— 那是拿**错位后的** StringData
  读法量出来的假象（当时 54 个桌面件里 29 个返回目录）。LinkInfo 读对以后拼 IDList
  名字是**净负**：176 对变 175 对，因为 LocalBasePath 本来就含文件名。
  IDList 解析因此一行都不需要，代码比修前**更短**。
- 本轮我自己写过一个"316 个真件"的说法，是双算（`Recent` 那把尺扫的 133 个
  与 oracle 语料大量重叠）。去重后的真实数是 235 个件 / 183 个带 LinkInfo，已更正。

**剩下 50 个报"没有目标"的**：一律是**没有 LinkInfo** 的件。量过兜底值：
用 RelativePath + .lnk 所在目录只多复原 7 条，且多数是 `%windir%\system32\narrator.exe`
这种要环境变量展开的系统入口 ⇒ 按范围硬约束不做，只把"不猜"钉成测试
（`never_invents_a_target_from_the_bytes_after_linkinfo`）。
那 4 条"错"是形如 `C:\Users\<装机那台机器的用户名>\…` 这种在别人机器上做出来的件，
Windows 会用 IDList 重新落到本机同名目录，我们不跟这个行为。

**门**：`crates/fmnr-pkg/src/lnk.rs` 单测 16 条 + `tests/lnk_fuzz.rs` 4 条。
9 条变异**全部判红**，且都红在断言消息上（无崩溃红、无编译不过逃逸）：
字段 16→12、24→20、后缀一律拼、允许偏移指回头部、不校验绝对路径、
编码签名恒判 ANSI、头长下限 28→4、ANSI 允许高位字节、不跳 IDList。
🔴 第一轮跑的时候有 **4 条逃逸**，原因是两条真缺陷：
(a) 夹具直接引用被测代码的常数，常数改错夹具跟着改 ⇒ 现在夹具里写**字面量** 16/24；
(b) `at >= end` 那道守卫是**死代码**（两条读法本身就被 `end` 收口）⇒ 删掉，
不留"看着安全其实不承重"的行。
另外把 Windows 自己写出来的 78 字节 LinkInfo 块（`Node.js command prompt.lnk`，
VolumeID 段抹平）当字面量钉进 `parses_a_real_windows_written_link_info_block`，
这样**不依赖本机文件**也能在 CI 上判红。

🔴 这轮还撞出一个关于"独立夹具"的教训：`fmnr-ui` 的 `shortcut_check_tests` 里
另有一个自拼的 `.lnk` builder，注释写着"和解析器各写一遍，builder 错了两边一起错
就测不出东西"。听起来对，**但它两个都是从同一段记错的规格推出来的**——
所以门全绿了五轮，而真件上 155 个错答案。改对解析器之后那三条 UI 门立刻判红，
才把这个假独立性暴露出来。结论：**各写一遍不叫独立，能从外部权威（真字节 / oracle）
倒推回来的才叫**；本轮起 `fmnr-pkg` 那条 real-bytes 门就是那个权威。

**已知边界**（写清没证到的）：183 个件的 LocalBasePath 全是 ANSI，
UTF-16 那一档只有合成夹具测过；`LinkInfoHeaderSize > 28`（Unicode 版偏移字段）
本机 0 件，**没实现也没猜位置**；带中文的 ANSI 路径按"宁可报没有"处理。
oracle 对照脚本与三把 Python 尺都在沙盒 `D:\KF\fmnr-sbx\.scratch\`，不入库（含本机路径）。

## 证否（跑了才敢说没有）

| 怀疑 | 尺 | 结果 |
| --- | --- | --- |
| 手写 `.lnk` 解析器面对畸形件会 panic | 137 次截断 × 64 组标志 × 42 组虚报长度 × 2 组字符串 + 3000 个随机字节 | **0 次 panic**。12 条 clippy 命中全是假阳（被 `.get()` / 短路挡住）。门转正为 `crates/fmnr-pkg/tests/lnk_fuzz.rs`。🔴 但这只证明"不崩"，**不证明"对"**：正确性是上面 §5 那 155 个错答案 |
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
- 评审代理另给的几条，逐条复算后的**实际处置**：
  - `app.rs` 白板载入失败静默而后续保存会覆盖 → **已修**。`BoardError::Io` 能区分
    `NotFound`（首次运行，正常，不报警）与权限/损坏（必须说，且说清"这次改动会覆盖它"）。
    门 `app::board_notice_tests::missing_is_silent_but_unreadable_is_not`。
  - `fmnr-archive` 的 `header.size().unwrap_or(0)` 会把损坏尺寸显示成 0 字节空文件
    → **前提不成立（实测推翻）**：把 tar 头块的 size 字段改成非法八进制后，
    tar 读取器在**更早的迭代阶段**就报
    `numeric field was not a number: zzzzzzzzzzz when getting size for one.txt`，
    卷直接返回 Err，根本走不到那一行。改动仍做了（`.ok()` 严格更对），
    但**没为它写门**——为一个不可达分支写测试就是假安全。
  - `startup.rs` 的 `mirror_entries().unwrap_or_default()` "会丢掉恢复入口"
    → **前提也不成立**：`read_run_values` 对"键不存在"同样返回 Err，
    而镜像键在用户**第一次关掉某个启动项之前本来就不存在**。
    那个 `unwrap_or_default()` 是承重的"还没有"，不是吞错。
    真要改进的话是让 `read_run_values` 分开 `ERROR_FILE_NOT_FOUND` 与拒绝访问——
    那是另一件事，登记不动。
  - `fmnr-pkg/registry.rs` 枚举被拒子键静默缺项：可达性已量过——本机三棵
    Uninstall 树 196 + 77 + 26 = 299 个子键，读属性失败 **0 个**（同一次测量里
    缺 `DisplayName` 的有 25 个）⇒ 健壮性缺口，不是这台机器上的在用缺陷；
    按"不可达兜底臂不写假测试"的规矩，不为它编门。
  - `ops.rs dir_stats` 吞错让"释放字节数"虚低、`usage.rs` 的
    `metadata().unwrap_or(0)`：**仍未动，但本轮把暴露面量出来了**（脚本
    `D:\KF\fmnr-sbx\.scratch\exposure*.py` + 两个沙盒探针）：
    - 构造面（icacls 拒访一个子目录，与 §1 那条门同一形状）：真值 4500 B / 2 文件，
      `dir_stats` 报 **500 B / 1 文件 ⇒ 虚低 88%**，而且调用方无从知道；
    - 自然面（本机真实树）：`Program Files (x86)` 2,671 层 + `ProgramData` 1,288 层 +
      `AppData\Roaming` 25,267 层 + `AppData\Local\{Packages,Microsoft}` 3,595 层
      = **29,626 层里打不开 0 层**；`14,373 个文件里 stat 失败 0 个`；
    - 超长路径**不是**这条来源：305 字符的三层嵌套目录 `read_dir` 照样 `Ok`，
      `dir_stats` 读数与真值一致（虚低 0）。
      ⇒ 结论：这是"**别的机器 / 非管理员身份下会真发生**"的缺口，本机不触发。
      要标注就得让它出通路——`OpReport.bytes` 已经在 MCP 的 JSON 里（`tools.rs:456`），
      所以"加一个 `missed` 字段"属于对外契约变更，与 #38、#64 的 `Volume::search`
      部分失败表达是同一批待拍板事项，本轮**没有**为一条本机 0 暴露的分支新写门。
