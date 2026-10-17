# 界面缺口与下一步（2026-10-17 实测盘点）

这份文档不是愿望清单。每一条**现状**都有可复算的证据（grep 命中数、真窗口截图、
进程命令行），每一条**方案**都标了代价和"怎么算做完"。
凡是我没验过的，写在"待拍"里，不混进结论。

---

## 0. 方法（为什么这份盘点可信）

- **判"有没有"**：`grep` 源码，0 命中即没有。下表每条都附了 grep 的关键字。
- **判"能不能用"**：真跑 `fmnr-ui.exe`，用 Computer Use 逐条点击 + 截图回读，
  不看编译过就算数（上一轮"从没开过窗口"的教训）。
- **判"进程为什么多"**：`Get-CimInstance Win32_Process` 读命令行 + 父链归因，
  不靠 `tasklist` 的计数猜。
- 派出去的研究子代理**第一轮把 disktree 认错了对象**（见 §3.1）。
  它的结论我没有直接采用，凡是进这份文档的都用仓库实际内容复核过。

---

## 1. 缺口清单

本轮做完的两件已标出；其余仍是"没写"，不是"没做好"。

| 能力 | 证据（`grep` 关键字 / 实测） | 结论 |
|---|---|---|
| 右键上下文菜单 | ~~0 命中~~ → **已做**（`891663b`，11 个动词挂上行） | 行上菜单可用；**栏内空白处与栏头还没挂** |
| 选中项的完整路径 | 栏头只画目录路径 | **部分解决**：行 hover 有 tooltip、右键有"复制完整路径"（实测进系统剪贴板）；但界面上仍不常显 |
| 目录树侧栏 | ~~无树组件~~ → **已做**（`e18854d`，`tree.rs` + 9 条单测） | 只显示目录、懒加载、展开态按路径为键；特殊文件夹尚未挂为根 |
| 应用内拖拽（拖到对栏、拖出到系统） | `begin_drag\|drag_payload\|has_dragged_files` → **0 命中** | 只有**外部拖入**（`dropped_files`）。栏内/栏间拖动没有 |
| 特殊文件夹（下载/文档/图片/桌面） | `Downloads\|Documents\|known_folder` → **0 命中** | 没有。起步只有 `dirs_home()` |
| 代码预览高亮 | `syntect\|highlight` in `preview.rs` → **0 命中** | 纯文本 + 等宽。`syntect` 曾是文档里的假声称，已删（`Cargo.lock` 里 0 个） |
| git 状态（哪个仓库/分支/脏不脏） | `git` in ui/core → **0 命中** | 没有 |
| 开机自启 | `autostart\|CurrentVersion\\Run` → **0 命中** | 没有 |
| 包更新提示 | 无 | 没有 |
| 磁盘占用视图 | 无 | 没有 |


---

## 2. 🔴 顺带查出来的两个真缺陷（不是"缺功能"，是"现有功能是错的"）

### 2.1 打开未关联文件 = 静默 no-op + `cmd.exe` 进程泄漏

**链路**（三步都在源码里）：

1. `fmnr-core/src/open.rs::route_open` → 不可预览 ⇒ `OpenRoute::External(default_external_program())`
2. `default_external_program()` 在 Windows 返回 **`$COMSPEC`（即 `C:\WINDOWS\system32\cmd.exe`）**
3. `fmnr-fs/src/lib.rs::spawn_external_open` 拼 `"<exe>" "<文件>"` 交给 `CreateProcessW`

于是"打开一个没有关联程序的文件"实际执行的是 `cmd.exe "那个文件"`。
`cmd.exe` 拿到一个路径参数、**没有 `/c`**，不会去开它，也不会退出。

**实测到的后果**（本机，2026-10-17 02:3x）：

- 当前 14 个 `cmd.exe` 里 **5 个**的命令行是 fmnr 造的，形如
  `"C:\WINDOWS\system32\cmd.exe" "E:\tmp\fmnr-mcp-open-23556-1792175375663662700\thing.xyzzy"`
- `%TEMP%` 下 **46 个** `fmnr-mcp-open-*` 残留目录（MCP 契约测试每跑一次留一批）
- 这 5 个 cmd 的父进程**已经没了** ⇒ 孤儿，不会自己退

**它不是远程代码执行**——我做了对照实验：造了个 `probe.bat`（内容是把标记写进
`MARKER.txt`），走 CLI 的 `open`，**`MARKER.txt` 没有生成**。
所以 `cmd.exe "<路径>"` 没把那个脚本当命令跑起来。这个结论要留着，
免得下次有人按"能执行"去改，改错方向。

**但它是三件事**：
1. 用户视角"双击没反应"，且**没有任何提示**（静默失败）；
2. 每次泄漏一个永不退出的 `cmd.exe`（正是你问的"为什么有好多脚本在运行"的一部分）；
3. 角标会显示"已在 cmd.exe 中打开"，而那个 cmd 永远不死 ⇒ **角标在说谎**，
   而"进程退出角标消失"是 M3 的核心承诺。

🔴 **两处我先前判断错了，留在这里：**

- 我写过"顺带：契约测试要自己清临时目录"。**不对**——`Fixture` 本来就有 `Drop`
  在清。清不掉是因为**孤儿 cmd.exe 继承了 MCP server 的工作目录**
  （`Mcp::start` 用 `.current_dir(fixture目录)`），目录被占着，
  `remove_dir_all` 失败又被 `let _ =` 吞掉。
  ⇒ 残留目录和孤儿进程**是同一个根因的两个症状**，不是两个 bug。
  实证：杀掉那 7 个 cmd.exe 之后，54 个残留目录**一次全删干净、0 失败**。
- 更难看的一条：`fmnr-core/src/open.rs` 里原本有个
  `default_program_is_non_empty` 测试，**断言的正是这个缺陷行为**
  （`assert!(!default_external_program().is_empty())`）。
  所以"回退到 cmd.exe"不是漏网，是被测试钉成契约的——这也是为什么一直没人发现。
  我把它删了，换成 `default_program_asks_the_volume_to_resolve_it`
  （断空串 = "交给卷解析"，并写明为什么不许回退到 shell）。

**修法（本轮已做的部分）**：`default_external_program()` 不再返回 `$COMSPEC`
而是返回**空串**，含义是"交给卷去解析系统默认程序"；`LocalVolume` 拿到空程序时
**明确报 `Unsupported`**，不再猜。`Fixture::drop` 改成把清理失败
`eprintln!` 出来——静默的 `let _ =` 就是这次攒下 54 个目录的原因。

**验证（A/B 同尺）**：跑一整轮 `cargo test --workspace --locked`，
跑前 54 个 `fmnr-mcp-*` 目录 / 7 个 fmnr 造的 `cmd.exe`，
跑后 **54 / 7，增量为 0**；测试全绿。

**还没做的（P0b）**：真正去解析系统默认程序，让"双击一个 .docx"能开起来。
`MCP open` 工具的说明写的是"省略则用系统默认"，**这句今天是不真的**。
解析要处理的比想象中多，所以单独一步做，别顺手塞进 P0：
`HKCU\...\Applications\...UserChoice\ProgId` → `HKLM\SOFTWARE\Classes\<progid>\shell\open\command`
三级回落之外，还要 ① 展开 `%ProgramFiles%` 这类环境变量；
② 剥掉 `%1` / `%*` / `%l` 占位符（`spawn_external_open` 自己会追加目标文件，
不剥就会把字面量 `%1` 传给程序）；③ 拒绝 `rundll32`/UWP 激活器这类
"不是普通 exe + 参数"的命令；④ 无扩展名与未知扩展名分开报。


### 2.2 子树搜索没走卷抽象（已挂号 #26）

`fmnr-ui/src/search.rs:97` 用 `jwalk::WalkDir::new(&root)`。归档卷的根是**包内虚拟路径**，
zip 根甚至是空串 ⇒ `WalkDir::new("")` 遍历的是**进程当前目录**。

实测：挂上 `sample.zip` 后搜 `note`，包内有 `inside/a-note.txt`，界面报「0 个结果」。
**它把"我搜不了"说成了"这里没有"。**

本轮已加 `local_walkable()` 挡板：非本地卷拒绝开搜索并在状态栏说明原因
（2 条单测钉住：`archive_and_remote_volumes_are_not_local_walkable`、
`local_unc_and_wsl_roots_are_walkable`）。

🔴 **写这段时我下的结论比实际严重，这里更正**：我原本写"要接进 `Volume::list` 递归"，
读了一遍 trait 才发现 **`Volume::search` 早就在 trait 里**，而且
- `fmnr-fs`：jwalk 实现（和本地 UI 那条等价）
- **`fmnr-archive`：真实现**——直接在已读的包内条目索引上按前缀+子串匹配（`lib.rs:488`）
- `fmnr-remote`：**显式 `Unsupported`**（注释写明"代价高且无主机可测，宁可显式不支持"）

⇒ 归档卷**本来就搜得了**，缺的只是 UI 去调它。#26 因此从"造能力"降级成"改接线"。
一个要一起决定的取舍：`Volume::search` 返回 `Vec<VPath>`（一次给完、带 limit、
**没有 size 也没有命中区间**），而现在的 UI 搜索是**流式增量 + 高亮命中段 + 带大小**。
所以切过去会掉两样体验，要么给 trait 方法加一个流式变体，要么在 UI 里对返回结果
自己补 size/高亮。这是 #26 真正的工作量所在，不是"接个线"那么轻。


---

## 3. 磁盘占用视图：disktree 的核心逻辑怎么进来

### 3.1 先把对象搞对（我踩的坑，写下来免得再踩）

`tobi/disktree` 是 **Rust + GPUI 的 treemap 磁盘占用浏览器**：
`git remote -v` → `git@github.com:tobi/disktree.git`；`Cargo.toml` →
workspace = `crates/disktree-app` + `crates/disktree-core`，version **0.11.0**，
edition 2024，license **MIT**。

我派去研究的子代理第一轮把它描述成"单文件 Go 写的终端 ASCII 占比条工具，
没有缩放没有实时"——**与仓库实际内容不符**。它的对比表（ncdu/dust/baobab 那些）
方向对，但凡是它关于 disktree 本身的说法我都没采用。
教训：子代理给的"是什么"必须用 `git remote` + `Cargo.toml`/`package.json` 复核。

**本节证据分层**（免得读者把两种置信度混起来）：
- **我亲自复核过的**：仓库身份（`git remote -v`）、`disktree-core` 13,045 行、
  core 的依赖表里**没有 GPUI**、core 里 `grep watcher|notify` 无对应实现、
  `scan.rs:14-15` 的 "live progress counters + cooperative cancellation" 原句、
  论文署名（Squarified 的正确作者是 Bruls/Huizing/van Wijk，子代理写成
  "Brasso, van Wijk 2004" 是错的）。
- **来自研究回合、按行号引用但未逐条复读的**：§3.2 的参数表、§3.3 的
  6.5 s@16 / 3.4 s MFT / 299 MB 三个数字、§3.4 的 110 ms / 150 ms 轮询间隔。
  动手前值得再回读一次原文——它们是决策依据，别只信我转述。


### 3.2 值得抄的是 core 的不变量（这是"核心逻辑要，界面不抄"的具体内容）

摘自该仓 `AGENTS.md` 的 Invariants 一节，逐条对我们的意义：

1. **大小用 `st_blocks * 512`，不是 `st_size`**（除非显式要表观大小）。
   理由：删掉文件时**退回来的是这个数**。Windows 侧读目录报告分配大小，
   提权扫整块 NTFS 时直接从 MFT 读（`mft.rs`），连 ADS 一起算——
   "流会跟着文件一起走，所以该算"。**我们现在的 `Entry::size` 是表观大小**，
   做这个视图必须换口径，否则用户按它决定删什么，删完发现空间没回来。
2. **总量一律派生，绝不手工累加**。叶子 `bytes` 是唯一输入，
   `tree::aggregate` 自底向上算出每个目录的 `bytes/files/dirs`；
   硬链接去重是把重复叶子的 `bytes` **置零**，任何"直接改目录 bytes"的代码
   都会被聚合覆盖。⇒ 这条正好治"计数器和真值不一致"这类 bug。
3. **一个目录只有在它自己的扫描 *和* 所有子目录任务都完成后才建**
   （`PendingDir::pending` 里那个 `+1` 哨兵）。提前建会**静默丢掉整棵子树**，
   作者注明"这事发生过一次"。
4. **`Node` 要被实例化几千万次，所以它必须小**（有 `a_node_stays_small` 测试钉着）。
5. **只能删标记路径下的东西**；挂载点、根、home、home 的父目录、符号链接目标一律拒；
   Windows 上还额外拒 `FOLDERID_UserProfiles`（通常 `C:\Users`）里**直接**那一层。
6. **标记按绝对路径 key**，不按树里的位置 ⇒ 重扫后标记还在；
   `Marks::refresh` 重读大小、消失的自动摘掉。
7. **tile 的 crumbs 必须是绝对路径**（从被扫根算起）。相对 crumbs 在 `~` 看着对，
   下钻之后就悄悄指向别的目录了——**包括标记**。
8. **缩放只改视图变换**：布局在 base-space 像素里算并且**缓存**，
   `screen = (base - origin) * scale`（scale 1.0–5.0，`state.rs:1942-1969` 的
   `LayoutKey{crumbs, w.round(), h.round(), options, filter_epoch}` 命中缓存）。
   ⇒ 滚轮缩放不触发重排，只重画。
9. **状态栏不许声称量不到的节省**：投影只算"这块盘上被标记的字节"，
   盘内挂载点上的标记会显示但**不计进本盘可用空间**；最终数字来自删除前后的 `statvfs`。

另外这几条**具体参数**值得直接继承（都是量出来的，不是拍的）：

| 参数 | 值 | 出处 | 为什么是这个数 |
|---|---|---|---|
| 每层磁贴上限 | `max_children = 96`，**尾部合并成一个 `Others` 磁贴但面积保留** | `treemap.rs:72-78, 225-236` | 不是阈值截断——面积保留 ⇒ 用户看得见"这里还有 40% 没摊开" |
| 最小磁贴 | `min_tile = 5.0` px 直接丢弃 | `treemap.rs` | 画不下的磁贴只会造成点击歧义 |
| 默认绘制深度 | `max_depth = 3`，`[`/`]` 在 1..6 调 | `treemap.rs:127-139` | **这是"不改变画面也能看更深"的第二根轴**，见 §3.4 |
| 节点大小 | `Node` 固定 **88 B**，有测试钉住 | `tree.rs:430-436` | 要被实例化几千万次 |
| 去重结构 | 8 MiB 原子位图（<2^26）+ 64 分片哈希兜底 | `tree.rs:281-342` | 410 万 id：64 锁 150 ms vs 单锁 1.2 s |
| 排序 | 值降序，再按名 | `tree.rs:349-404` | 与 squarify 的"先排序"要求一致 |

第 8 条对我们特别划算：**白板视图已经有相机（`board_view::Camera` 的 pan/zoom +
`to_screen`/`to_canvas`）**，treemap 的缩放语义可以复用同一层，不必再造一套。

### 3.3 三种接入方式与代价

`disktree-core` 实测 **13,045 行**，依赖里**没有 GPUI**（`chrono`/`rayon`/`rustc-hash`
+ 平台段：`rustix` / `windows-sys` / `trash` / `winapi-util` / `sysinfo`），
所以它是真·UI 无关，可复用。

**先给一个必须让你知道的量纲**（这条直接决定要不要做全盘视图）：
`Node` 88 B × 一次真实扫描的条目数 ⇒ **340 万条目 ≈ 299 MB 常驻**
（`tree.rs:434` 的 88 B 是钉住的，条目数是它自己 README 里那台机器的实测规模）。
这台机器内存本来就紧（常驻只剩 2–3 GB、页文件在机械盘上），
所以"扫整块盘"这个动作要**默认关掉**，默认只扫当前目录子树 + 明确显示条目数与估算内存。

扫描侧的实测数字（照 dust 的做法再加东西，README:266-269 自己承认）：

- 一个根一个 `rayon::scope`，递归栈深度 O(1)；Windows 专用池**上限 16 线程**，
  实测 **340 万文件 6.5 s@16 vs 8.1 s@24**（`scan.rs:973-992`）⇒ 线程不是越多越好，
  这条对我们尤其重要（本机跑变异门时已经吃过"并发越高越慢"的亏）
- 提权读 **NTFS MFT** 而不是遍历：**3.4 s** vs WizTree 11.2 s vs 普通遍历约 20 s
  （README:320-328，`mft.rs`）⇒ 全盘视图要快，唯一杠杆是 MFT 直读，而那要管理员

| 方案 | 做法 | 代价 | 我会选它的条件 |
|---|---|---|---|
| A git 依赖 | `disktree-core = { git = ... }` | 最快拿到扫描+布局；但把 `rayon`/`sysinfo`/`trash`/`chrono` 拉回依赖树（**`rayon` 是我们上一轮刚当死依赖删掉的**），且版本跟着别人节奏 | 只想要"能看"，不在意依赖重量 |
| B vendored 子集 | 把 scan/tree/treemap 三个模块搬进来，MIT 署名 | 中；要自己维护与上游的差；但能剥掉 `sysinfo`/`trash`（我们已有自己的回收站路径） | 想要核心算法 + 保持依赖克制 |
| C 按不变量重写 | 用 `Volume::list` 递归自己实现，抄它的**规则**不抄代码 | 最大；但天然接进卷抽象（归档/WSL/SSH 也能扫），且能直接长出一堆单测 | 如果这个视图要成为一等公民，这是唯一不返工的路 |

**建议 C，但先做 A 的一次性 spike**：用 git 依赖把视图跑起来看两周，
确认可用性曲线值这个钱，再落到 C。spike 的成本是一个分支，返工可控。

（未核：`disktree-core` 有没有发到 crates.io。没有的话 A 只能走 git 依赖。）

### 3.4 "实时"这个词必须拆开——它的真实边界比想象的窄

**disktree 没有文件系统 watcher**（全仓 `Cargo.toml` 无 `notify`/watch 依赖，已核）。
它的"live"实际由四件事组成，其中只有一件是真·实时：

1. 扫描线程 + mpsc，UI **110 ms 定时轮询进度原子量**——但
   **树是整棵一次性落地的，扫描期间不渲染部分结果**（`state.rs:951-1027`）。
   ⇒ 所谓"进度实时"只是数字在动，画面要等扫完。
2. 手动 `r` 重扫；删除完成后再扫一次让数字对得上盘（README:201-202）。
3. **空闲空间表 150 ms / 1200 ms 轮询**（`state.rs:1043-1057`）——
   这才是它唯一真正一直在变的数字。
4. **加宽扫描时复用子树**（`Known{path,tree}`：`~`→`/` 只读 `~` 之外，
   "seconds instead of a full rescan"，README:288-292）。
   ⚠️ 它**故意不做失效**——测试注释明写"memoized 子树不该看见扫描后的改动"
   （`scan.rs:1792-1794`）。⇒ 复用换来的快，代价是"这部分数字是上次的"。

同类工具对照（**下钻/缩放到底是谁**，别糊在一起）：

| 工具 | 扫描 | 下钻 | 缩放 |
|---|---|---|---|
| disktree | 一次性全扫（加宽可复用） | 滚轮放大到目标恰好填满视口后**下一格就进入**，一进一穿连续；Enter/Backspace | **两条正交轴**：视图变换 scale 1.0–5.0（不重排）+ `[`/`]` 改绘制深度 |
| ncdu | 整棵树先建在内存 | right/enter 进、left 出 | 无缩放，靠层级切换；`r` 重算当前目录 |
| dust | 并行一次扫 | 终端条形图 | `--collapse` 折叠指定目录＝**显示期缩放，不省扫描** |
| QDirStat | 一次性后台扫 | 树 + treemap 双向联动 | **一格滚轮＝一层目录**；`MinTileSize` 默认 3 px，**切掉的小磁贴留灰块明示"这里还有东西，放大看"** |
| WinDirStat | 一次性全扫，带 pause/resume | 点磁贴聚焦子树 | README 声称 "Watch file-system changes"——**这一族里唯一提到 watcher 的** |
| duf | — | 表格，只到文件系统级 | 无层级无下钻，不属这一族 |

**QDirStat 那个"留灰块明示还有东西"**值得直接抄：它把"我画不下了"变成可见信息，
而不是静默丢弃——和 §2 里"不许把'我搜不了'说成'这里没有'"是同一条纪律。

所以"实时"三档，价格完全不同：

| 档 | 含义 | 代价 | 建议 |
|---|---|---|---|
| 实时-a | 进度数字在动、能取消 | 便宜（progress 原子量 + 协作取消，就是 disktree 这套） | 做 |
| 实时-b | 删除/移动后自动重扫受影响子树 | 中（脏区失效边界 + NTFS MFT 懒写那个坑） | 做，且**明说滞后** |
| 实时-c | 磁盘一变画面就变（watcher） | 贵：`ReadDirectoryChangesW` 线程 + 缓冲溢出重扫 + 每卷一个订阅 + 归档/远程卷语义完全不同；**且和"每帧立即模式重绘"的 egui 打架**：一次批量落盘会打爆重绘 | **第一版不做** |

### 3.5 落地时必须先解决的三个架构问题

1. **扫描层不用发明**：fmnr 已经有这个形状的现成范式——
   `fmnr-ui/src/search.rs:37-78`（后台线程 + mpsc 分批 `BATCH=200` + 主线程每帧
   `drain()` 拉一批）与 `:82-124`（专用命名线程，`send` 失败即退出＝天然取消），
   `app.rs` 用 `request_repaint` 驱动。磁盘扫描照抄这个骨架即可。
2. 🔴 **别用下标当路径键**。disktree 的 crumbs 是**子节点下标**，重排即失效，
   它靠"整树一次性落地" + "切量纲时按路径找回 crumbs"兜住。
   我们一旦做实时-b（增量重扫），子目录完成/重排会让下标漂移——
   **这是最先把正确性搞崩的点**。要么一开始就用绝对路径/稳定 id 为键
   （disktree 自己的**标记**就是这么做的，不变量 5），要么放弃增量。
3. **磁贴必须一次画，不能一个磁贴一个 widget**。disktree 不变量 6
   "treemap is painted, not composed of elements"在 egui 里更严：
   几千个 `allocate_rect` 会把响应式图压爆。走 `egui::Painter` 批量 rect +
   命中测试自己做（白板视图已经是这个形状，`render_board` 就是 Painter 画卡 +
   一次 `interact`）。**GPUI 那套 retained-mode `cx.notify()`/
   `request_animation_frame` 不要抄**，在 egui 里等价物就是 `request_repaint`。

### 3.6 文献（署名已复核）

- Shneiderman, B. (1992). *Tree Visualization with Tree-Maps: 2-d Space-Filling
  Approach*. J. Vis. Lang. Comput. 3(3):223–243 —— 原始 treemap；
  **它同时给了"嵌套矩形"这个表示和"点击下钻"这个交互**，正是你要的"缩小放大"的原型。
  代价：slice-and-dice 的长宽比会退化成细条。
- Bruls, Huizing & van Wijk (2000). *Squarified Treemaps*.
  Joint Eurographics–IEEE TCVG Symp. (VisSym), 33–42（[PDF](https://vanwijk.win.tue.nl/stm.pdf)）
  —— 专治长宽比：先排序 O(n log n)，行内贪心，每加一个候选重算一次 `worst()`。
  **disktree 的 `layout_squarified` 是它的逐字实现**（`treemap.rs:339-411`）。
  ⚠️ 后续技术报告证明**最坏情形长宽比仍无界** ⇒ 别把"接近方形"当保证写进文案。
- Ordered Treemaps（van Wijk & van de Wetering 1999；Lee/Segall/Shneiderman 2001）
  —— 解决"顺序本身有语义时不能按大小重排"。**我们按 size 降序排 ⇒ 无收益，不做。**
- TreeArcs（van Wijk & van Schaik, *Information Visualization* 1(2), 2002）
  —— 小值可读性更好，但不留满铺面积、每节点三角函数、resize 要重算。**不做。**

工程结论一句：**在"每帧重绘 + 窗口可随意 resize"的 egui 里，squarify 的正确用法是
让布局只在几何键变化时重算、resize 只改变换**（= 不变量 8）。
磁贴数被 `max_children=96`/层封顶 ⇒ O(n log n) 里的 n 是**可见孩子数**，
一帧重算也就几十到几百个矩形，**不必为布局做增量**。
`max_depth`（`scan.rs:56-59`，注释称能让巨树概览扫描变便宜）才是大目录的第一杠杆。


---

## 4. 包更新提示（UniGetUI 那一排）

### 4.1 先把对象锚定（本机实测，不是猜）

`C:\Users\lbx13\Desktop\UniGetUI.lnk` → `D:\rj\GJ\UniGetUI.x64\UniGetUI.exe`。
VersionInfo：**2026.3.1**，CompanyName **Devolutions Inc.**，签名有效
（GlobalSign EV，2028-12-05 到期）。`last-update-attempt.log` 写 build 107 /
**Auto-launch: False**。

**装法是便携目录**：Uninstall 注册表 0 命中、HKCU/HKLM Run 0 命中、
计划任务里没有 UniGet*/winget* ⇒ 不是 MSIX、不自启、不留装机痕迹。
二进制里带 `libSkiaSharp/libHarfBuzzSharp/av_libglesv2/e_sqlite3` = Avalonia 栈。

后端（日志里实测出现）：Winget、Chocolatey、Scoop、Pip、Npm、Bun、Cargo、
.NET Tool、PowerShell、PowerShell7。架构是**混合**：多数后端是 CLI 包装，
但 **winget 走自研 Pinget** —— `Settings\Pinget\sources\winget\index.db` +
`source2.msix` + `sources.json`（`eTag`/`sourceVersion`/`lastModified`）
= 自己实现预索引源协议，不依赖 `winget.exe`。

🔴 **一条必须写进设计的实测分歧**：同一台机器，**UniGetUI 报 winget 有 6 个可升级，
`winget` 自己报 11 个**。⇒ "有 N 个可更新"不是一个客观数，
它取决于**哪个源、索引新不新**。界面上任何这类数字都必须带
"截至 <时间> / 来自 <源>"，否则就是 §2.1 那种"把'我量不到'说成'没有'"的复发。

### 4.2 不要解析 `winget` 的 stdout

- `winget upgrade --list` **在这台机上不存在**（v1.29.380 报"无法识别参数名称: --list"）；
  `--output-format` 对 `upgrade`/`list` 也**不被识别**（issue #2595 已 closed as dup of #1753）。
- 能用的等价式是 `winget list --upgrade-available`（实测 11 行），但输出是
  **本地化表格**：表头中文、cp936 乱码、尾行"11 升级可用。" ⇒ 脆，不能当契约。
- `winget source list` 甚至不接受 `--accept-source-agreements`（实测）。
- COM 面（`WindowsPackageManager.dll` + `Microsoft.Management.Deployment.winmd`
  实测存在）里的 `GetAvailableUpgrades` **覆盖范围与可用性未证**（pwsh7 里类型解析失败）
  ⇒ 要用得 Rust 端亲测，不能先写进设计。

**结论**：走 UniGetUI 那条路（**直读预索引源 + 本地比对注册表**），
而不是"跑 winget 再解析文本"。

### 4.3 「.NET 要再点一次才更新」——你的直觉有具体机制

拆成五个成因，各自都有出处：

1. **UAC 提权**：无管理员时部分包要提权，运行安装器才弹（Learn 文档）。
   UniGetUI #5442 记批量升级"每包一次 UAC"且 elevator 竞态**提前返回成功**；
   winget-cli #3335 记提权窗口被最小化看不见 ⇒ 观感就是"点了没反应"。
2. 🔴 **MSI/Burn 的 3010**：`ERROR_SUCCESS_REBOOT_REQUIRED`。
   winget-cli #994 记录 **winget 把 3010 当失败**
   ⇒ "报失败但其实已装上，再点一次就显示'已最新'"——
   **这最像你说的那个现象的真身**，而且它不是 .NET 专属，是所有 MSI 类。
3. **.NET 特有的两件事**（本机实测）：
   - 注册表里同名多行且 **DisplayVersion 两套口径**（`8.0.31.36420` vs `64.124.58447`）
     ⇒ 按版本号字符串比对会算错；
   - **side-by-side**：8.0.31 与 8.0.32 并存、winget 仍列 8.0.31→8.0.32（不删旧）
     ⇒ "升级完它还在列表里"是正常的；
   - 清单实测：`Microsoft.DotNet.Runtime.8` 8.0.32 =
     **InstallerType: burn**、Scope: machine、Silent: `/quiet`、Custom: `/norestart`。
     Burn 内嵌 MSI，**dotnet 进程占用文件时会失败** ⇒ 要重试。
4. **静默参数但仍弹 UI**：有的包清单本身写 `SilentWithProgress: /passive`（实测）
   ⇒ 会弹进度窗，不是 bug。
5. **壳层升级 ≠ 软件更新**：Edge/VS Code/Qoder/Trae 自带 updater；
   本机 Run 键实测有 `QuarkUpdaterTaskUser…`、`FluxDown … --autostart`。

⇒ 1/2/3/5 **全部可以靠"只列出、把动作交回 winget/UniGetUI"规避**。
fmnr 不代跑升级。这是这一节最重要的决定。

### 4.4 「浏览到某软件目录 → EXE 上有更新提示」：准确率天花板 ~18%

实测 297 个 Uninstall 键：`InstallLocation` 只有 **73 个**（其中 68 个路径真存在）。
反查上限 **48/272 ≈ 18%**；已实测的坑：

- `TRAE CN (User)` 的 InstallLocation **目录已经不存在**；
- 大小写不符：注册 `CD Projekt RED` vs 真实 `CD Projekt Red`；
- HKLM/HKCU 双份且信息不一致（`Qoder 0.4.8` 无 loc + `Qoder IDE 1.106.3` 有 loc；`Zed`/`ZedG` 同）；
- 7-Zip 的 InstallLocation 里**只有 `7-zip.dll`，没有 exe**（20 个产品连 exe 都没有）；
- `.NET Runtime` 全无 InstallLocation；微信输入法在 Uninstall 里根本没有行。

索引代价也实测了：`C:\Program Files` 783 exe/2.2 s、`(x86)` 319/1.0 s、
`AppData\Local\Programs` 169/4.7 s、`D:\rj` **2826/35 s**
⇒ **绝不做全量索引**，只在进目录时定点查。

**判断**：这个提示只能做成**弱提示**——只有当目录能对上 `InstallLocation`/
`WindowsApps` 前缀**且**能对上 PackageIdentifier 时才打角标，**宁漏勿错**。
18% 命中意味着它永远覆盖不了大部分场景，所以它不能是"更新入口"的主路径，
只能是顺手。

### 4.5 该抄 UniGetUI 的四个具体机制

1. **节流快照**：它用 `Settings\Configuration\MaintenanceTaskLastRun.json`
   （实测内容 `{"check-updates":"2026-10-15T15:16:52Z"}`）⇒
   fmnr 用 `.fmnr/update-check.json` 存 `last_check` + 结果，
   **UI 秒开上次结果，后台刷新**，绝不在点击时现查网络。
2. **缓存"包 ID → 最新版本"映射**（清单里的 `PackageVersion` +
   `AppsAndFeaturesEntries.DisplayName`），与注册表 `DisplayVersion` **本地比对**，
   不跑 `winget.exe`。
3. 🔴 **把清单当风险标签源**——这条正好兑现你要的".NET 要小心"：
   `InstallerType`(burn/wix/msi) + `Scope`(machine) + `Silent` 开关
   可以**在列出时就预先把包标成"需提权 / 可能要再点一次 / 需重启"**，
   不用等它失败。这是可实现的，不是玄学。
4. **自身发版检查**：UniGetUI 是一次 GET `devolutions.net/productinfo.json`；
   fmnr 用 GitHub `Releases/latest` + 缓存即可。

**负面清单**（别人踩过的坑，别复刻）：#5442 批量提权竞态、
#4782 多版本只取第一条 ⇒ 保留"把动作交回 winget/UniGetUI"的降级路径。

**未找到可抄的先例**：Files / Rufus 这类里没有"文件管理器内嵌包更新提示"的实现
（Rufus 只查自身版本）⇒ 记为不存在/未证，别再花时间找。

### 4.6 界面形状

- 「下载 / 文档 / 图片 / 桌面」那一排**目前根本不存在**（§1 的 0 命中），
  所以「更新」入口是**先建那一排**才谈得上加进去。这两件事在 P1 里一起做。
- 「更新」页 = 上次快照 + 刷新时间 + 每项带风险标签（§4.5 第 3 条）+
  **一个"交给 winget/UniGetUI 执行"的跳转，而不是自己跑**。

---

## 5. 右键菜单：这一串诉求的公约数

你要的很多操作（打开方式、复制路径、扫描此目录、更新这个包、钉到白板、
在新窗口打开、属性、git 状态）本质是**同一批动词的不同挂载点**。
现在这些动词只有工具栏按钮和快捷键两个入口，而工具栏上一轮刚从"挤成一行"
改成"两行"——再加动词就没地方放了。

- 挂载点三个就够：**行上**（文件/目录）、**栏内空白处**（此目录）、**栏头**（卷/路径）。
- 菜单项必须复用 `fmnr-ai/src/tools.rs::dispatch_payload` 那张表——
  这是本仓既有纪律（"避免 CLI 能删、MCP 不能"）。右键不新建一套动作层。
- 判据：右键菜单里每一项，在 MCP/CLI 里有同名工具；反之工具里的动词
  要么在菜单里、要么在"更多"子菜单里，**不许有只有键盘能碰到的动作**
  （DESIGN 明确要规避"键位隐藏"）。

---

## 6. 其余各项的代价（一句话一条，按"值不值"排）

| 项 | 代价 | 值 |
|---|---|---|
| 选中项完整路径 | 极小（栏头下加一行，或状态栏左侧） | **高**：现在"我在哪"看得见、"这个文件到底在哪"看不见 |
| 特殊文件夹一行 | 小（Windows 走 `SHGetKnownFolderPath`，其余平台各有对应） | 高：这是"文件管理器及格线" |
| 右键菜单 | 中（见 §5） | 最高：一次性给十几个动词安家 |
| 应用内拖拽 | 中（egui 有 drag-and-drop payload API；拖出到系统要另做） | 中高：栏间拖动是双栏管理器的主交互 |
| 左侧导航树 | 中大（要决定它和双栏的关系：谁占位、要不要三态） | 中：**先问你要的是树还是"常用位置"**，两者不是一个东西 |
| 磁盘占用视图 | 大（§3） | 中高，且是你点名要的 |
| 包更新提示 | 中（§4，只读列出那半便宜） | 中 |
| 开机自启 | 小（HKCU 写一个值；**必须可逆**，见 §8） | 低中 |
| git 状态 | 中（读 `.git/HEAD` + 工作区脏检查不便宜；disktree 有 `git.rs` 407 行可参考） | 中 |
| 代码高亮 | 中（要么引 syntect 这类真依赖，要么走 WASM 插件宿主——**后者更贴本项目的设计**） | 中 |
| 包更新提示（只列不跑） | 中（§4.5 那四条机制；网络 + 缓存 + 一份清单解析） | 中 |
| 包更新提示（EXE 角标） | 小，但**准确率天花板 ~18%**（§4.4 实测） | 低：宁漏勿错，只能当顺手 |

---

## 7. 分期（每段都能"一天内验完"，判据写在里面）

- ✅ **P0 修错，不加功能**（`7497399`）：打开回退不再 spawn COMSPEC，改成明确报错；
  `Fixture::drop` 不再吞掉清理失败。
  判据实测过：跑一整轮 workspace 测试，`fmnr-mcp-*` 目录与 fmnr 造的 `cmd.exe`
  增量为 0（修前每轮都涨）。
  🔴 **代价要记着**：MCP `open` 工具说明里"省略 program 则用系统默认"这句**现在仍不真**，
  只是从"静默失败"变成"明确报错"。真能用是 P0b（#29）。
- **P1 及格线**：右键菜单 ✅（`891663b`，行上）；选中项完整路径 部分（tooltip +
  复制到剪贴板，界面上不常显）；特殊文件夹挂进树 ⬜。
  判据：三条都能在真窗口里点出来并截图；菜单动词与 MCP 工具表逐条对得上。
- **P2 主交互**：应用内拖拽（栏间移动/复制，带修饰键区分）。
  判据：拖到对栏后源栏不再显示该项（或显示副本），冲突走既有弹窗。
- **P3 磁盘占用视图**：先 spike（§3.3 方案 A）再落 C。
  判据：扫一个真目录，数字与 `st_blocks` 口径一致；缩放不触发重排；
  删除后重扫在 MFT 懒写窗口内**明说**"数字可能滞后"，不装准。
- **P4 包更新（只读）+ 开机自启（独立开关）**。
  包更新按 §4.5 的四条机制做：快照文件 + 本地比对 + 风险标签 + 不代跑。
- **P5 git 状态、代码高亮（走高亮插件而非内置依赖）**。


---

## 8. 需要你先拍的六件事

1. ✅ **已拍（2026-10-17）：左栏要的是「目录树」常驻侧栏**，不是常用位置侧栏。
   由此定下四条契约，实现按这四条走：
   - **只显示目录**，不显示文件（树的作用域是"位置"，文件归两栏）。
   - **懒加载**：展开哪层列哪层。`D:\rj` 实测 2826 个 exe 要 35 s（§4.4 的索引代价），
     全量建树在这个量级上是不可接受的；卷根、归档根、远程根都走同一套。
   - **展开态按绝对路径为键**，不按树里的位置。这条直接借 disktree 的不变量 5
     （"标记按绝对路径 key，所以能活过一次重扫"）——我们重列目录时展开态不该炸。
   - **树驱动当前活动栏**；对栏要接收目录得靠显式动作（右键/中键/箭头），
     不抢焦点。特殊文件夹（下载/文档…）作为**根节点挂进树**，
     而不是另做一排——这是"目录树"这个选择顺带定下来的形状。
   读不了的目录**要在树上留可见标记**，不许静默变空（§2 同一条纪律）。

2. **包更新要不要"代跑"**：我建议**只列不跑**（§4.3 的 1/2/3/5 全部因此规避）。
   要跑就得接受 UAC 弹窗、3010 被当失败、burn 被占用文件卡住这三类失败面。
   折中方案：列出来 + 一个"交给 winget / UniGetUI 执行"的跳转。
3. **实时磁盘视图做到哪一档**：我建议承诺到 实时-b（删除后重扫受影响子树），
   把 实时-c（watcher）明确列为不做，除非你愿意加一个里程碑。
   注意 disktree 自己**连 实时-a 都不完整**：扫描期间不渲染部分树（§3.4）。
4. **开机自启的形态**——本机实测条目：HKCU Run 8 个、HKLM Run 4 个、
   用户启动夹 2 个（其中 1 个被改名成 `.disabled-20261015`，说明**你已经在用
   "改名即停用"这个惯例**）、公共启动夹 1 个、计划任务 186 个（27 个 logon 触发）。

   | 做法 | 需管理员 | 可逆 | 在"设置>启动应用"可见 | 残留风险 |
   |---|---|---|---|---|
   | HKCU `\Run` 一个值 | 否 | 删值即可 | 是 | 低（死值） |
   | 启动文件夹 `.lnk` | 否 | 删/改名 | 是，且资源管理器里看得见 | 低 |
   | 计划任务 logon 触发 | Interactive 触发不需要 admin | 能 `Unregister` 但用户看不见 | **否** | **高**（本机 27 个就是证据） |
   | MSIX `StartupTask` | 要 MSIX 打包 + 弹窗授权 | 系统托管 | 是 | fmnr 是便携 exe，不适用 |

   研究侧推荐 **HKCU `\Run` + `--autostart` 参数**（本机 FluxDown 实测就是这个形状）；
   我原先倾向 `.lnk`（对用户可见、改名即停用——正好是你自己的惯例）。
   **两者都合规（只 HKCU、可逆、不碰别人的键），请挑一个。**
   无论哪个，都**不建计划任务**：本机 27 个 logon 触发任务已经证明这类东西会积累且看不见。
5. **磁盘占用视图的内存预算能不能接受**：全盘扫一次 ≈ **299 MB 常驻**
   （88 B/节点 × 340 万条目，§3.3）。这台机常驻只剩 2–3 GB。
   我的建议是**默认只扫当前子树 + `max_depth` 截断 + 显示条目数与估算内存**，
   全盘要显式点。你要是想要"一进来就是整盘"，那得先接受这个数字。
6. **依赖重量**：接 `disktree-core` 会把 `rayon`/`sysinfo`/`trash`/`chrono` 拉回依赖树
   （`rayon` 上一轮刚当死依赖删掉）。三条路（git 依赖 / vendored 子集 / 按不变量重写）
   在 §3.3 表里，我推荐"先 A spike 再 C"，但要不要为此破一次"依赖克制"是你说了算。

---

## 9. 本轮研究留下的临时件（可删）

- `C:\Users\lbx13\Documents\Qoder\2026-10-15\cf9915da\.research\disktree\`
  —— 第三方仓的**只读克隆**（`git remote` 指向 `tobi/disktree`），只为核对本文引用。
- 同目录 `.research\{uninstall_scan.ps1,map_probe2.ps1,revmap.ps1}` —— 注册表/映射取证脚本。
- `C:\Users\lbx13\Documents\Qoder\2026-10-15\cf9915da\proc_probe.py` —— §2.1 的进程归因脚本。
- `E:\tmp\fmnr-ui-check\`、`E:\tmp\fmnr-open-probe\` —— 界面实测用的临时目录树。
- `%TEMP%\fmnr-mcp-open-*` 46 个 —— **不是本轮造的**，是 §2.1 那个缺陷的历史残留，
  P0 修完再清（现在清也会被下一次测试重新造出来）。


---

## 附：本轮实测到的界面事实（已修，未推）

上一轮的前端修复已在本文件之外记录于 README「已知边界」；
本轮新增实测：挂 zip 后两栏能列出包内内容、进"拒绝访问"目录会红字报原因、
过滤框退格不再让对栏跳上级、重命名扩展名保留。

---

## 10. ✅ 已确诊并修掉：栏头显示的路径和列出来的内容不是同一个目录

**原登记（2026-10-17 上午）**：复现条件 `USERPROFILE=E:\tmp\fmnr-ui-check`、
进程 CWD=`E:\tmp` 起 `fmnr-ui.exe`。量到：栏头/面包屑/状态栏写着盘符，
列出的目录行却是 `E:\tmp` 的（`adv-mut`、`arch-fixture`、`fmnr-drop-*`…，
`ls /e` 里一个没有），而文件段是 `E:\` 根独有的 4 个
（`cmd_start_for.ini`、`DumpStack.log`、`exp_robocopy.log`、`tmp_self.json`）；
双击其中一个目录后右栏栏头少了一段（`E:\fmnr-drop-5748-empty`，`ls` 判不存在），
显示的内容却是对的。当时机制没定，只留了假设位。

**机制（现已量到，不是推断）**：面包屑的两个函数各算各的——
`breadcrumbs()` 用 `file_name()`/`parent()` 出标签，`crumbs_to_path()` 用
`components()` **取前 N 个重拼**路径。Windows 的组件序列是
`[Prefix("E:"), RootDir("\\"), Normal("tmp"), …]`，而标签序列里没有盘根
（`Path::new("E:\\").file_name()` 是 `None`），两套下标差一层。
用旧代码原样跑一遍（`E:\tmp\adv-mut`）：

```
标签 = ["tmp", "adv-mut"]
  点第 0 跳("tmp")     -> "E:"    is_absolute=false is_dir=true
  点第 1 跳("adv-mut") -> "E:\"   is_absolute=true  is_dir=true
```

第一跳拿到的是 **`E:`——盘相对写法**，按该盘当前目录解析。
所以"点 tmp"实际去的是 `E:`，而 `E:` 又解析成 `E:\tmp`（进程 CWD 所在），
栏头写着盘符、内容是 CWD 目录；再往下双击就变成 `E:adv-mut`
→ 解析成 `E:\tmp\adv-mut`（**内容对、路径串错**，正是登记的第 4 条）。

盘相对写法在枚举层的后果也量了：`fmnr ls "E:"` 在 CWD=`E:\tmp` 时返回
**1001 条**（= `E:\tmp`），而 `fmnr ls "E:/"` 返回 **17 条**（= 盘根）。
根因是 `PathBuf::from("E:\\tmp").join("E:") == "E:"`——`join` 把卷根整个丢掉。

**撤回其中一条**：上面"文件段是 `E:\` 根独有的 4 个"这半句站不住。
按确诊的机制，那一栏列的是 `E:\tmp`，文件行也该是 tmp 的；
那 4 个名字是盘根的。最可能是我把左边**目录树列**（根标着 `tmp`，
子节点正是 `adv-mut`/`arch-fixture`/`fmnr-drop-*`）和中间**文件列**读串了。
结论不变（栏头与内容确实不是同一目录），但那半条测量作废。

**修了两处**：
1. `fmnr-ui::crumb_list()`：标签与目标路径**同源**，都从 `ancestors()` 来，
   不再有"标签一套、下标另一套"的算法；盘根因此多出一跳（`E:\ › tmp › adv-mut`）。
2. `fmnr-fs::LocalVolume::resolve()`：`<盘符>:` 这种写法补成盘根
   （`E:` → `E:\`、`E:foo` → `E:\foo`）。这一处是给 MCP/HTTP/CLI 兜底的——
   外部客户端可以直接把 `"path": "E:"` 传进来，那条腿没有面包屑可修。

**没有顺手改的两处**（探针否证了它们的必要性，写了就是不可达分支）：
`go_up()` 与空间视图的「↑ 上一级」都走 `parent()`，而
`Path::new("E:\\").parent()` 实测是 `None`——它们在盘根就停了，产不出 `E:`。

**验证**：
- 单测 `fmnr-fs::path_tests`（3 条）与 `fmnr-ui::name_tests`（2 条面包屑）。
  反证打过两次：摘掉 `resolve` 的补根 → 两条判红；
  把 `crumb_list` 换回按组件重拼 → 两条判红，报的正是
  「面包屑 "D:" 的目标 "D:" 是盘相对写法」。
  中途一个变异活了下来（删掉 `crumb_list` 里的 `is_drive_relative` 筛子），
  因为 `ancestors()` 的链在盘根就断，那行**永远不会真**——已经删掉了。
- 真跑：`USERPROFILE=E:\tmp` 起窗口 → 双击进 `E:\tmp\adv-mut`
  → 点面包屑「tmp」→ 栏头与状态栏都是 `E:\tmp`，列出的是 tmp 的内容。
  修前同一跳给的是 `E:`（列的是该盘 CWD 的目录）。


## 11. 本轮做完的两件事（都真跑过）

- **应用内拖拽**：跨栏拖到空白、跨栏拖到文件夹行、原地拖放被拒并说明、
  文件夹拖进自己子树被拒 —— 四条都用 computer-use 点过，落地后 `ls` 对过账。
  顺带修掉一个**一开拖就整窗自锁死**的死锁（`ctx.input` 闭包里调 `layer_painter`/`style`，
  它们要同一把 Context 写锁）。Ctrl+drag=复制这条自动化按不住修饰键，**未点验**。
- **包更新盘点（§4 的只读那一半）**：新 crate `fmnr-pkg`，
  解析器拿本机 `winget upgrade` 的**真实输出形状**当语料锁住 8 条契约
  （含"少读一行必须判红"和英文 `No applicable upgrade was found.` 恰好 5 词那个坑；
  夹具里的包名与 ID 换成中性的，列宽 / CJK 名 / 结尾计数行逐字符保留）；
  侧栏「更新」→ 面板列 11 项 → 「复制命令」进系统剪贴板已实测拿到
  形如 `winget upgrade --id <厂商.包名>` 的那一行。**不代跑升级**。
  §4.4 那半（浏览到已装程序目录时提示可升级）没动。


## 13. 特殊文件夹挂进目录树（下载/文档/图片/桌面）

拍定的方案是"挂成起点目录下的子节点"，实现落在两处：

- `fmnr-ui::special_folders()`：标签走 `dirs` 的已知文件夹接口
  （Windows 上是系统 known folder），**不自己拼 `home\Downloads`**——
  这几个目录常被重定向（挪去 OneDrive 或 `D:\文档`），拼出来的路径
  会指向一个不存在、或不是用户那个文档库的目录。拿不到的就不挂。
- `fmnr-tree::merge_specials()`：并进起点那一层时，带标签的那份**排到最前**，
  同一个路径若已被真实列举收进来则从字母位摘掉。

中途先写成"原位换标签"，真跑一次就否掉了：这台机器的 home 下有
一百多个点目录，`下载` 排在字母位等于没挂（滚到看不见）。
所以顺序本身是判据的一部分，测试里钉死了"前端顺序 = 调用方给的顺序"。

列失败的层（权限/卷掉了）原样退回，不并节点：那一层已经有红标要说
"我读不到"，再塞四个指得准不准都不知道的入口进去，等于把错误盖成
一份看起来正常的清单。

**实测**：起窗口 → 树里 `lbx13` 下第一组就是 下载/文档/图片/桌面，
其后才是字母序的其余目录；点「下载」→ 左栏 `C:\Users\lbx13\Downloads`，
栏头、面包屑（`C:\ › Users › lbx13 › Downloads`）、状态栏三处一致。

## 14. 「用默认程序打开」接上真系统默认值（P0b）

`spawn_external_open(file, "")` 以前是一条明确的报错（"解析尚未实现"）——
那之前它回退到 COMSPEC，静默起一个永不退出的 cmd.exe，所以报错本身是对的，
缺的是解析。现在补在新模块 `fmnr-fs::default_prog`：

1. `HKCU\...\Explorer\FileExts\<ext>\UserChoice` 的 `ProgID`（用户点过"始终"的权威值）
2. → `HKCU\Software\Classes\<ext>` 默认值 → `HKLM\Software\Classes\<ext>` 默认值
3. → 拿到 ProgID 后读 `<ProgID>\shell\open\command`（先 HKCU 后 HKLM）
4. → 命令行拆成「程序 + 参数模板」，摘掉 `%1` 与 `%*`

两条刻意的取向：

- **只读不写**。这个模块不碰任何默认值——写的那部分是 `assoc.rs` 的事，
  两个文件混在一起容易把"我只查了一下"写成"我改了设置"（DESIGN 第 68 行）。
- **摘掉 `%1`，而不是替换成文件路径**。这样"默认程序"和"显式指定程序"
  两条路共用同一套拼接代码（文件由调用方补），也避免文件被传两次；
  `%*` 同理必须摘，留着等于 spawn 一个等参数的壳。
- 没有扩展名的点文件（`.gitignore`）走 `*`：Rust 与 Windows 都判它"无扩展名"，
  这条测试里钉死了，因为写成 `".gitignore"` 也能过大部分场景。

**实测**：本机 `.txt` 解析到
`C:\Program Files\WindowsApps\Microsoft.WindowsNotepad_.../Notepad/Notepad.exe`
（UserChoice 里的真值，不是猜的 `notepad.exe`）；
`fmnr open <file.txt> --external`（不传 `--program`）返回
`route: external, program: Notepad.exe` 并真的起了进程。
`live_lookup_on_this_machine_resolves_to_a_real_exe` 这条测试的硬判据是
"解析出来的程序必须是一个真实存在的绝对路径"，拿不到默认程序时跳过而不是判红。

## 15. 子树搜索接进卷抽象（归档卷已实测）

原来只有一条搜索路：`jwalk` 遍历本地文件系统。它在本地卷上是对的，
但归档/远程卷的路径**在本地根本不存在**——`docs/readme.txt` 在一个 zip 里。
所以那条路会去遍历**进程当前目录**（相对虚拟路径被当成相对本地路径解析），
报出来的还是"没有匹配项"。当时界面没有说谎：`open_search` 对非本地卷
直接早退并写"这个卷还搜不了"，渲染层还有一句"这里说的是搜不了，不是没有"。

现在两条路并存，按卷类型选：

- 本地 / UNC / WSL → 还是 `jwalk`（一次遍历，快，且这些路径本地真存在）；
- 归档 / 远程 → 新增 `spawn_volume_search`：逐层 `Volume::list` 的广度遍历，
  同样分批回传、同样用 `fmnr_core::search::find_range` 判命中（高亮与
  MCP/CLI 的有界搜索共用一份谓词，不会出现"高亮说命中、搜索说不命中"）。

一处新契约：`SearchState` 多了 `error`，后台通道从 `Vec<Hit>` 换成
`Batch | Failed`。**根读不了必须说成"搜不动：…"**，不能报 0 命中——
"这里没有"和"我看不到"是两句话，前者让用户改关键词，后者让用户去看挂载。
（原来那句"还没接上"的早退因此删掉了：现在接上了，剩下的情况交给 error。）

代价说清楚：远程卷这条是真 IO，逐层一次 `list`，慢到什么程度
**没在真实主机上量过**（这台机器没有可连的 SSH 目标），所以只做单线程、
接收端一断线程就退出。

**实测（归档卷，真跑）**：`E:\tmp\search-fixture.zip` 里放
`needle-in-zip-9182.txt` 与 `inner/deep-readme-4471.md` →
顶栏「挂载」→ 归档 → 填路径 → 挂上（顶栏变 `archive:E:\tmp\search-fixture.zip`）
→ 「搜索」→ 输入 `deep` → **1 个结果 · 1 ms**，命中 `deep-readme-4471.md`
（它在包内 `inner/` 下一层，jwalk 那条路永远看不到它）。
**远程卷这条没实测**。

## 16. WSL 挂载在界面里点通了（M8 的三条腿补上第二条）

「📦 挂载」→ WSL 页：发行版列表来自 `wsl -l -q`（本机发现到 `Ubuntu-24.04`），
选中 → 挂载 → 顶栏变 `wsl:Ubuntu-24.04`、两栏与目录树都列出该发行版的根
（`bin.usr-is-merged`/`boot`/`dev`/`etc`/`home`/…），状态栏写「已切换到卷 wsl:Ubuntu-24.04」。

这条与"在本地卷里直接进 `\wsl.localhost\<distro>\…`"用的是同一条底层路径
（WSL 的 UNC 本来就可 `read_dir`，不需要协议层），**差别是入口现在看得见**——
M8 当初交付的是库能力，这一条才是用户能力。

还剩 SSH 那条腿没点过：本机没有可连的真实主机。
它和"远程卷子树搜索"是同一个缺口（都要一台能连的机器才能出证据），
所以两条一起挂着，不在没有主机的情况下假装验过。

## 19. 重复文件接进界面（在「磁盘占用」面板里）

同一个根上的另一件事，所以放在同一个窗口里，而不是再加一个顶栏按钮。
一条后台线程 + 通道回快照 + 取消旗，形状照 `usage.rs`。

三句话必须说，否则这一节会给假结论：
- **还在扫** → "已处理 N 个文件，目前 M 组"，不说"没有重复"；
- **被取消** → 明说"这是扫到一半的结果"（半截清单和完整清单长得一模一样）；
- **有读不到的条目** → 报条数 + 第一条原因，并写明"这些没算进结果"。
另外 root 不是目录时给 `error` 并**挡在所有结论前面**——不然界面会说
"查过 0 个文件，没有重复"，把"我进不去"报成"这里很干净"。

「记住指纹（默认不记）」是真开关：不勾就一个字节都不往盘上写
（`HashCache::ephemeral()` 连 `save_to` 都拒绝）；勾了才读写
`<config>/fmnr/dup-hash-cache.txt`。

⚠ **一个只有真跑才看得见的布局错**：条形列表那个 `ScrollArea` 原来写的是
`auto_shrink([false, false])`，它会**吃掉整个窗口的剩余高度**，
于是新加的这一节（连同本来就在的那句提示）被顶到窗口外——
窗口拖高也没用，因为滚动区一直把高度占满。改成
`auto_shrink([false, true]) + max_height(240)` 才看得见。
⇒ 编译、398 个测试、门禁全绿都没能发现它；这类"位置错了"只能靠截图。

**实测**：造 `E:\tmp\duphome`，放 `one.bin` 与 `sub\two.bin`（同为 10 B 同内容）
加一个 `only.bin`（唯一内容）→ 开「磁盘占用」→ 点「♻ 找重复」→
面板报「1 组重复 · 每组留一个可省 10 B」，列出那两个路径（跨子目录找到），
`only.bin` 没被误收。

**这一节只列清单，不删文件**：删除仍然回到列表里选中后走「删除」（回收站）。
逐项勾选 + 只删副本的那套交互是下一件事，没在这里塞。
