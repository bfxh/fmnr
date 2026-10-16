use eframe::egui::{self, Color32, Key, Modifiers};
use fmnr_core::open::{route_open as core_route_open, OpenRoute};
use fmnr_core::volreg::VolumeRegistry;
use fmnr_core::{ListOpts, SortOrder, VPath, Volume, VolumeId};
use fmnr_mount::{MountSpec, MountTable};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 外置打开角标的颜色：按程序名归类着色。
/// 条目出现在这个函数里，就说明**那个进程此刻确实还活着**——进程退出后
/// 看门狗线程已把它从登记表摘掉，所以这里不需要（也不该）有任何「过期」分支。
fn color_for(program: &str) -> Color32 {
    match program.to_lowercase().as_str() {
        p if p.contains("chrome")
            | p.contains("edge")
            | p.contains("firefox")
            | p.contains("brave") =>
        {
            Color32::from_rgb(0, 122, 255)
        }
        p if p.contains("code") | p.contains("vscode") | p.contains("cursor") => {
            Color32::from_rgb(230, 191, 0)
        }
        p if p.contains("notepad")
            | p.contains("word")
            | p.contains("excel")
            | p.contains("powerpoint") =>
        {
            Color32::from_rgb(230, 89, 38)
        }
        _ => Color32::from_rgb(0, 128, 255),
    }
}

pub struct App {
    volume_registry: VolumeRegistry,
    current_volume_id: Option<VolumeId>,
    /// 挂载记录（哪些挂上了、哪些失败了）——失败原因要留档，不能只闪一条状态。
    mount_table: MountTable,
    /// 挂载对话框；None = 未打开。
    mount_dialog: Option<MountDialog>,
    /// 左栏当前目录
    left_path: PathBuf,
    /// 右栏当前目录
    right_path: PathBuf,
    left_entries: Vec<fmnr_core::Entry>,
    right_entries: Vec<fmnr_core::Entry>,
    /// 每栏选中的**条目名**，不是行号。
    ///
    /// 存行号的话，过滤一生效行号就对不上 `*_entries`，
    /// 回车/复制/删除会作用在另一条上——界面显示的和实际操作的对象分裂。
    left_selected: Option<String>,
    right_selected: Option<String>,
    /// 每栏最近一次列目录失败的原因。
    ///
    /// 原来错误被 `unwrap_or_default()` 吞成空列表，用户看到的是
    /// 「这个目录是空的」，而真相是「没权限 / 卷没挂上 / 路径不存在」。
    left_error: Option<String>,
    right_error: Option<String>,
    /// 上一帧是否有文本框持有焦点：快捷键据此让行。
    text_focus: bool,
    /// 重命名/新建目录弹窗的输入框需要在打开那一帧抢到焦点。
    name_focus: bool,
    /// 弹窗里上一次提交被拒绝的原因（空名、已存在、非法字符…）。
    name_error: Option<String>,
    cjk_font_ok: bool,
    active_pane: Pane,
    show_hidden: bool,
    sort: SortOrder,
    /// 面包屑行内过滤
    left_filter: String,
    right_filter: String,
    /// 状态栏消息
    status: String,
    /// 待写入**系统**剪贴板的文本（"复制完整路径"用）。
    /// 攒到字段里是因为动作执行时手上没有 ui，下一帧开头再交给 egui。
    clipboard_text: Option<String>,
    /// M3 统一预览面板状态
    preview: Option<crate::preview::Preview>,
    /// M1 图标纹理缓存（IconKey → 纹理）
    icons: crate::icons::IconCache,
    /// M2 剪贴板（复制/剪切）
    clipboard: crate::ops::Clipboard,
    /// M2 正在跑的长操作（拷贝/移动/删除）
    running: Option<crate::ops::RunningOp>,
    /// M2 冲突策略弹窗：None = 不在弹窗中
    conflict: Option<ConflictPrompt>,
    /// M2 重命名弹窗输入框
    renaming: Option<String>,
    /// M2 新建目录名输入框
    new_dir_name: Option<String>,
    /// M2 子树搜索的在途任务（None = 当前没有搜索）
    search: Option<crate::search::SearchState>,
    /// 搜索面板是否展开。和 `search` 分开存：
    /// 面板刚打开、还没打字时 `search` 是 None，但面板得画出来。
    search_open: bool,
    /// 面板打开那一帧要抢焦点。
    search_focus: bool,
    /// 本次搜索的根目录，展开面板时锁定。
    /// 每次敲键都重扫一遍当前子树：活动栏会被搜索面板顶掉，
    /// 用 active_path 的话扫到一半栏位一变，结果集就跟着抖动。
    search_root: PathBuf,
    /// 搜索框文本
    search_text: String,
    /// M5 插件宿主：按扩展名路由预览渲染器
    plugins: fmnr_plugins::registry::PluginSet,
    /// 插件加载警告（坏插件路径 + 原因），启动时填一次，状态栏可见
    plugin_warnings: Vec<fmnr_plugins::registry::LoadWarning>,
    /// 目录树（左侧常驻侧栏的模型；契约见 tree 模块与 docs/UI-NEXT.md §8.1）
    tree: crate::tree::Tree,
    /// 包更新盘点（只读，入口在目录树那一排的「更新」）
    pkg: PkgPanel,
    /// 设置窗口
    settings_open: bool,
    /// 磁盘占用视图：在跑的扫描 + 窗口是否开着
    usage_open: bool,
    usage: Option<crate::usage::Running>,
    /// 当前在看哪一层（点占比条会下钻，面包屑回上级）。
    /// 和扫描的 root 是同一个东西：下钻=重起一次以它为根的扫描。
    usage_root: PathBuf,
    /// 自启动管理（顶栏常驻入口）。`None` = 还没读。
    startup_open: bool,
    startup: Option<fmnr_fs::startup::StartupInventory>,
    startup_error: Option<String>,
    /// 自启动状态。`None` = 还没读；读失败走 `autostart_error` 而不是当成"没开"——
    /// 把"读不到"显示成"未开启"会诱导用户去点开启，然后覆盖一个他看不见的项。
    autostart_status: Option<fmnr_fs::autostart::Status>,
    autostart_error: Option<String>,
    /// 自启动要登记的就是这个 exe 自己
    self_exe: PathBuf,
    /// M6 空间白板（布局层，与双栏视图互斥显示）
    board: fmnr_core::board::Board,
    /// 当前视图：双栏 or 白板
    view: ViewMode,
    /// 白板相机（平移 + 缩放）。坐标变换全在 `board_view::Camera` 里。
    board_cam: crate::board_view::Camera,
    /// 白板上正在拖拽的节点下标
    board_drag: Option<usize>,
    /// 应用内拖拽：正在拖的那批文件（None = 没在拖）。
    ///
    /// 刻意不用 egui 的 `DragAndDrop` payload：它在**释放那一帧**就被清掉，
    /// 而"落到哪个目录"恰好要在释放那一帧判。自己存一份，时序才说得清。
    drag: Option<DragPayload>,
    /// 本帧指针悬在哪个目录上（可以收这次拖拽）。每帧开头清空。
    drop_target: Option<PathBuf>,
    /// 上一帧的画布区域。用于把「视图中心」换算成画布坐标（钉节点、适应全部）。
    board_viewport: egui::Rect,
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum ViewMode {
    Panes,
    Board,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
enum Pane {
    Left,
    Right,
}

/// 右键菜单里的一个动作。
///
/// 刻意做成"收集意图 → 闭包外执行"：菜单是在 `show_rows` 的闭包里画的，
/// 而每个动作都要可变借用 `self`（重列目录、起长操作、开弹窗）。
/// 在闭包里直接执行会和列表的借用打架。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAction {
    Open,
    OpenExternal,
    Copy,
    Cut,
    Paste,
    Delete,
    Rename,
    CopyPath,
    PinToBoard,
    OpenInOtherPane,
}

/// 一次应用内拖拽搬的那批路径。
///
/// 来源目录不单独记栏位——`plan_drop` 从 `paths` 的父目录判原地拖放，
/// 栏位只是对同一件事的第二种说法，两者不一致时反而不知道该信谁。
#[derive(Debug, Clone)]
struct DragPayload {
    paths: Vec<PathBuf>,
}

/// 包面板的两个页签。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PkgTab {
    #[default]
    Upgrades,
    Ledger,
}

/// 包更新盘点的状态。**只读**：这里只列"有什么可升"，不代跑升级。
///
/// 不代跑有三个理由（见 `fmnr-pkg` 模块注释）：winget 的表格输出是本地化的、
/// .NET 这类运行时"点了第一次只是排队"、以及代跑就要替用户决定 MSI 的作用域。
#[derive(Default)]
struct PkgPanel {
    open: bool,
    tab: PkgTab,
    rows: Vec<fmnr_pkg::Upgrade>,
    error: Option<String>,
    /// 在途的盘点。`Some` 表示已经跑起来了，别再起第二个。
    rx: Option<std::sync::mpsc::Receiver<Result<Vec<fmnr_pkg::Upgrade>, String>>>,
    /// 已装底账（注册表那三棵树）。它和"可升级"是两件事：
    /// 底账回答"这台机器装了什么、谁装的"，可升级只覆盖包管理器看得见的那部分。
    ledger: Vec<fmnr_pkg::registry::InstalledApp>,
    ledger_error: Option<String>,
    ledger_rx:
        Option<std::sync::mpsc::Receiver<Result<Vec<fmnr_pkg::registry::InstalledApp>, String>>>,
    filter: String,
    /// 详情面板看的是底账里第几条（过滤后的下标）。
    detail: Option<usize>,
    /// 拖进来的快捷方式。解析要等底账，所以存路径而不是存结论——
    /// 底账晚一点读出来，这里的结果就自己变准了。
    shortcuts: Vec<PathBuf>,
}

/// 冲突弹窗：已把请求暂存，等用户选策略。
struct ConflictPrompt {
    kind: crate::ops::OpKind,
    sources: Vec<VPath>,
    dest_dir: Option<VPath>,
    conflicting: Vec<String>,
    choice: fmnr_core::ConflictPolicy,
}

/// 挂载对话框：选类型 + 填入参。三种入口共用一个窗口，
/// 避免工具栏上挂三个长得几乎一样的按钮（UI 自身的克制，见 DESIGN §5）。
struct MountDialog {
    kind: MountKind,
    /// 归档路径 / SSH 端点 /（发现失败时的）手动 distro 名。
    input: String,
    /// `wsl -l -q` 的发现结果；Some = 已尝试过发现。
    wsl_distros: Option<Result<Vec<String>, String>>,
    /// WSL 列表里的选中下标。
    wsl_picked: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MountKind {
    Archive,
    Wsl,
    Ssh,
}

impl MountDialog {
    fn new() -> Self {
        Self {
            kind: MountKind::Archive,
            input: String::new(),
            // 打开时同步发现（与本应用其它地方一致的同步模型；
            // `wsl -l -q` 实测很快，不值得为它引一套异步管线）。
            wsl_distros: Some(fmnr_mount::discover_wsl_distros().map_err(|e| e.to_string())),
            wsl_picked: 0,
        }
    }

    /// 把对话框内容拼成挂载 spec；WSL 优先用发现列表的选中项。
    fn spec(&self) -> Option<MountSpec> {
        match self.kind {
            MountKind::Archive => {
                let p = self.input.trim();
                if p.is_empty() {
                    return None;
                }
                Some(MountSpec::Archive {
                    path: PathBuf::from(p),
                })
            }
            MountKind::Wsl => {
                if let Some(Ok(list)) = &self.wsl_distros {
                    if let Some(d) = list.get(self.wsl_picked) {
                        return Some(MountSpec::Wsl { distro: d.clone() });
                    }
                }
                let d = self.input.trim();
                if d.is_empty() {
                    return None;
                }
                Some(MountSpec::Wsl {
                    distro: d.to_string(),
                })
            }
            MountKind::Ssh => {
                let e = self.input.trim();
                if e.is_empty() {
                    return None;
                }
                Some(MountSpec::Ssh {
                    endpoint: e.to_string(),
                })
            }
        }
    }
}

impl App {
    pub fn new(cjk_font_ok: bool) -> Self {
        // M8 卷接线：启动时用 fmnr_mount 的默认计划挂载（本地 home 起步，
        // 归档/WSL/SSH 由顶栏「挂载」入口随时补挂），与 CLI 共用同一套逻辑。
        let home = dirs_home();
        let registry = VolumeRegistry::new();
        let specs = fmnr_mount::default_plan(&home, None);
        let mount_table = fmnr_mount::mount_plan(&registry, &specs);
        // 当前卷取挂载成功的第一个 id —— 不硬编码：LocalVolume::new(home)
        // 自生的 id 是 local:<home 绝对路径>，写死会永远查不到、
        // 让 current_volume() 每次都走 fallback，等于卷接线没生效。
        let current_volume_id = mount_table
            .entries()
            .iter()
            .find_map(|e| e.volume_id.clone());
        let mut app = Self {
            volume_registry: registry,
            current_volume_id,
            mount_table,
            mount_dialog: None,
            left_path: home.clone(),
            right_path: home,
            left_entries: Vec::new(),
            right_entries: Vec::new(),
            left_selected: None,
            right_selected: None,
            left_error: None,
            right_error: None,
            text_focus: false,
            name_focus: false,
            name_error: None,
            cjk_font_ok,
            active_pane: Pane::Left,
            show_hidden: false,
            sort: SortOrder::Name,
            left_filter: String::new(),
            right_filter: String::new(),
            // 没装到 CJK 字体的话，界面所有中文都是豆腐块，而用户无从知道
            // 是"程序坏了"还是"这台机没字体"。这条要说出来。
            clipboard_text: None,
            status: if cjk_font_ok {
                String::new()
            } else {
                "⚠ 未找到中文字体，界面文案会显示成 □□；候选路径见 docs/DESIGN.md §5".to_string()
            },
            preview: None,
            icons: crate::icons::IconCache::new(
                overrides_dir(),
                crate::icons::IconCache::builtin_dir(),
            ),
            clipboard: crate::ops::Clipboard::default(),
            running: None,
            conflict: None,
            renaming: None,
            new_dir_name: None,
            search: None,
            search_open: false,
            search_focus: false,
            search_root: PathBuf::new(),
            search_text: String::new(),
            plugins: fmnr_plugins::registry::PluginSet::new(),
            plugin_warnings: Vec::new(),
            tree: crate::tree::Tree::default(),
            pkg: PkgPanel::default(),
            settings_open: false,
            usage_open: false,
            usage: None,
            usage_root: PathBuf::new(),
            startup_open: false,
            startup: None,
            startup_error: None,
            autostart_status: None,
            autostart_error: None,
            self_exe: std::env::current_exe().unwrap_or_default(),
            board: fmnr_core::board::Board::new("default"),
            view: ViewMode::Panes,
            board_cam: crate::board_view::Camera::default(),
            drag: None,
            drop_target: None,
            board_drag: None,
            board_viewport: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0)),
        };
        // 插件目录不存在是正常状态（没装插件）；坏插件只记警告不阻塞启动。
        let (plugins, warnings) = fmnr_plugins::registry::PluginSet::load_dir(&plugins_dir());
        app.plugins = plugins;
        app.plugin_warnings = warnings;
        // 白板：能读就恢复，读不到（首次运行/文件坏了）就用空板，不阻塞启动。
        if let Ok(b) =
            fmnr_core::board::Board::load(&fmnr_core::board::Board::default_path("default"))
        {
            app.board = b;
        }
        app.refresh();
        app.reset_tree_roots();
        app
    }

    fn opts(&self) -> ListOpts {
        ListOpts {
            show_hidden: self.show_hidden,
            sort: self.sort,
        }
    }

    fn refresh(&mut self) {
        // 一个 runtime 列两栏：`Runtime::new()` 每次会起工作线程，
        // 按栏各建一个等于每次刷新白扔一个线程。
        let rt = tokio::runtime::Runtime::new().unwrap();
        let opts = self.opts();
        let Some(volume) = self.current_volume() else {
            self.left_entries = Vec::new();
            self.right_entries = Vec::new();
            self.left_error = Some("没有已挂载的卷".to_string());
            self.right_error = self.left_error.clone();
            return;
        };
        let (entries, error) = Self::list_dir(&rt, &volume, &self.left_path, &opts);
        self.left_entries = entries;
        self.left_error = error;
        let (entries, error) = Self::list_dir(&rt, &volume, &self.right_path, &opts);
        self.right_entries = entries;
        self.right_error = error;

        // 走到这里的都是"目录内容变了"（删除/改名/新建/粘贴完成），
        // 树里那两层的缓存就过期了。只作废两栏路径之下：
        // 全清会让每次刷新都重列一遍用户点开的分支。
        let (l, r) = (self.left_path.clone(), self.right_path.clone());
        self.tree.invalidate_below(&l);
        self.tree.invalidate_below(&r);
    }

    /// 只刷一栏。导航/面包屑跳转用这个——全刷会把对栏也重列一遍，
    /// 而列归档卷或远程卷是慢操作，对栏没变不该跟着抖。
    fn refresh_pane(&mut self, which: Pane) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let opts = self.opts();
        let Some(volume) = self.current_volume() else {
            self.set_entries(which, Vec::new(), Some("没有已挂载的卷".to_string()));
            return;
        };
        let path = self.path_of(which).clone();
        let (entries, error) = Self::list_dir(&rt, &volume, &path, &opts);
        self.set_entries(which, entries, error);
    }

    /// 列一个目录，把失败原因原样带出来而不是吞掉。
    ///
    /// 空目录和"读不了"必须能区分：前者显示"这里没有文件"，
    /// 后者显示路径 + 错误，用户才知道要挂卷还是改权限。
    fn list_dir(
        rt: &tokio::runtime::Runtime,
        volume: &Arc<dyn Volume>,
        path: &Path,
        opts: &ListOpts,
    ) -> (Vec<fmnr_core::Entry>, Option<String>) {
        match rt.block_on(volume.list(&VPath(path.to_path_buf()), opts)) {
            Ok(entries) => (entries, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        }
    }

    fn set_entries(&mut self, which: Pane, entries: Vec<fmnr_core::Entry>, error: Option<String>) {
        match which {
            Pane::Left => {
                self.left_entries = entries;
                self.left_error = error;
            }
            Pane::Right => {
                self.right_entries = entries;
                self.right_error = error;
            }
        }
    }

    /// 让某一栏进入新目录：换路径、清掉该栏的过滤与选中，再重新取条目。
    ///
    /// 必须清过滤：路径变了但过滤词还在，新目录的条目会被旧词筛掉，
    /// 界面显示成一个近乎空的列表，看起来像"目录被清空了"。
    fn navigate(&mut self, which: Pane, path: PathBuf) {
        self.status = format!(
            "{} → {}",
            match which {
                Pane::Left => "左栏",
                Pane::Right => "右栏",
            },
            path.display()
        );
        match which {
            Pane::Left => {
                self.left_path = path;
                self.left_filter.clear();
                self.left_selected = None;
            }
            Pane::Right => {
                self.right_path = path;
                self.right_filter.clear();
                self.right_selected = None;
            }
        }
        self.refresh_pane(which);
    }

    /// 卷的起点路径。与 `switch_volume` 里那套一致：
    /// 本地/WSL 卷从 home 起步，归档等虚拟卷没有 home，从卷根（空路径）开始。
    fn volume_root(&self) -> PathBuf {
        match self.current_volume().map(|v| v.kind()) {
            Some(fmnr_core::VolumeKind::Local) => dirs_home(),
            _ => PathBuf::new(),
        }
    }

    /// 重建树的根为"当前卷"。
    ///
    /// 只放一个根：跨卷混排会让"点这个目录该用哪个卷去列"变成需要额外规则的问题，
    /// 而切卷本来就有顶栏下拉。特殊文件夹按拍定的方案挂成这个根下的子节点。
    fn reset_tree_roots(&mut self) {
        let volume = match self.current_volume() {
            Some(v) => v,
            None => {
                self.tree.set_roots(Vec::new());
                return;
            }
        };
        let root = self.volume_root();
        // 根节点的名字取**这个起点目录自己**的名字，不取卷 id 的短名。
        // `short_volume_name` 是按"盘符开头"设计的（把 `local:C:\<home>` 收成 `local:C:`），
        // 起点不是盘根时它会截出误导性标签：实测把 `E:\tmp\fmnr-ui-check`
        // 显示成 `local:E:\`，而它列出来的确实是 fmnr-ui-check 的子目录。
        // 只有起点真的是盘根（或归档卷的空路径）时才回落到卷名。
        let name = match root.file_name() {
            Some(n) => n.to_string_lossy().to_string(),
            None => short_volume_name(&volume.id().0),
        };
        self.tree
            .set_roots(vec![crate::tree::TreeDir { name, path: root }]);
        // 根下第一层默认展开：否则侧栏开局只有一个孤零零的卷名，看着像坏了。
        if let Some(first) = self.tree.roots().first() {
            let path = first.path.clone();
            self.tree.toggle(&path);
        }
    }

    fn path_of(&self, which: Pane) -> &PathBuf {
        match which {
            Pane::Left => &self.left_path,
            Pane::Right => &self.right_path,
        }
    }

    fn selection_of(&self, which: Pane) -> Option<String> {
        match which {
            Pane::Left => self.left_selected.clone(),
            Pane::Right => self.right_selected.clone(),
        }
    }

    /// 当前选中的卷（Registry 查表）。
    ///
    /// 查不到时回落到「仓里的第一个卷」而不是新建一个临时 `LocalVolume`：
    /// 临时卷会让 `external_opens()`（外部打开角标）落在没人读的登记表上，
    /// 角标从此永远不亮——这个 bug 不会报错，只会静默失灵。
    fn current_volume(&self) -> Option<Arc<dyn Volume>> {
        let by_id = self
            .current_volume_id
            .as_ref()
            .and_then(|id| self.volume_registry.get(id));
        by_id.or_else(|| {
            self.volume_registry
                .list()
                .first()
                .and_then(|v| self.volume_registry.get(&v.id))
        })
    }

    /// 切换当前卷：换卷要同时把两栏路径复位到该卷根，
    /// 否则会拿上一卷的绝对路径去问新卷要条目（归档/WSL 卷上必然失败）。
    fn switch_volume(&mut self, id: VolumeId) {
        if self.current_volume_id.as_ref() == Some(&id) {
            return;
        }
        self.current_volume_id = Some(id);
        let kind = self.current_volume().map(|v| v.kind());
        // 本地/WSL 卷用 home 当起点（这是用户最常去的地方）；
        // 归档等虚拟卷没有「home」概念，从卷根开始。
        let start = match kind {
            Some(fmnr_core::VolumeKind::Local) => dirs_home(),
            _ => PathBuf::new(),
        };
        self.left_path = start.clone();
        self.right_path = start;
        self.left_selected = None;
        self.right_selected = None;
        self.preview = None;
        self.reset_tree_roots();
        self.status = format!(
            "已切换到卷 {}",
            short_volume_name(&self.current_volume_id.as_ref().unwrap().0)
        );
        self.refresh();
    }

    fn active_path(&self) -> &PathBuf {
        match self.active_pane {
            Pane::Left => &self.left_path,
            Pane::Right => &self.right_path,
        }
    }

    /// 处理外部拖入的文件：拷进当前活动栏的目录。
    ///
    /// 与「复制/粘贴」走同一条 `start_op`，因此冲突弹窗、进度、取消全都自动一致——
    /// 拖放只是**另一个触发入口**，不是第二套拷贝实现。
    ///
    /// 判定逻辑在 `plan_drop`（纯函数，可单测）；这里只负责取输入与执行。
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }

        // 快捷方式不是"要搬进来的文件"，是"要认一下的入口"。
        // 分流之后：.lnk 只被读取，原件留在原处（用户要的"自动放回去"
        // 靠的是**根本不动它**，而不是搬进来再搬回去）；其余走原来的拷贝。
        let (shortcuts, files): (Vec<PathBuf>, Vec<PathBuf>) = dropped
            .into_iter()
            .partition(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")));
        if !shortcuts.is_empty() {
            self.inspect_shortcuts(&shortcuts);
        }
        if files.is_empty() {
            return;
        }
        let dropped = files;

        let dest = self.active_path().clone();
        match plan_drop(&dropped, &dest) {
            DropPlan::Ignore(reason) => self.status = reason,
            DropPlan::Copy(sources) => {
                let n = sources.len();
                let sources = sources.into_iter().map(VPath).collect();
                self.start_op(
                    crate::ops::OpKind::Copy,
                    sources,
                    Some(VPath(dest)),
                    fmnr_core::ConflictPolicy::Skip,
                );
                if self.running.is_some() {
                    self.status = format!("拖入拷贝（{n} 项）…");
                }
            }
        }
    }

    /// 打开一个条目：目录在当前栏进入，文件走外部程序（内部会先试预览）。
    ///
    /// `is_dir` 由调用方从**卷报的条目**里取，不在这里问文件系统：
    /// 归档卷里的 `/doc/readme.txt` 在本地不存在，`path.is_dir()` 恒为 false，
    /// 那样 zip 里的文件夹会变成"用外部程序打开一个不存在的文件"。
    fn enter(&mut self, path: PathBuf, is_dir: bool) {
        if is_dir {
            self.navigate(self.active_pane, path);
            self.view = ViewMode::Panes;
        } else {
            self.spawn_external(&path);
        }
    }

    /// 从搜索结果回到文件：活动栏进它的目录、对栏进它的父目录、选中它。
    ///
    /// 原来搜到结果双击只把**活动栏**导航过去，选中态留在上一个目录里，
    /// 用户看到的还是旧目录高亮着旧文件——搜索结果等于没落到地面上。
    fn reveal_in_panes(&mut self, path: &Path) {
        let Some(dir) = path.parent() else {
            return;
        };
        let other = match self.active_pane {
            Pane::Left => Pane::Right,
            Pane::Right => Pane::Left,
        };
        // 对栏停在这个文件的父目录：一屏就能同时看到"它在哪"和"它旁边有什么"。
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        self.navigate(other, dir.to_path_buf());
        self.set_selection_for(other, &name);
    }

    /// 按名字设某一栏的选中态。名字不在该栏列表里就不选——
    /// 选中一个看不见的条目等于给用户一个点了删除会动别的东西的假象。
    fn set_selection_for(&mut self, which: Pane, name: &str) {
        let entries = match which {
            Pane::Left => &self.left_entries,
            Pane::Right => &self.right_entries,
        };
        let hit = entries.iter().any(|e| e.name == name);
        match which {
            Pane::Left => self.left_selected = hit.then(|| name.to_string()),
            Pane::Right => self.right_selected = hit.then(|| name.to_string()),
        }
    }

    fn spawn_external(&mut self, path: &Path) {
        let route = core_route_open(path, false);
        let program = match route {
            OpenRoute::Preview => {
                self.open_preview(path);
                return;
            }
            OpenRoute::External(p) => p,
        };
        let program_for_status = program.clone();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let Some(volume) = self.current_volume() else {
            self.status = "打开失败：没有已挂载的卷".into();
            return;
        };
        let volume = volume.clone();
        let vpath = VPath(path.to_path_buf());
        let spawn_volume = volume.clone();
        let result =
            rt.block_on(async move { spawn_volume.spawn_external_open(&vpath, &program).await });
        match result {
            Ok(open) => {
                // 登记进**刚才那个卷**的角标表：看门狗线程会在该进程退出时自动摘掉。
                // 这里再查一次 current_volume() 的话，中途切了卷就把角标挂到别的卷上，
                // 而读角标的是原来那一栏，于是角标永远不亮（也不报错）。
                let key = path.display().to_string();
                volume.external_opens().insert(key, open);
                self.status = format!(
                    "已在 {} 中打开，关闭该程序后角标自动消失",
                    program_for_status
                );
                self.refresh();
            }
            Err(e) => {
                self.status = format!("打开失败 ({e})");
            }
        }
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.active_path().parent() {
            self.navigate(self.active_pane, parent.to_path_buf());
        }
    }

    /// M3 统一预览：读文件字节 → classify → 挂到 self.preview。
    fn open_preview(&mut self, path: &Path) {
        let cap = crate::preview::cap_for(path);
        let rt = tokio::runtime::Runtime::new().unwrap();
        // 这里原来直接 unwrap()：卷表被清空（挂载失败/全被卸载）时
        // 双击任何文件都是 panic，而 UI 崩掉比报一句"没有卷"糟得多。
        let Some(volume) = self.current_volume() else {
            self.preview = None;
            self.status = "预览失败：没有已挂载的卷".into();
            return;
        };
        let volume = volume.clone();
        let vpath = VPath(path.to_path_buf());
        let bytes = rt.block_on(async move {
            let mut stream = volume.read_stream(&vpath).await?;
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                if buf.len() + chunk.len() > cap {
                    buf.extend_from_slice(&chunk[..cap.saturating_sub(buf.len())]);
                    break;
                }
                buf.extend_from_slice(&chunk);
            }
            Ok::<_, fmnr_core::Error>(buf)
        });
        match bytes {
            Ok(buf) => {
                let truncated = buf.len() >= cap;
                let content = self.preview_content_for(path, &buf, truncated);
                self.preview = Some(crate::preview::Preview {
                    path: path.to_path_buf(),
                    content,
                    texture: None,
                });
                self.status = format!("预览 {}", path.display());
            }
            Err(e) => {
                self.status = format!("预览失败 ({e})");
            }
        }
    }

    /// 决定预览内容：插件先认领，没人认领才回落内置分类。
    ///
    /// 判定在 `preview::route`（纯函数，可单测）；这里只负责取输入。
    fn preview_content_for(
        &mut self,
        path: &Path,
        buf: &[u8],
        truncated: bool,
    ) -> crate::preview::PreviewContent {
        crate::preview::route(&mut self.plugins, path, buf, truncated)
    }

    // ---- M2 文件操作 ----

    /// 当前栏选中的条目名（可能多选，预留）。
    ///
    /// 名字直接来自选中态本身，不再拿下标去 `*_entries` 里捞——
    /// 列表会被过滤词筛短，下标一旦按筛后的行列出、按全量取，就指向别的文件。
    fn selected_names(&self) -> Vec<String> {
        self.selection_of(self.active_pane).into_iter().collect()
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        let base = self.active_path().clone();
        self.selected_names()
            .into_iter()
            .map(|n| base.join(&n))
            .collect()
    }

    fn put_clipboard(&mut self, mode: crate::ops::ClipMode) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            self.status = "没有选中任何项".into();
            return;
        }
        self.clipboard.put(mode, paths);
        self.status = self.clipboard.summary();
    }

    /// 粘贴：跨栏则目标是对栏当前目录。
    fn paste(&mut self) {
        if !self.clipboard.still_valid() {
            self.status = "剪贴板为空或源已不存在".into();
            self.clipboard.clear();
            return;
        }
        let cut = self.clipboard.mode == Some(crate::ops::ClipMode::Cut);
        let kind = if cut {
            crate::ops::OpKind::Move
        } else {
            crate::ops::OpKind::Copy
        };
        let sources: Vec<VPath> = self
            .clipboard
            .paths
            .iter()
            .map(|p| VPath(p.clone()))
            .collect();

        // 目标 = 对栏当前目录：双栏管理器的常规走法，"把左边的挪到右边"少一步。
        let dest = self.other_pane_path();
        // 两栏显示同一个目录时（单栏用法、或刚跳进同一层），对栏就是自己。
        // 这时复制应当就地生成副本，剪切则是原地不动 —— 那种要挡掉。
        if dest == *self.active_path() && self.clipboard.mode == Some(crate::ops::ClipMode::Cut) {
            self.status = "剪切的目标就是源所在目录，无需粘贴".into();
            return;
        }

        let conflicts = self.find_conflicts(&sources, &dest);
        if !conflicts.is_empty() {
            self.conflict = Some(ConflictPrompt {
                kind,
                sources,
                dest_dir: Some(VPath(dest)),
                conflicting: conflicts,
                choice: fmnr_core::ConflictPolicy::Rename,
            });
            return;
        }
        self.start_op(
            kind,
            sources,
            Some(VPath(dest.clone())),
            fmnr_core::ConflictPolicy::Skip,
        );
        // 说清贴到哪了：状态栏原本只写"复制…"，粘完之后用户不知道文件去了哪。
        if self.running.is_some() {
            let verb = if cut { "移动到" } else { "复制到" };
            self.status = format!("{verb} {}", dest.display());
        }
    }

    /// 列出「源名在目标目录已存在」的条目名。
    fn find_conflicts(&self, sources: &[VPath], dest: &Path) -> Vec<String> {
        sources
            .iter()
            .filter_map(|s| {
                let name = s.as_path().file_name()?;
                if dest.join(name).exists() {
                    Some(name.to_string_lossy().to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    fn start_op(
        &mut self,
        kind: crate::ops::OpKind,
        sources: Vec<VPath>,
        dest_dir: Option<VPath>,
        policy: fmnr_core::ConflictPolicy,
    ) {
        if self.running.is_some() {
            self.status = "已有操作在进行中".into();
            return;
        }
        let req = crate::ops::OpRequest {
            kind,
            sources,
            dest_dir,
            policy,
        };
        let Some(volume) = self.current_volume() else {
            self.status = "无法启动操作：没有已挂载的卷".into();
            return;
        };
        match crate::ops::spawn_op(volume.clone(), req) {
            Ok(running) => {
                self.status = running.label.clone();
                self.running = Some(running);
            }
            Err(e) => self.status = format!("无法启动操作：{e}"),
        }
    }

    /// 每帧轮询长操作。完成后刷新两栏。
    fn poll_running(&mut self) {
        let Some(running) = self.running.as_ref() else {
            return;
        };
        let Some(outcome) = running.poll() else {
            return;
        };
        let label = running.label.clone();
        let cancelled = running.snapshot().cancelled;
        self.running = None;

        match outcome {
            crate::ops::OpOutcome::Done(kind, report) => {
                self.status = if cancelled {
                    format!("{label}：已取消（完成 {} 项）", report.done)
                } else if report.skipped > 0 {
                    format!(
                        "{label}：完成 {} 项，跳过 {} 项冲突",
                        report.done, report.skipped
                    )
                } else {
                    format!("{label}：完成 {} 项", report.done)
                };
                // 剪切成功后清空剪贴板（剪掉的东西不该还能再贴一次）
                if kind == crate::ops::OpKind::Move {
                    self.clipboard.clear();
                }
            }
            crate::ops::OpOutcome::Failed(kind, msg) => {
                self.status = format!("操作失败（{kind:?}）：{msg}");
            }
        }
        self.refresh();
    }

    fn delete_selected(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            self.status = "没有选中任何项".into();
            return;
        }
        let n = paths.len();
        self.start_op(
            crate::ops::OpKind::Delete,
            paths.into_iter().map(VPath).collect(),
            None,
            fmnr_core::ConflictPolicy::Skip,
        );
        self.status = format!("送入回收站（{n} 项，可恢复）…");
    }

    fn begin_rename(&mut self) {
        let Some(old) = self.selected_names().first().cloned() else {
            self.status = "没有选中任何项".into();
            return;
        };
        // 去掉扩展名，编辑 stem（与主流文件管理器一致）
        let stem = PathBuf::from(&old)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or(old);
        self.name_error = None;
        self.name_focus = true;
        self.renaming = Some(stem);
    }

    fn commit_rename(&mut self) {
        let Some(input) = self.renaming.take() else {
            return;
        };
        let Some(old) = self.selected_names().first().cloned() else {
            return;
        };
        let name = input.trim();
        if name.is_empty() {
            // 名字为空：留着弹窗、把输入还给用户，而不是悄悄丢掉整个重命名。
            self.name_error = Some("名称不能为空".into());
            self.renaming = Some(input);
            self.name_focus = true;
            return;
        }
        // 框里只有 stem：用户没写扩展名就把原来的补回去，写了就按写的走。
        let new_name = rename_target(&old, name);
        let base = self.active_path().clone();
        let from = VPath(base.join(&old));
        let to = VPath(base.join(&new_name));
        if from == to {
            self.renaming = None;
            self.name_error = None;
            return;
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = match self.current_volume() {
            Some(volume) => rt.block_on(volume.rename(&from, &to)),
            None => Err(fmnr_core::Error::Other("没有已挂载的卷".to_string())),
        };
        match result {
            Ok(()) => {
                self.status = format!("已重命名为 {new_name}");
                self.renaming = None;
                self.name_error = None;
                // 选中跟着新名字走，而且要落在**活动栏**：写死左栏的话
                // 在右栏重命名完，高亮跑到左栏一个叫 new_name 的条目上（或没有）。
                let pane = self.active_pane;
                self.refresh();
                self.set_selection_for(pane, &new_name);
            }
            Err(e) => {
                // 失败时不关窗：用户已经打好的字不该被吞掉。
                self.name_error = Some(format!("重命名失败：{e}"));
                self.renaming = Some(input);
                self.name_focus = true;
            }
        }
    }

    fn begin_new_dir(&mut self) {
        self.name_error = None;
        self.name_focus = true;
        self.new_dir_name = Some("新建文件夹".into());
    }

    fn commit_new_dir(&mut self) {
        let Some(input) = self.new_dir_name.take() else {
            return;
        };
        let name = input.trim();
        if name.is_empty() {
            self.name_error = Some("目录名不能为空".into());
            self.new_dir_name = Some(input);
            self.name_focus = true;
            return;
        }
        let base = self.active_path().clone();
        if base.join(name).exists() {
            self.name_error = Some(format!("{name} 已经存在，换个名字"));
            self.new_dir_name = Some(input);
            self.name_focus = true;
            return;
        }
        let target = VPath(base.join(name));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = match self.current_volume() {
            Some(volume) => rt.block_on(volume.create_dir(&target)),
            None => Err(fmnr_core::Error::Other("没有已挂载的卷".to_string())),
        };
        match result {
            Ok(()) => {
                self.status = format!("已新建 {name}");
                self.new_dir_name = None;
                self.name_error = None;
                self.refresh();
            }
            Err(e) => {
                self.name_error = Some(format!("新建失败：{e}"));
                self.new_dir_name = Some(input);
                self.name_focus = true;
            }
        }
    }

    // ---- M2 即时搜索 ----

    /// 展开搜索面板：锁定本次搜索的根、把焦点交给输入框。
    ///
    /// 面板状态（`search_open`）和在途任务（`search`）是两个东西——
    /// 原来工具栏按钮直接调 `start_search()`，而空搜索词会让它把
    /// `search` 置回 None，于是**点按钮什么也不会发生**：面板根本没开过。
    fn open_search(&mut self) {
        let kind = self.current_volume().map(|v| v.kind());
        if !local_walkable(kind) {
            // 不是"搜不到"，是"还没接上"——两句话必须分开说。
            self.status =
                "这个卷还搜不了：子树搜索目前只走本地文件系统（归档/远程卷未接进 Volume）"
                    .to_string();
            return;
        }
        if self.search_open {
            self.search_focus = true;
            return;
        }
        self.search_open = true;
        self.search_focus = true;
        self.search_root = self.active_path().clone();
        let scope = if self.search_root.as_os_str().is_empty() {
            "卷根".to_string()
        } else {
            self.search_root.display().to_string()
        };
        self.status = format!("在 {scope} 中搜索");
    }

    fn close_search(&mut self) {
        self.search_open = false;
        self.search_focus = false;
        self.search = None;
        self.search_text.clear();
    }

    fn start_search(&mut self) {
        let text = self.search_text.trim().to_string();
        if text.is_empty() {
            self.search = None;
            return;
        }
        self.search = Some(crate::search::spawn_search(crate::search::Query {
            text,
            root: self.search_root.clone(),
            case_sensitive: false,
        }));
    }

    /// 每帧拉一次搜索增量；有新结果就催重绘。
    ///
    /// egui 只在有输入事件时重绘。后台线程是按批次推结果的，
    /// 用户不动鼠标时结果到了界面也不更新——看起来就是"搜索卡住了"。
    fn poll_search(&mut self, ctx: &egui::Context) {
        let repaint = match self.search.as_mut() {
            Some(s) => {
                let was_done = s.done;
                let got_batch = s.drain();
                got_batch || (s.done && !was_done)
            }
            None => false,
        };
        if repaint {
            ctx.request_repaint();
        }
    }

    /// 起一次包更新盘点。重复点不会起第二个：在途就只把面板带出来。
    fn start_pkg_scan(&mut self) {
        if self.pkg.rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(fmnr_pkg::list_upgrades().map_err(|e| e.to_string()));
        });
        self.pkg.rx = Some(rx);
        self.pkg.error = None;
        self.pkg.rows.clear();
    }

    /// 起一次已装底账读取（注册表三棵树）。同样只起一个。
    fn start_ledger_load(&mut self) {
        if self.pkg.ledger_rx.is_some() || !self.pkg.ledger.is_empty() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(fmnr_pkg::registry::list_installed().map_err(|e| e.to_string()));
        });
        self.pkg.ledger_rx = Some(rx);
        self.pkg.ledger_error = None;
    }

    /// 每帧收一次盘点/底账结果。
    ///
    /// `request_repaint_after` 而不是 `request_repaint`：winget 冷跑要好几秒，
    /// 连续重绘等于把 CPU 白烧在等一个外部进程上。
    fn poll_pkg(&mut self, ctx: &egui::Context) {
        // 先把结果取成owned值：`Receiver` 不可克隆，而后面要写 self 的别的字段。
        let got = self.pkg.rx.as_ref().map(|rx| rx.try_recv());
        match got {
            Some(Ok(Ok(rows))) => {
                let n = rows.len();
                self.pkg.rows = rows;
                self.pkg.rx = None;
                self.pkg.error = None;
                self.status = format!("包更新盘点完成：{n} 项可升级（只列出，不代跑）");
            }
            Some(Ok(Err(msg))) => {
                self.pkg.rx = None;
                self.pkg.error = Some(msg.clone());
                self.status = format!("包更新盘点失败：{msg}");
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                self.pkg.rx = None;
                self.pkg.error = Some("盘点线程没给结果就结束了".into());
                self.status = "包更新盘点失败：线程没给结果".into();
            }
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
            None => {}
        }

        let got_ledger = self.pkg.ledger_rx.as_ref().map(|rx| rx.try_recv());
        match got_ledger {
            Some(Ok(Ok(apps))) => {
                let n = apps.len();
                self.pkg.ledger = apps;
                self.pkg.ledger_rx = None;
                self.pkg.ledger_error = None;
                self.status = format!("已装底账读好了：{n} 条");
            }
            Some(Ok(Err(msg))) => {
                self.pkg.ledger_rx = None;
                self.pkg.ledger_error = Some(msg.clone());
                self.status = format!("已装底账读失败：{msg}");
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                self.pkg.ledger_rx = None;
                self.pkg.ledger_error = Some("底账线程没给结果就结束了".into());
            }
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(120));
            }
            None => {}
        }
    }

    /// 把拖进来的 `.lnk` 认成"哪个应用的入口"。
    ///
    /// **不复制也不移动**：快捷方式只是被读了一下，原件留在它原来的地方——
    /// 这正是用户要的"会自动放回去"，而"先搬进来再搬回去"是更差的做法
    /// （中途失败就把用户的桌面弄乱了）。
    fn inspect_shortcuts(&mut self, paths: &[PathBuf]) {
        for p in paths {
            if !self.pkg.shortcuts.contains(p) {
                self.pkg.shortcuts.push(p.clone());
            }
        }
        self.pkg.open = true;
        self.pkg.tab = PkgTab::Ledger;
        self.start_ledger_load();
        self.status = format!("检查 {} 个快捷方式（原件未移动）", paths.len());
    }

    /// 包面板：两个页签（可升级 / 已装清单）+ 拖进来的快捷方式 + 详情。
    fn render_pkg_panel(&mut self, ctx: &egui::Context) {
        if !self.pkg.open {
            return;
        }
        let mut copy_cmd: Option<String> = None;
        let mut want_scan = false;
        let mut want_ledger = false;
        let mut drop_shortcut: Option<PathBuf> = None;
        let mut open = self.pkg.open;
        let shortcuts = self.pkg.shortcuts.clone();
        let ledger = self.pkg.ledger.clone();
        let rows = self.pkg.rows.clone();
        let tab = self.pkg.tab;

        egui::Window::new("包管理器")
            .id(egui::Id::new("pkg"))
            .default_size([620.0, 420.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(tab == PkgTab::Upgrades, format!("可升级 {}", rows.len()))
                        .clicked()
                    {
                        self.pkg.tab = PkgTab::Upgrades;
                    }
                    if ui
                        .selectable_label(
                            tab == PkgTab::Ledger,
                            format!("已装清单 {}", ledger.len()),
                        )
                        .clicked()
                    {
                        self.pkg.tab = PkgTab::Ledger;
                        want_ledger = true;
                    }
                });

                if !shortcuts.is_empty() {
                    ui.separator();
                    ui.label(egui::RichText::new("拖进来的快捷方式").strong());
                    for s in &shortcuts {
                        let name = s
                            .file_stem()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        ui.horizontal(|ui| {
                            let text = match shortcut_verdict(s, &ledger, &rows) {
                                Verdict::Loading(t) => t,
                                Verdict::Ok(t) => t,
                                Verdict::Bad(t) => t,
                            };
                            let lbl = egui::RichText::new(format!("{name}：{text}"));
                            match shortcut_verdict(s, &ledger, &rows) {
                                Verdict::Bad(_) => {
                                    let c = ui.visuals().error_fg_color;
                                    ui.colored_label(c, lbl);
                                }
                                _ => {
                                    ui.label(lbl);
                                }
                            }
                            if ui.button("移除").clicked() {
                                drop_shortcut = Some(s.clone());
                            }
                        });
                    }
                }

                ui.separator();
                match self.pkg.tab {
                    PkgTab::Upgrades => {
                        if self.pkg.rx.is_some() {
                            ui.label("正在问 winget…（冷跑要几秒）");
                        }
                        if let Some(err) = &self.pkg.error {
                            let c = ui.visuals().error_fg_color;
                            ui.colored_label(c, err);
                            ui.label("这一步只是读 winget 的表格，没装任何东西。");
                        }
                        if rows.is_empty()
                            && self.pkg.rx.is_none()
                            && self.pkg.error.is_none()
                            && ui.button("问一次 winget").clicked()
                        {
                            want_scan = true;
                        }
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .max_height(200.0)
                            .show(ui, |ui| {
                                for r in &rows {
                                    ui.horizontal(|ui| {
                                        ui.label(&r.name);
                                        ui.weak(format!("{} → {}", r.installed, r.available));
                                        ui.label(format!("({})", r.source));
                                        if ui
                                            .button("复制命令")
                                            .on_hover_text(format!("winget upgrade --id {}", r.id))
                                            .clicked()
                                        {
                                            copy_cmd =
                                                Some(format!("winget upgrade --id {}", r.id));
                                        }
                                    });
                                }
                            });
                    }
                    PkgTab::Ledger => {
                        if self.pkg.ledger_rx.is_some() {
                            ui.label("正在读注册表…");
                        }
                        if let Some(err) = &self.pkg.ledger_error {
                            let c = ui.visuals().error_fg_color;
                            ui.colored_label(c, err);
                        }
                        ui.horizontal(|ui| {
                            ui.label("过滤");
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut self.pkg.filter)
                                        .desired_width(200.0)
                                        .hint_text("名称 / 发布者"),
                                )
                                .changed()
                            {
                                // 详情按下标记住选中项，列表一变这个下标就指向
                                // 另一个应用了——那不是"选中丢了"，是"显示错数据"。
                                self.pkg.detail = None;
                            }
                        });
                        let f = self.pkg.filter.to_lowercase();
                        let shown: Vec<(usize, &fmnr_pkg::registry::InstalledApp)> = ledger
                            .iter()
                            .enumerate()
                            .filter(|(_, a)| {
                                f.is_empty()
                                    || a.name.to_lowercase().contains(&f)
                                    || a.publisher
                                        .as_deref()
                                        .is_some_and(|p| p.to_lowercase().contains(&f))
                            })
                            .collect();
                        let detail = self.pkg.detail;
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .max_height(180.0)
                            .show(ui, |ui| {
                                for (i, a) in &shown {
                                    if ui
                                        .selectable_label(
                                            detail == Some(*i),
                                            format!(
                                                "{} — {} — {} — {}",
                                                a.name,
                                                a.publisher.as_deref().unwrap_or("（无发布者）"),
                                                a.version.as_deref().unwrap_or("（无版本）"),
                                                a.family.label()
                                            ),
                                        )
                                        .clicked()
                                    {
                                        self.pkg.detail = Some(*i);
                                    }
                                }
                            });
                        if let Some(i) = self.pkg.detail {
                            if let Some(a) = shown.get(i) {
                                let a = a.1;
                                ui.separator();
                                ui.label(egui::RichText::new(&a.name).strong());
                                field(ui, "发布者", a.publisher.as_deref());
                                field(ui, "版本", a.version.as_deref());
                                field(ui, "来源", Some(a.family.label()));
                                field(ui, "作用域", Some(a.hive.label()));
                                field(ui, "安装位置", a.install_location.as_deref());
                                field(ui, "卸载串", a.uninstall_string.as_deref());
                                field(ui, "注册表键", a.reg_key.as_deref());
                                if let Some(g) = &a.github {
                                    ui.label(format!("上游 GitHub 仓：{g}（还没接联网查询）"));
                                }
                                if let Some(u) = &a.url {
                                    ui.horizontal(|ui| {
                                        ui.label("官网");
                                        if ui.button("复制链接").clicked() {
                                            copy_cmd = Some(u.clone());
                                        }
                                    });
                                }
                            }
                        }
                    }
                }

                ui.separator();
                // 边界要写在界面上，不能只写在源码注释里：
                // 用户看到一排"可升级"最自然的预期就是点一下能升。
                // 数字一律取现场读到的，不写死本机测出来的那 273/74——
                // 换台机器那句话就成了假声称。
                ui.label(
                    egui::RichText::new(format!(
                        "这里只列清单，不代跑升级。要更新请在 winget 或 UniGetUI 里执行；\
                         .NET 运行时这类包通常要再点一次才真正装上。\n\
                         「可升级」只覆盖 winget 看得见的那部分（本机会报 {} 项）；\
                         「已装清单」读注册表，是全量底账（本机 {} 条，其中 {} 条写了安装位置，\
                         没写安装位置的只能按卸载器所在目录认）。",
                        rows.len(),
                        ledger.len(),
                        ledger
                            .iter()
                            .filter(|a| a.install_location.is_some())
                            .count()
                    ))
                    .size(12.0)
                    .weak(),
                );
            });

        self.pkg.open = open;
        if want_scan {
            self.start_pkg_scan();
        }
        if want_ledger {
            self.start_ledger_load();
        }
        if let Some(s) = drop_shortcut {
            self.pkg.shortcuts.retain(|p| p != &s);
        }
        if let Some(cmd) = copy_cmd {
            self.clipboard_text = Some(cmd);
        }
    }

    /// 打开设置窗口。自启动状态在**第一次打开时**读一次：
    /// 每帧读注册表既没必要（用户不会在别的窗口里改它而期望这里立刻跟着变），
    /// 也会让"读失败"变成每帧刷新的红字。
    fn open_settings(&mut self) {
        self.settings_open = true;
        if self.self_exe.as_os_str().is_empty() {
            self.autostart_error =
                Some("拿不到本程序的路径（current_exe 失败），无法登记自启动".into());
            return;
        }
        if self.autostart_status.is_none() && self.autostart_error.is_none() {
            self.read_autostart();
        }
    }

    fn read_autostart(&mut self) {
        let exe = self.self_exe.display().to_string();
        match fmnr_fs::autostart::status(&exe) {
            Ok(s) => {
                self.autostart_status = Some(s);
                self.autostart_error = None;
            }
            Err(e) => {
                self.autostart_status = None;
                self.autostart_error = Some(e.to_string());
            }
        }
    }

    fn render_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let exe = self.self_exe.display().to_string();
        // 按钮要做的动作先记下来，闭包外再做：这里头既有注册表写入又有刷新，
        // 画 UI 的闭包里不干这些。
        let mut want_enable = false;
        let mut want_disable = false;
        let mut want_reread = false;
        let mut rehidden = false;
        let mut open = self.settings_open;

        egui::Window::new("设置")
            .id(egui::Id::new("settings"))
            .default_size([460.0, 320.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("开机自启动").strong());
                match &self.autostart_error {
                    Some(err) => {
                        let c = ui.visuals().error_fg_color;
                        ui.colored_label(c, err);
                        ui.label("这一步只读 HKCU 下我们自己的那一个值，读不到就是读不到。");
                    }
                    None => {
                        // 先取成 owned 值再画按钮：闭包里还要写 self.autostart_status，
                        // 直接 match &self.… 会让这段借用横跨整个臂体。
                        let status = self.autostart_status.clone();
                        let (text, enabled) = match &status {
                            Some(fmnr_fs::autostart::Status::Off) => {
                                ("未开启。".to_string(), false)
                            }
                            Some(fmnr_fs::autostart::Status::Ours) => {
                                (format!("已开启，指向 {exe}"), true)
                            }
                            Some(fmnr_fs::autostart::Status::Foreign(v)) => {
                                (format!("值名 fmnr 已被别的命令占用：{v}"), false)
                            }
                            None => ("（还没读）".to_string(), false),
                        };
                        ui.label(text);
                        ui.horizontal(|ui| {
                            ui.add_enabled_ui(!enabled, |ui| {
                                if ui.button("开启").clicked() {
                                    want_enable = true;
                                }
                            });
                            ui.add_enabled_ui(enabled, |ui| {
                                if ui.button("关闭").clicked() {
                                    want_disable = true;
                                }
                            });
                            if ui.button("重新读取").clicked() {
                                want_reread = true;
                            }
                        });
                        if matches!(status, Some(fmnr_fs::autostart::Status::Foreign(_))) {
                            let c = Color32::from_rgb(230, 89, 38);
                            ui.colored_label(
                                c,
                                "这种情况这里不提供按钮：覆盖它等于替你把别人的自启动改掉。",
                            );
                        }
                    }
                }
                ui.label(
                    egui::RichText::new(
                        "只写 HKCU\\...\\Run 下名为 fmnr 的那一个值；关闭时也只删这一个值，\
                         不删 Run 键——里面是这台机器所有程序的自启动项。",
                    )
                    .size(12.0)
                    .weak(),
                );

                ui.separator();
                ui.label(egui::RichText::new("列表").strong());
                if ui.checkbox(&mut self.show_hidden, "显示隐藏项").changed() {
                    rehidden = true;
                }

                ui.separator();
                ui.label(egui::RichText::new("这台机器上 fmnr 看到的").strong());
                ui.label(format!(
                    "卷 {} 个 · 插件 {} 个",
                    self.volume_registry.list().len(),
                    self.plugins.len()
                ));
                ui.label(
                    egui::RichText::new(format!("本程序：{exe}"))
                        .size(12.0)
                        .weak(),
                );
                if !self.plugin_warnings.is_empty() {
                    let c = Color32::from_rgb(230, 89, 38);
                    ui.colored_label(
                        c,
                        format!(
                            "{} 个插件没加载成功，状态栏那条会说明原因",
                            self.plugin_warnings.len()
                        ),
                    );
                }
            });

        self.settings_open = open;
        if want_reread {
            self.autostart_status = None;
            self.autostart_error = None;
            self.read_autostart();
        }
        if want_enable {
            match fmnr_fs::autostart::enable(&exe) {
                Ok(r) => self.status = format!("已登记自启动（写 {} 项）", r.written),
                Err(e) => self.status = format!("自启动开启失败：{e}"),
            }
            self.autostart_status = None;
            self.read_autostart();
        }
        if want_disable {
            match fmnr_fs::autostart::disable() {
                Ok(r) => self.status = format!("已取消自启动（删 {} 项）", r.removed),
                Err(e) => self.status = format!("自启动取消失败：{e}"),
            }
            self.autostart_status = None;
            self.read_autostart();
        }
        if rehidden {
            self.refresh();
        }
    }

    /// 打开磁盘占用视图，从当前栏正在看的那一层开始算。
    fn open_usage(&mut self) {
        self.usage_open = true;
        let root = self.active_path().clone();
        if self.usage_root != root || self.usage.is_none() {
            self.start_usage_scan(root);
        }
    }

    /// 起一次扫描。旧的那次先取消：同时跑两棵树等于把内存和磁盘带宽翻倍，
    /// 而用户只会看最后点的那一层。
    fn start_usage_scan(&mut self, root: PathBuf) {
        if let Some(old) = self.usage.take() {
            old.cancel();
        }
        self.usage_root = root.clone();
        self.usage = Some(crate::usage::Running::start(root));
    }

    fn poll_usage(&mut self, ctx: &egui::Context) {
        let got = match self.usage.as_mut() {
            Some(r) => r.poll(),
            None => false,
        };
        let busy = self.usage.as_ref().is_some_and(|r| r.busy());
        if got {
            ctx.request_repaint();
        }
        if busy {
            // 扫描线程不会自己叫醒我们：给它一个节流的重绘节奏，
            // 条会长，但不会把 CPU 烧在空转重绘上。
            ctx.request_repaint_after(std::time::Duration::from_millis(300));
        }
    }

    /// 水位线 + 逐级占比条。
    ///
    /// "差了多少"要先给结论再给图：只剩一点时，用户要的是"动哪个有用"，
    /// 不是一根需要自己换算的条。
    fn render_usage(&mut self, ctx: &egui::Context) {
        if !self.usage_open {
            return;
        }
        let root = self.usage_root.clone();
        let space = fmnr_fs::space::of(&root).ok();
        let usage = self.usage.as_ref().and_then(|r| r.view()).cloned();
        let busy = self.usage.as_ref().is_some_and(|r| r.busy());
        let mut descend: Option<PathBuf> = None;
        let mut want_rescan = false;
        let mut want_cancel = false;
        let mut open = self.usage_open;

        egui::Window::new("磁盘占用")
            .id(egui::Id::new("usage"))
            .default_size([560.0, 420.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(root.display().to_string()).strong());
                    if ui.button("重扫").clicked() {
                        want_rescan = true;
                    }
                    // 下钻之后必须出得来：没有"上一级"的话，点一次就被关在一层里，
                    // 只能关掉窗口重开。
                    if let Some(p) = root.parent().filter(|x| x.is_dir()) {
                        if ui.button("↑ 上一级").clicked() {
                            descend = Some(p.to_path_buf());
                        }
                    }
                    if busy && ui.button("取消").clicked() {
                        want_cancel = true;
                    }
                });

                // 水位线
                match &space {
                    Some(s) => {
                        let ratio = s.used_ratio().unwrap_or(0.0);
                        let w = ui.available_width();
                        let rect = egui::Rect::from_min_size(
                            ui.cursor().min,
                            egui::vec2(w.max(80.0), 18.0),
                        );
                        ui.painter().rect_filled(rect, 6.0, ui.visuals().faint_bg_color);
                        ui.painter().rect_filled(
                            egui::Rect::from_min_size(
                                rect.min,
                                egui::vec2(rect.width() * ratio, rect.height()),
                            ),
                            6.0,
                            if ratio > 0.92 {
                                Color32::from_rgb(220, 50, 50)
                            } else if ratio > 0.8 {
                                Color32::from_rgb(230, 191, 0)
                            } else {
                                Color32::from_rgb(0, 122, 255)
                            },
                        );
                        ui.allocate_at_least(rect.size(), egui::Sense::hover());
                        ui.label(format!(
                            "剩余 {} / 共 {}（已用 {:.0}%）",
                            fmt_size(s.free),
                            fmt_size(s.total),
                            ratio * 100.0
                        ));
                        // 结论先行：只剩一点时直接说动谁有用，别让用户自己换算条子
                        if s.free < 5 * 1024 * 1024 * 1024 {
                            let c = ui.visuals().error_fg_color;
                            let mut lines = vec![format!(
                                "只剩 {}：装一个常见的更新包就可能不够。",
                                fmt_size(s.free)
                            )];
                            if let Some(u) = &usage {
                                for b in u.sorted().into_iter().take(3) {
                                    lines.push(format!(
                                        "  最大的一支：{}（{}，{} 个文件）",
                                        b.name,
                                        fmt_size(b.bytes),
                                        b.entries
                                    ));
                                }
                            }
                            ui.colored_label(c, lines.join("\n"));
                        }
                    }
                    None => {
                        let c = ui.visuals().error_fg_color;
                        ui.colored_label(
                            c,
                            "这一层的剩余空间读不到（只实现了 Windows 的卷查询）。读不到就说读不到，不画成空盘。",
                        );
                    }
                }

                ui.separator();
                match &usage {
                    None => {
                        ui.label("（还没起扫描）");
                    }
                    Some(u) => {
                        if let Some(err) = &u.error {
                            let c = ui.visuals().error_fg_color;
                            ui.colored_label(c, format!("扫描：{err}"));
                        }
                        ui.label(format!(
                            "{} · 已扫 {} 个文件 · 合计 {}",
                            if u.done {
                                "扫完了"
                            } else {
                                "还在算（下面的数会一路长）"
                            },
                            u.visited,
                            fmt_size(u.total())
                        ));
                        let total = u.total().max(1);
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if u.loose_bytes > 0 {
                                    bar(
                                        ui,
                                        "（这一层散落的文件）",
                                        u.loose_bytes,
                                        u.loose_bytes as f64 / total as f64,
                                        0,
                                        false,
                                    );
                                }
                                for b in u.sorted() {
                                    let frac = b.bytes as f64 / total as f64;
                                    if bar(ui, &b.name, b.bytes, frac, b.errors, !u.done) {
                                        descend = Some(b.path.clone());
                                    }
                                }
                            });
                    }
                }
                ui.separator();
                ui.label(
                    egui::RichText::new(
                        "点一条进到那一层重算。这里只统计大小，不删任何东西；\
                         没扫完之前数字是半截的，别当结论用。",
                    )
                    .size(12.0)
                    .weak(),
                );
            });

        self.usage_open = open;
        if want_rescan {
            self.start_usage_scan(root);
        }
        if want_cancel {
            if let Some(r) = self.usage.as_ref() {
                r.cancel();
            }
        }
        if let Some(p) = descend {
            self.start_usage_scan(p);
        }
    }

    fn open_startup(&mut self) {
        self.startup_open = true;
        if self.startup.is_none() {
            self.reload_startup();
        }
    }

    fn reload_startup(&mut self) {
        match fmnr_fs::startup::list() {
            Ok(inv) => {
                self.startup = Some(inv);
                self.startup_error = None;
            }
            Err(e) => {
                self.startup = None;
                self.startup_error = Some(e.to_string());
            }
        }
    }

    /// 自启动管理器：全量列出，用户级的可开关，机器级只读并说明原因。
    fn render_startup(&mut self, ctx: &egui::Context) {
        if !self.startup_open {
            return;
        }
        // 先取成 owned 值：闭包里要按名字回写 self.startup 之外的字段
        let inv = self.startup.clone();
        let err = self.startup_error.clone();
        let mut toggle: Option<(fmnr_fs::startup::StartupItem, bool)> = None;
        let mut want_reload = false;
        let mut open = self.startup_open;

        egui::Window::new("开机自启动")
            .id(egui::Id::new("startup"))
            .default_size([620.0, 420.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("重新读取").clicked() {
                        want_reload = true;
                    }
                    ui.label(
                        egui::RichText::new("关掉一项会先把它的命令行抄进 HKCU\\Software\\fmnr\\startup-off 再删，所以能开回来。")
                            .size(12.0)
                            .weak(),
                    );
                });
                if let Some(e) = err {
                    let c = ui.visuals().error_fg_color;
                    ui.colored_label(c, e);
                }
                let Some(inv) = inv else {
                    return;
                };
                for w in &inv.warnings {
                    ui.colored_label(Color32::from_rgb(230, 89, 38), format!("有一处没读到：{w}"));
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let mut last: Option<fmnr_fs::startup::Origin> = None;
                        for it in &inv.items {
                            if Some(it.origin) != last {
                                last = Some(it.origin);
                                ui.separator();
                                ui.label(egui::RichText::new(it.origin.label()).strong());
                            }
                            ui.horizontal(|ui| {
                                let name = if it.turned_off_by_us {
                                    format!("（已关）{}", it.name)
                                } else {
                                    it.name.clone()
                                };
                                ui.label(name);
                                if it.origin.editable() {
                                    let verb = if it.turned_off_by_us { "开回来" } else { "关掉" };
                                    if ui.button(verb).clicked() {
                                        toggle = Some((it.clone(), it.turned_off_by_us));
                                    }
                                } else if let Some(r) = it.origin.reason() {
                                    ui.weak(r);
                                }
                            });
                            ui.label(
                                egui::RichText::new(it.command.clone()).size(11.0).weak(),
                            );
                        }
                    });
            });

        self.startup_open = open;
        if want_reload {
            self.reload_startup();
        }
        if let Some((item, was_off)) = toggle {
            let res = if was_off {
                fmnr_fs::startup::turn_on(&item)
            } else {
                fmnr_fs::startup::turn_off(&item)
            };
            match res {
                Ok(n) => {
                    self.status = if was_off {
                        format!("已开回「{}」（{n} 步）", item.name)
                    } else {
                        format!("已关掉「{}」，原值存在镜像里（{n} 步）", item.name)
                    };
                    self.reload_startup();
                }
                Err(e) => {
                    self.status = format!("调整「{}」失败：{e}", item.name);
                    self.reload_startup();
                }
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 系统剪贴板：上一帧右键"复制完整路径"攒下的文本在这里落地。
        if let Some(text) = self.clipboard_text.take() {
            ctx.copy_text(text);
        }
        self.handle_keys(ctx);
        self.handle_dropped_files(ctx);
        self.poll_running();
        self.poll_search(ctx);
        self.poll_pkg(ctx);
        self.poll_usage(ctx);
        // 拖拽目标每帧重算：它是"这一帧指针悬在哪个目录上"的结论。
        self.drop_target = None;

        // 工具栏分两行。原来 12 个控件挤在一行：窄窗口下"排序"下拉直接被裁掉，
        // 而且看不出哪些是"我在哪"、哪些是"对选中的东西动手"。
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("fmnr").strong());
                ui.separator();
                if ui.button("⬅ 上级").clicked() {
                    self.go_up();
                }
                if ui.button("⟳ 刷新").clicked() {
                    self.refresh();
                }
                if ui.button("🔎 搜索").clicked() {
                    self.open_search();
                }
                ui.separator();
                // M8 卷接线：卷列表来自 VolumeRegistry，不是写死的本地盘。
                // 注册进 registry 的卷（归档/WSL/SSH）会自动出现在这里。
                let volumes = self.volume_registry.list();
                if !volumes.is_empty() {
                    let current = self
                        .current_volume_id
                        .as_ref()
                        .map(|id| id.0.clone())
                        .unwrap_or_default();
                    let mut picked: Option<VolumeId> = None;
                    egui::ComboBox::from_id_salt("volume")
                        .selected_text(short_volume_name(&current))
                        .show_ui(ui, |ui| {
                            for v in &volumes {
                                let label = format!(
                                    "{}{}",
                                    short_volume_name(&v.id.0),
                                    if v.writable { "" } else { "（只读）" }
                                );
                                if ui.selectable_label(v.id.0 == current, label).clicked() {
                                    picked = Some(v.id.clone());
                                }
                            }
                        });
                    if let Some(id) = picked {
                        self.switch_volume(id);
                    }
                }
                // M8 挂载入口：归档/WSL/SSH 卷在这里注册进同一个 registry。
                if ui.button("📦 挂载").clicked() {
                    self.mount_dialog = Some(MountDialog::new());
                }
                if ui.button("⚙ 设置").clicked() {
                    self.open_settings();
                }
                if ui.button("📊 空间").clicked() {
                    self.open_usage();
                }
                // 自启动放在顶栏常驻，不藏进设置面板：用户要的是"随时能看到、
                // 随时能改"，而藏在设置里等于每次都要开一个窗口再翻一层。
                if ui.button("🚀 自启动").clicked() {
                    self.open_startup();
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // 只在白板视图里有意义：平移走远后靠它找回自己的东西。
                    if self.view == ViewMode::Board && ui.button("🔍 适应全部").clicked() {
                        self.fit_board();
                    }
                    if ui
                        .selectable_label(self.view == ViewMode::Board, "🗺 白板")
                        .clicked()
                    {
                        self.view = ViewMode::Board;
                    }
                    if ui
                        .selectable_label(self.view == ViewMode::Panes, "▥ 双栏")
                        .clicked()
                    {
                        self.view = ViewMode::Panes;
                    }
                });
            });

            ui.horizontal(|ui| {
                if ui.button("📋 复制").clicked() {
                    self.put_clipboard(crate::ops::ClipMode::Copy);
                }
                if ui.button("✂ 剪切").clicked() {
                    self.put_clipboard(crate::ops::ClipMode::Cut);
                }
                if ui.button("📥 粘贴").clicked() {
                    self.paste();
                }
                if ui.button("🗑 删除").clicked() {
                    self.delete_selected();
                }
                if ui.button("✎ 重命名").clicked() {
                    self.begin_rename();
                }
                if ui.button("＋ 新建目录").clicked() {
                    self.begin_new_dir();
                }
                if ui.button("📌 钉到白板").clicked() {
                    self.pin_selected_to_board();
                }
                ui.separator();
                // 这两个改变的是列表内容本身，不重列的话要等下次导航才见效——
                // 用户勾完看到列表纹丝不动，只会以为勾选框没接上。
                if ui.checkbox(&mut self.show_hidden, "隐藏项").changed() {
                    self.refresh();
                }
                let mut sort_changed = false;
                egui::ComboBox::new("sort", "排序")
                    .selected_text(sort_label(self.sort))
                    .show_ui(ui, |ui| {
                        for (order, name) in [
                            (SortOrder::Name, "名称"),
                            (SortOrder::Size, "大小"),
                            (SortOrder::Modified, "修改时间"),
                            (SortOrder::Type, "类型"),
                        ] {
                            sort_changed |=
                                ui.selectable_value(&mut self.sort, order, name).changed();
                        }
                    });
                if sort_changed {
                    self.refresh();
                }
            });
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // 进度优先占位：操作进行中状态栏显示进度而不是静态文案
                if let Some(r) = self.running.as_ref() {
                    let snap = r.snapshot();
                    ui.add(
                        egui::ProgressBar::new(snap.fraction())
                            .desired_width(ui.available_width() - 340.0)
                            .animate(true),
                    );
                    ui.label(r.bar_text(snap));
                    if ui.button("中止").clicked() {
                        r.cancel();
                    }
                } else {
                    ui.label(&self.status);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(self.clipboard.summary());
                    // 没装到 CJK 字体时整屏中文都是豆腐块，而用户分不清
                    // "程序坏了"和"这台机没字体"——这条必须一直挂在状态栏上。
                    if !self.cjk_font_ok {
                        ui.label(
                            egui::RichText::new("⚠ 缺中文字体")
                                .color(Color32::from_rgb(185, 38, 48)),
                        )
                        .on_hover_text(
                            "没找到任何 CJK 字体，界面中文显示成 □□。\
                             候选路径与安装办法见 fmnr-ui/src/fonts.rs",
                        );
                    }
                    // 插件状态要**可见**：坏插件静默失败的话，用户看到的现象是
                    // 「插件装上了但不生效」，没有线索可查。
                    if !self.plugin_warnings.is_empty() {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} 个插件加载失败",
                                self.plugin_warnings.len()
                            ))
                            .color(Color32::from_rgb(220, 50, 50)),
                        )
                        .on_hover_text(
                            self.plugin_warnings
                                .iter()
                                .map(|w| format!("{}：{}", w.path.display(), w.error))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        );
                    } else if !self.plugins.is_empty() {
                        ui.label(format!("插件 {}", self.plugins.len()));
                    }
                });
            });
        });

        // 目录树侧栏：只在双栏且搜索面板没展开时占位。
        // SidePanel 必须在 CentralPanel 之前声明，否则拿不到剩余宽度。
        if self.view == ViewMode::Panes && !self.search_open {
            self.render_tree(ctx);
        }

        // 搜索面板展开时接管中央区（双栏暂时让位，回来时列表状态不变）。
        if self.search_open {
            self.render_search_panel(ctx);
        } else if self.view == ViewMode::Board {
            egui::CentralPanel::default().show(ctx, |ui| {
                self.render_board(ui);
            });
        } else {
            egui::CentralPanel::default().show(ctx, |ui| {
                let avail = ui.available_size();
                let half = avail.x / 2.0;
                ui.horizontal_top(|ui| {
                    self.pane(ui, Pane::Left, half);
                    self.pane(ui, Pane::Right, half);
                });
            });
        }

        // 拖拽落地：释放那一帧才知道目标，而目标正是上面画两栏时记下的。
        if self.drag.is_some() {
            let released = ctx.input(|i| i.pointer.button_released(egui::PointerButton::Primary));
            let copy = ctx.input(|i| i.modifiers.command);
            if released {
                self.finish_drag(copy);
            } else {
                // 指针位置要在 `ctx.input` 里读出来，但**画图必须在闭包外**：
                // `Context::input` 拿的是 Context 的写锁，而 `layer_painter()`
                // （内部 `screen_rect()` → 又一次 `input()`）和 `style()` 也要这把锁。
                // 写在闭包里的话不是报错而是**整条 UI 线程自锁死**——窗口从此
                // 不响应、不再出帧，只有拖一下才踩得到。
                let hover = ctx.input(|i| i.pointer.hover_pos());
                if let Some(pt) = hover {
                    let text = match (self.drag.as_ref(), copy) {
                        (Some(d), true) => format!("复制 {} 项", d.paths.len()),
                        (Some(d), false) => format!("移动 {} 项", d.paths.len()),
                        (None, _) => String::new(),
                    };
                    if !text.is_empty() {
                        let style = ctx.style();
                        let painter = ctx.layer_painter(egui::LayerId::new(
                            egui::Order::Foreground,
                            egui::Id::new("drag_ghost"),
                        ));
                        let at = pt + egui::vec2(14.0, 14.0);
                        let g = painter.layout_no_wrap(
                            text,
                            egui::FontId::proportional(12.0),
                            style.visuals.text_color(),
                        );
                        let box_rect =
                            egui::Rect::from_min_size(at, g.size() + egui::vec2(12.0, 6.0));
                        painter.rect_filled(box_rect, 6.0, style.visuals.widgets.hovered.bg_fill);
                        painter.galley(at + egui::vec2(6.0, 3.0), g, egui::Color32::PLACEHOLDER);
                    }
                }
                // 拖拽中要连续重绘，否则高亮不跟手、幽灵标签会停在原地。
                ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                ctx.request_repaint();
            }
        }

        self.render_modals(ctx);

        // M3 统一预览面板：浮动窗口，Esc 关闭。
        if self.preview.is_some() {
            let mut open = true;
            let title = self
                .preview
                .as_ref()
                .map(|p| p.path.display().to_string())
                .unwrap_or_default();
            egui::Window::new(title)
                .id(egui::Id::new("preview"))
                .open(&mut open)
                .default_width(520.0)
                .default_height(420.0)
                .resizable(true)
                .show(ctx, |ui| {
                    self.render_preview(ui, ctx);
                });
            // `open` 每帧都是新写的 true，所以必须在这里读它：
            // 点标题栏的 ✕ 只会把这个局部变量置 false，不读就等于关不掉窗口。
            if !open || ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.preview = None;
            }
        }
    }
}

impl App {
    /// 冲突策略弹窗：让用户显式选，不静默覆盖。
    ///
    /// 三段式（取出 → 画 → 写回）是为了避开 egui 闭包对 `self` 的独占捕获，
    /// 否则闭包里没法同时读状态和改状态。
    fn render_modals(&mut self, ctx: &egui::Context) {
        if let Some(mut prompt) = self.conflict.take() {
            let mut open = true;
            egui::Window::new("目标已存在")
                .id(egui::Id::new("conflict"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!("{} 项与目标同名：", prompt.conflicting.len()));
                    egui::ScrollArea::vertical()
                        .max_height(160.0)
                        .show(ui, |ui| {
                            for name in prompt.conflicting.iter().take(20) {
                                ui.label(format!("  · {name}"));
                            }
                            if prompt.conflicting.len() > 20 {
                                ui.label(format!("  … 另有 {} 项", prompt.conflicting.len() - 20));
                            }
                        });
                    ui.add_space(8.0);
                    ui.label("如何处理？");
                    ui.horizontal(|ui| {
                        ui.selectable_value(
                            &mut prompt.choice,
                            fmnr_core::ConflictPolicy::Rename,
                            "自动改名（推荐）",
                        );
                        ui.selectable_value(
                            &mut prompt.choice,
                            fmnr_core::ConflictPolicy::Skip,
                            "跳过",
                        );
                        ui.selectable_value(
                            &mut prompt.choice,
                            fmnr_core::ConflictPolicy::Overwrite,
                            "覆盖",
                        );
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("确定").clicked() {
                            open = false;
                        }
                        if ui.button("取消").clicked() {
                            open = false;
                            prompt.choice = fmnr_core::ConflictPolicy::Skip;
                            prompt.sources.clear(); // 空源 = 不执行
                        }
                    });
                });
            if open {
                self.conflict = Some(prompt);
            } else if prompt.sources.is_empty() {
                self.status = "已取消操作".into();
            } else {
                self.start_op(prompt.kind, prompt.sources, prompt.dest_dir, prompt.choice);
            }
        }

        if let Some(mut text) = self.renaming.take() {
            let mut submit = false;
            let mut cancel = false;
            let error = self.name_error.clone();
            let want_focus = self.name_focus;
            self.name_focus = false;
            egui::Window::new("重命名")
                .id(egui::Id::new("rename"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut text)
                            .desired_width(260.0)
                            .hint_text("新名称"),
                    );
                    // 打开即抢焦点：不抢的话回车条件永不成立，只能拿鼠标点确定。
                    if want_focus {
                        resp.request_focus();
                    }
                    if resp.has_focus() {
                        self.text_focus = true;
                    }
                    // 回车提交。原来判的是 `resp.lost_focus() && Enter`，
                    // 而这个输入框打开时根本没拿到焦点 ⇒ 条件永不成立，
                    // 只能靠鼠标点「确定」。
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        submit = true;
                    }
                    if let Some(err) = error {
                        let c = ui.visuals().error_fg_color;
                        ui.colored_label(c, err);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("确定").clicked() {
                            submit = true;
                        }
                        if ui.button("取消").clicked() {
                            cancel = true;
                        }
                    });
                });
            if submit {
                self.renaming = Some(text);
                self.commit_rename();
            } else if cancel {
                self.renaming = None;
                self.name_error = None;
            } else {
                self.renaming = Some(text);
            }
        }

        if let Some(mut text) = self.new_dir_name.take() {
            let mut submit = false;
            let mut cancel = false;
            let error = self.name_error.clone();
            let want_focus = self.name_focus;
            self.name_focus = false;
            egui::Window::new("新建目录")
                .id(egui::Id::new("newdir"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut text)
                            .desired_width(260.0)
                            .hint_text("目录名"),
                    );
                    if want_focus {
                        resp.request_focus();
                    }
                    if resp.has_focus() {
                        self.text_focus = true;
                    }
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        submit = true;
                    }
                    if let Some(err) = error {
                        let c = ui.visuals().error_fg_color;
                        ui.colored_label(c, err);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("确定").clicked() {
                            submit = true;
                        }
                        if ui.button("取消").clicked() {
                            cancel = true;
                        }
                    });
                });
            if submit {
                self.new_dir_name = Some(text);
                self.commit_new_dir();
            } else if cancel {
                self.new_dir_name = None;
                self.name_error = None;
            } else {
                self.new_dir_name = Some(text);
            }
        }

        self.render_mount_dialog(ctx);
        self.render_pkg_panel(ctx);
        self.render_settings(ctx);
        self.render_usage(ctx);
        self.render_startup(ctx);
    }

    /// 挂载对话框：类型三选一 + 入参 + 结果反馈。
    fn render_mount_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dlg) = self.mount_dialog.take() else {
            return;
        };
        let mut submit = false;
        let mut cancel = false;
        egui::Window::new("挂载卷")
            .id(egui::Id::new("mount"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut dlg.kind, MountKind::Archive, "归档");
                    ui.selectable_value(&mut dlg.kind, MountKind::Wsl, "WSL");
                    ui.selectable_value(&mut dlg.kind, MountKind::Ssh, "SSH");
                });
                match dlg.kind {
                    MountKind::Archive => {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut dlg.input)
                                .desired_width(320.0)
                                .hint_text(r"D:\path\to\a.zip"),
                        );
                        if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                            submit = true;
                        }
                        ui.label("支持 zip / tar / tar.gz / tgz / tar.bz2；只读");
                    }
                    MountKind::Wsl => match &mut dlg.wsl_distros {
                        Some(Ok(list)) if !list.is_empty() => {
                            egui::ComboBox::from_label("发行版")
                                .selected_text(
                                    list.get(dlg.wsl_picked)
                                        .cloned()
                                        .unwrap_or_else(|| "（无）".into()),
                                )
                                .show_ui(ui, |ui| {
                                    for (i, d) in list.iter().enumerate() {
                                        ui.selectable_value(&mut dlg.wsl_picked, i, d);
                                    }
                                });
                            ui.label("发现自 wsl -l -q；写入走 Windows UNC");
                        }
                        Some(Ok(_)) => {
                            ui.label("wsl -l -q 返回空列表：没有任何发行版");
                        }
                        Some(Err(e)) => {
                            ui.label(format!("发现失败：{e}；可手动输入 distro 名"));
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut dlg.input)
                                    .desired_width(260.0)
                                    .hint_text("distro 名，如 Ubuntu-24.04"),
                            );
                            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                                submit = true;
                            }
                        }
                        None => {
                            ui.label("正在发现发行版…");
                        }
                    },
                    MountKind::Ssh => {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut dlg.input)
                                .desired_width(320.0)
                                .hint_text("ssh://user@host[:port][/path]"),
                        );
                        if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                            submit = true;
                        }
                        ui.label("认证走本机 ssh-agent；未知主机键会被拒绝");
                    }
                }
                ui.horizontal(|ui| {
                    if ui.button("挂载").clicked() {
                        submit = true;
                    }
                    if ui.button("取消").clicked() {
                        cancel = true;
                    }
                });
            });
        if submit {
            self.commit_mount(dlg);
        } else if cancel {
            // 取消只丢对话框，不动已挂的卷。
        } else {
            self.mount_dialog = Some(dlg);
        }
    }

    /// 执行挂载：成功注册并切过去；失败把原因写进状态栏 + 挂载记录。
    ///
    /// 走 `mount_all`（fmnr-mount 的受测路径），不在 UI 里另写一套挂载逻辑——
    /// 两套逻辑迟早分叉，而失败处理这种地方分叉就是静默 bug。
    fn commit_mount(&mut self, dlg: MountDialog) {
        let Some(spec) = dlg.spec() else {
            self.status = "挂载：输入为空".to_string();
            return;
        };
        let display = spec.display();
        let failures = self
            .mount_table
            .mount_all(&self.volume_registry, std::slice::from_ref(&spec));
        let last_id = self
            .mount_table
            .entries()
            .last()
            .and_then(|r| r.volume_id.clone());
        match last_id {
            Some(id) if self.current_volume_id.as_ref() == Some(&id) => {
                self.status = format!("卷 {display} 已在仓中");
            }
            Some(id) => {
                self.status = format!("已挂载 {display}");
                self.switch_volume(id);
            }
            None => {
                // 失败原因已经由 mount_all 格式化好（带 spec 前缀）。
                self.status = format!(
                    "挂载失败：{}",
                    failures.first().cloned().unwrap_or_default()
                );
            }
        }
    }

    /// 搜索面板：输入框 + 增量结果。
    ///
    /// 命中行要自己造命中区：`ui.horizontal(..).response` 的 Sense 是 hover，
    /// 挂在它上面的 `double_clicked()` 恒为 false——原来双击结果没反应就是这个原因。
    fn render_search_panel(&mut self, ctx: &egui::Context) {
        let root = self.search_root.clone();
        let want_focus = self.search_focus;
        self.search_focus = false;

        egui::TopBottomPanel::top("searchbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("🔎");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.search_text)
                        .hint_text("在这个子树里搜索…")
                        .desired_width(320.0),
                );
                if want_focus {
                    resp.request_focus();
                }
                if resp.has_focus() {
                    self.text_focus = true;
                }
                // 逐字符触发增量搜索：不等回车
                if resp.changed() {
                    self.start_search();
                }
                if ui.button("关闭").clicked() {
                    self.close_search();
                }
            });
            ui.label(
                egui::RichText::new(format!(
                    "范围 {} · 单击定位 · 双击打开 · Esc 返回",
                    root.display()
                ))
                .weak(),
            );
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let started = self
                .search
                .as_ref()
                .map(|s| (s.hits.clone(), s.total, s.done, s.elapsed_ms));
            let Some((hits, total, done, ms)) = started else {
                ui.label(
                    egui::RichText::new("输入即搜：不用回车，也不用等整棵树扫完才看到第一批结果。")
                        .weak(),
                );
                return;
            };

            ui.horizontal(|ui| {
                if done {
                    ui.label(format!("{total} 个结果 · {ms} ms"));
                } else {
                    ui.label(format!("搜索中… 已列出 {} 个", hits.len()));
                }
            });
            ui.separator();

            if done && hits.is_empty() {
                // 归档/远程卷上"0 个结果"是**假**的：搜索线程走的是 jwalk，
                // 只能走本地文件系统，包内条目它根本看不见。
                // 报"没有匹配项"等于把"我搜不了"说成"这里没有"。
                let searchable = matches!(
                    self.current_volume().map(|v| v.kind()),
                    None | Some(fmnr_core::VolumeKind::Local)
                        | Some(fmnr_core::VolumeKind::Unc)
                        | Some(fmnr_core::VolumeKind::Wsl)
                );
                let msg = if searchable {
                    format!("「{}」在这个子树里没有匹配项", self.search_text.trim())
                } else {
                    format!(
                        "「{}」：这个卷的子树搜索还没接上（搜索目前只走本地文件系统），                         所以这里说的是\"搜不了\"，不是\"没有\"",
                        self.search_text.trim()
                    )
                };
                ui.label(egui::RichText::new(msg).weak());
                return;
            }

            let mut jump: Option<(PathBuf, bool)> = None;
            let mut locate: Option<PathBuf> = None;
            let width = (ui.available_width() - 16.0).max(120.0);
            egui::ScrollArea::vertical().show_rows(ui, 22.0, hits.len(), |ui, range| {
                for i in range {
                    let hit = &hits[i];
                    let row_rect =
                        egui::Rect::from_min_size(ui.cursor().min, egui::vec2(width, 22.0));
                    let resp =
                        ui.interact(row_rect, ui.id().with(("hit", i)), egui::Sense::click());
                    if resp.hovered() {
                        ui.painter()
                            .rect_filled(row_rect, 6.0, ui.visuals().faint_bg_color);
                    }
                    // 命中片段高亮：命中段用强调色，其余普通
                    let label = highlight_hit(hit, ui.style());
                    let icon = if hit.is_dir { "📁" } else { "📄" };
                    ui.horizontal(|ui| {
                        ui.label(icon);
                        ui.add(egui::Label::new(label).selectable(false));
                        ui.allocate_ui_with_layout(
                            egui::vec2(width - 40.0, 22.0),
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                let parent = hit
                                    .path
                                    .parent()
                                    .map(|p| truncate_label(&p.display().to_string(), 46))
                                    .unwrap_or_default();
                                ui.add(
                                    egui::Label::new(egui::RichText::new(parent).weak().size(12.0))
                                        .selectable(false),
                                );
                                let size = if hit.is_dir {
                                    String::new()
                                } else {
                                    fmt_size(hit.size)
                                };
                                ui.label(egui::RichText::new(size).weak().monospace());
                            },
                        );
                    });
                    let resp = resp.on_hover_text(hit.path.display().to_string());
                    if resp.double_clicked() {
                        jump = Some((hit.path.clone(), hit.is_dir));
                    } else if resp.clicked() {
                        // 单击也要有反应：只把这一条记下来，收掉面板后
                        // 两栏就落在它的位置上。原来单击什么都不做，
                        // 一整行看起来可点、实际是死的。
                        locate = Some(hit.path.clone());
                    }
                }
            });
            if let Some((path, is_dir)) = jump {
                if is_dir {
                    self.enter(path, true);
                } else {
                    // 先把两栏落到那个文件上再打开。原来双击完两栏还停在
                    // 搜索前的目录，用户看不出自己开的是哪里的文件。
                    self.reveal_in_panes(&path);
                    self.spawn_external(&path);
                }
                self.close_search();
            } else if let Some(path) = locate {
                self.reveal_in_panes(&path);
                self.close_search();
            }
        });
    }
}

impl App {
    fn handle_keys(&mut self, ctx: &egui::Context) {
        // 上一帧有文本框持焦 ⇒ 用户在打字，快捷键必须让行。
        // 不拦的话在左栏过滤框里按退格，会一边删掉一个字符、
        // 一边让**右栏**跳到上级目录：处理器根本不知道焦点在哪。
        let typing = std::mem::take(&mut self.text_focus);

        // 弹窗打开时，键盘全归弹窗（否则 Backspace 会同时删输入和跳目录）
        if self.conflict.is_some() || self.renaming.is_some() || self.new_dir_name.is_some() {
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.conflict = None;
                self.renaming = None;
                self.new_dir_name = None;
            }
            return;
        }

        // Ctrl+F 唤起搜索。排在文本框闸门之前：从过滤框里也能唤起搜索面板。
        if ctx.input_mut(|i| {
            i.consume_key(Modifiers::CTRL, Key::F)
                || i.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::F)
        }) {
            self.open_search();
            return;
        }

        // 搜索面板开着时 Esc 收掉它（连同结果列表一起退出这个模式）。
        if self.search_open && ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.close_search();
            return;
        }

        if typing {
            return;
        }

        ctx.input(|i| {
            if i.key_pressed(Key::Tab) {
                self.active_pane = match self.active_pane {
                    Pane::Left => Pane::Right,
                    Pane::Right => Pane::Left,
                };
            }
            if i.key_pressed(Key::Backspace) {
                self.go_up();
            }
            if i.key_pressed(Key::Enter) {
                // 目录/文件要问卷，不能问 `path.is_dir()`：归档卷里的
                // `/doc/readme.txt` 在本地文件系统上不存在，is_dir() 一律 false，
                // 于是 zip 里的文件夹回车会变成"用外部程序打开一个不存在的文件"。
                if let Some(name) = self.selection_of(self.active_pane) {
                    let is_dir = match self.active_pane {
                        Pane::Left => &self.left_entries,
                        Pane::Right => &self.right_entries,
                    }
                    .iter()
                    .any(|e| e.name == name && e.is_dir);
                    let path = self.active_path().join(&name);
                    self.enter(path, is_dir);
                }
            }
        });

        // 组合键：文本复制/剪切/粘贴
        ctx.input_mut(|i| {
            let ctrl = i.modifiers.ctrl;
            if ctrl && i.key_pressed(Key::C) {
                self.put_clipboard(crate::ops::ClipMode::Copy);
            }
            if ctrl && i.key_pressed(Key::X) {
                self.put_clipboard(crate::ops::ClipMode::Cut);
            }
            if ctrl && i.key_pressed(Key::V) {
                self.paste();
            }
        });

        // 功能键
        ctx.input(|i| {
            if i.key_pressed(Key::F3) {
                self.open_search();
            }
            if i.key_pressed(Key::F5) {
                let dest = self.other_pane_path();
                let paths = self.selected_paths();
                if paths.is_empty() {
                    self.status = "没有选中任何项".into();
                } else if paths.len() == 1 {
                    // F5 = 复制到对栏当前目录
                    self.start_op(
                        crate::ops::OpKind::Copy,
                        paths.into_iter().map(VPath).collect(),
                        Some(VPath(dest)),
                        fmnr_core::ConflictPolicy::Rename,
                    );
                } else {
                    self.status = "多选未实现，先用 Ctrl+C → 对栏 Ctrl+V".into();
                }
            }
            if i.key_pressed(Key::F6) {
                let dest = self.other_pane_path();
                let paths = self.selected_paths();
                if !paths.is_empty() {
                    self.start_op(
                        crate::ops::OpKind::Move,
                        paths.into_iter().map(VPath).collect(),
                        Some(VPath(dest)),
                        fmnr_core::ConflictPolicy::Rename,
                    );
                }
            }
            if i.key_pressed(Key::Delete) {
                self.delete_selected();
            }
            if i.key_pressed(Key::F2) {
                self.begin_rename();
            }
            if i.key_pressed(Key::F7) {
                self.begin_new_dir();
            }
        });
    }

    /// 执行右键菜单选中的动作。
    ///
    /// 全部走既有的方法，不在这里另写一套动作层：工具栏按钮、快捷键和右键
    /// 三条入口必须做同一件事，否则会出现"右键能删、按钮删不掉"这种分叉。
    fn run_row_action(&mut self, act: RowAction, path: &Path, is_dir: bool, which: Pane) {
        // 菜单是对哪一栏弹的，就把那一栏设为活动栏：
        // 在右栏右键"删除"却删了左栏选中的文件，是最糟糕的一种意外。
        self.active_pane = which;
        match act {
            // 没有单独的"预览"项：文件的 Open 本来就走预览
            // （enter → spawn_external → route_open 判为 Preview）。
            // 少一个动词，就少一处"两个菜单项做同一件事却可能不一致"。
            RowAction::Open => self.enter(path.to_path_buf(), is_dir),
            RowAction::OpenExternal => {
                // force_external = true：即使是可预览类型也交给外部程序。
                // 这是"预览"和"用默认程序打开"必须分成两项的原因。
                let route = core_route_open(path, true);
                if let OpenRoute::External(_) = route {
                    self.spawn_external(path);
                } else {
                    self.status = "这个类型没有可外部打开的程序".into();
                }
            }
            RowAction::Copy => self.put_clipboard(crate::ops::ClipMode::Copy),
            RowAction::Cut => self.put_clipboard(crate::ops::ClipMode::Cut),
            RowAction::Paste => self.paste(),
            RowAction::Delete => self.delete_selected(),
            RowAction::Rename => self.begin_rename(),
            RowAction::CopyPath => {
                let text = path.display().to_string();
                self.status = format!("已复制路径 {text}");
                self.clipboard_text = Some(text);
            }
            RowAction::PinToBoard => self.pin_selected_to_board(),
            RowAction::OpenInOtherPane => {
                let other = match which {
                    Pane::Left => Pane::Right,
                    Pane::Right => Pane::Left,
                };
                if is_dir {
                    self.navigate(other, path.to_path_buf());
                } else if let Some(parent) = path.parent() {
                    self.navigate(other, parent.to_path_buf());
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    self.set_selection_for(other, &name);
                }
            }
        }
    }

    /// 拖拽落地。目标目录是画那一栏时记下来的（`drop_target`）。
    ///
    /// 判定复用 `plan_drop`——那是"从外面拖进窗口"用的同一个纯函数
    /// （路径不存在、原地复制、文件夹拖进自己子树）。两条入口共用一份规则，
    /// 才不会出现"从外面拖进来被挡住、自己拖过去却把目录套嵌了"。
    fn finish_drag(&mut self, copy: bool) {
        let Some(drag) = self.drag.take() else { return };
        let Some(target) = self.drop_target.take() else {
            // 松手时没落在任何可接收的目录上：什么都不做，但要说一句，
            // 否则用户会以为已经搬走了。
            self.status = "未在目录上松手，未移动".into();
            return;
        };
        let sources = match plan_drop(&drag.paths, &target) {
            DropPlan::Ignore(reason) => {
                self.status = reason;
                return;
            }
            DropPlan::Copy(sources) => sources,
        };
        let kind = if copy {
            crate::ops::OpKind::Copy
        } else {
            crate::ops::OpKind::Move
        };
        if kind == crate::ops::OpKind::Move {
            if let Some(v) = self.current_volume() {
                if !v.writable() {
                    self.status = "这个卷是只读的，不能移动".into();
                    return;
                }
            }
        }
        let n = sources.len();
        let vpaths: Vec<VPath> = sources.into_iter().map(VPath).collect();
        let conflicts = self.find_conflicts(&vpaths, &target);
        if !conflicts.is_empty() {
            // 冲突交给既有的策略弹窗，和粘贴同一条路：不静默覆盖也不静默跳过。
            self.conflict = Some(ConflictPrompt {
                kind,
                sources: vpaths,
                dest_dir: Some(VPath(target.clone())),
                conflicting: conflicts,
                choice: fmnr_core::ConflictPolicy::Rename,
            });
            return;
        }
        self.start_op(
            kind,
            vpaths,
            Some(VPath(target.clone())),
            fmnr_core::ConflictPolicy::Skip,
        );
        if self.running.is_some() {
            let verb = if copy { "拖放复制" } else { "拖放移动" };
            self.status = format!("{verb} {n} 项 → {}", target.display());
        }
    }

    fn other_pane_path(&self) -> PathBuf {
        match self.active_pane {
            Pane::Left => self.right_path.clone(),
            Pane::Right => self.left_path.clone(),
        }
    }

    /// 白板画布：自绘节点卡片 + 拖拽 + 平移缩放。
    ///
    /// 位置是用户数据（DESIGN §7.1 空间记忆），所以这里**不做任何自动排布**：
    /// 新节点落在当前视图中心，之后只跟着鼠标走。
    fn render_board(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let origin = rect.min;
        self.board_viewport = rect;

        // 交互层：先占满整块画布，处理平移与缩放。
        let bg = ui.interact(
            rect,
            egui::Id::new("board_bg"),
            egui::Sense::click_and_drag(),
        );

        // 滚轮缩放（Ctrl 或直接滚轮都可，白板里滚轮没有别的用途）。
        if bg.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > 0.0 {
                self.board_cam.zoom_by((scroll * 0.002).exp());
            }
        }

        // 拖动空白处 = 平移画布。拖动节点时不平移。
        if self.board_drag.is_none() && bg.dragged() {
            self.board_cam.pan += bg.drag_delta();
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0, egui::Color32::from_rgb(246, 246, 248));

        let cam = self.board_cam;
        let card_font = egui::FontId::proportional(13.0);

        // 连线先画，节点卡片后画——否则线会盖在卡片上。
        for link in &self.board.links {
            let (Some(a), Some(b)) = (
                self.board.nodes.get(link.from),
                self.board.nodes.get(link.to),
            ) else {
                continue;
            };
            let ra = crate::board_view::card_rect(a.pos);
            let rb = crate::board_view::card_rect(b.pos);
            let ca = cam.to_screen(
                fmnr_core::board::Pos::new(ra.center().x, ra.center().y),
                origin,
            );
            let cb = cam.to_screen(
                fmnr_core::board::Pos::new(rb.center().x, rb.center().y),
                origin,
            );
            painter.line_segment(
                [ca, cb],
                egui::Stroke::new(1.5_f32, egui::Color32::from_rgb(150, 160, 175)),
            );
        }

        let mut drag_to: Option<(usize, fmnr_core::board::Pos)> = None;

        for idx in 0..self.board.nodes.len() {
            let node = self.board.nodes[idx].clone();
            let r = crate::board_view::card_rect(node.pos);
            let screen = egui::Rect::from_min_max(
                cam.to_screen(fmnr_core::board::Pos::new(r.min.x, r.min.y), origin),
                cam.to_screen(fmnr_core::board::Pos::new(r.max.x, r.max.y), origin),
            );

            // 节点卡片：缺文件的用红色描边区分，但**照样显示**（布局不能丢）。
            let fill = if node.missing {
                egui::Color32::from_rgb(255, 235, 235)
            } else {
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 235)
            };
            painter.rect_filled(screen, egui::CornerRadius::same(10), fill);
            let stroke_color = if node.missing {
                egui::Color32::from_rgb(220, 90, 90)
            } else {
                egui::Color32::from_rgb(200, 205, 215)
            };
            painter.rect_stroke(
                screen,
                egui::CornerRadius::same(10),
                egui::Stroke::new(1.0_f32, stroke_color),
                egui::StrokeKind::Inside,
            );

            // 标题。过长截断，免得溢出卡片。
            let label = truncate_label(&node.label, 22);
            painter.text(
                screen.left_top() + egui::vec2(10.0, 10.0),
                egui::Align2::LEFT_TOP,
                label,
                card_font.clone(),
                egui::Color32::from_rgb(30, 30, 35),
            );
            if node.missing {
                painter.text(
                    screen.left_bottom() + egui::vec2(10.0, -10.0),
                    egui::Align2::LEFT_BOTTOM,
                    "文件不在了",
                    egui::FontId::proportional(11.0),
                    egui::Color32::from_rgb(200, 70, 70),
                );
            }

            // 交互层叠在卡片上，处理拖拽与双击。
            let resp = ui.interact(
                screen,
                egui::Id::new(("board_node", idx)),
                egui::Sense::click_and_drag(),
            );
            if resp.drag_started() {
                self.board_drag = Some(idx);
            }
            if resp.dragged() && self.board_drag == Some(idx) {
                let d = cam.screen_delta_to_canvas(resp.drag_delta());
                drag_to = Some((
                    idx,
                    fmnr_core::board::Pos::new(node.pos.x + d.x, node.pos.y + d.y),
                ));
            }
            if resp.double_clicked() {
                // 双击：交给既有的打开语义（预览或外置程序 + 角标）。
                // 白板节点是本地路径（钉的时候从栏里取的），问文件系统是对的。
                let p = node.path.clone();
                let is_dir = p.is_dir();
                self.enter(p, is_dir);
            }
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
        }

        if let Some((idx, pos)) = drag_to {
            self.board.move_node(idx, pos);
        }
        if ui.input(|i| i.pointer.any_released()) {
            if self.board_drag.is_some() {
                // 拖动结束：落盘。位置是用户数据，改了就存。
                self.save_board();
            }
            self.board_drag = None;
        }

        // 空板提示。
        if self.board.nodes.is_empty() {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "白板是空的：在双栏视图里选中文件，点「📌 钉到白板」",
                egui::FontId::proportional(14.0),
                egui::Color32::from_rgb(140, 145, 155),
            );
        }
    }

    /// 把当前活动栏选中的条目钉到白板，落在**视图中心**。
    fn pin_selected_to_board(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            self.status = "没有选中任何项".into();
            return;
        }
        // 视图中心的画布坐标：走 to_canvas，这样缩放/平移后位置都算得对。
        // 之前用 pan 当中心，一缩放就会钉到看不见的地方。
        let vp_center = self.board_viewport.center();
        let center = self.board_cam.to_canvas(vp_center, self.board_viewport.min);

        let mut added = 0usize;
        for p in paths.iter() {
            // 已钉过的不重复添加（同路径只保留一个节点）。
            if self.board.nodes.iter().any(|node| &node.path == p) {
                continue;
            }
            // 落点稍微错开，避免完全重叠成一摞看不出有几个——但这只是**首次**
            // 落点，之后绝不重排。错开量按已有节点数递增，让新钉的往外排。
            let k = (self.board.nodes.len() + added) as f32;
            let pos = fmnr_core::board::Pos::new(center.x + k * 24.0, center.y + k * 18.0);
            self.board.add(p.clone(), pos);
            added += 1;
        }
        self.board.refresh_missing();
        self.save_board();
        if added == 0 {
            self.status = "选中的项都已在白板上".into();
        } else {
            self.status = format!("已钉到白板（{added} 项）");
        }
    }

    /// 把所有节点收进视野。用户平移走远后，这是唯一的「找回我的东西」入口。
    fn fit_board(&mut self) {
        let positions: Vec<fmnr_core::board::Pos> =
            self.board.nodes.iter().map(|n| n.pos).collect();
        let Some(content) = crate::board_view::bounds(&positions) else {
            self.status = "白板是空的".into();
            return;
        };
        // bounds 给的是画布坐标，fit_camera 要的是画布坐标 + 视口 + origin。
        match crate::board_view::fit_camera(content, self.board_viewport, self.board_viewport.min) {
            Some(cam) => {
                self.board_cam = cam;
                self.status = format!("已适应全部 {} 个节点", self.board.nodes.len());
            }
            None => self.status = "视口太小，无法适应".into(),
        }
    }

    /// 白板落盘。失败要报出来——静默失败会让用户以为布局存住了。
    fn save_board(&mut self) {
        let path = fmnr_core::board::Board::default_path(&self.board.name);
        if let Err(e) = self.board.save(&path) {
            self.status = format!("白板保存失败：{e}");
        }
    }

    /// 画目录树侧栏，并把"可见但还没列"的目录补上数据。
    ///
    /// 列举放在渲染开头而不是点击回调里：一次 runtime 列完所有待列节点，
    /// 且同一帧就能看到结果（点击时只改展开态，下一帧自然补数据）。
    fn render_tree(&mut self, ctx: &egui::Context) {
        let pending = self.tree.pending_loads();
        if !pending.is_empty() {
            if let Some(volume) = self.current_volume() {
                let rt = tokio::runtime::Runtime::new().unwrap();
                let opts = self.opts();
                for path in pending {
                    let level = match rt.block_on(volume.list(&VPath(path.clone()), &opts)) {
                        Ok(entries) => crate::tree::level_of(&path, &entries),
                        Err(e) => crate::tree::Level {
                            dirs: Vec::new(),
                            error: Some(e.to_string()),
                        },
                    };
                    self.tree.put_level(&path, level);
                }
            }
        }

        // 先把要画的取出来：闭包里不碰 self，点击结果留到闭包外再落到状态上。
        let rows = self.tree.rows();
        let selected = self.tree.selected().map(|p| p.to_path_buf());
        let mut toggle_to: Option<PathBuf> = None;
        let mut open_to: Option<PathBuf> = None;
        let mut want_pkg = false;

        egui::SidePanel::left("tree")
            .default_width(220.0)
            .width_range(150.0..=460.0)
            .show(ctx, |ui| {
                // 「更新」和目录树同一排：这一排是"这台机器上有什么"的入口，
                // 包盘点问的正是"有什么该动了"。它不列文件，所以不放栏头工具条。
                ui.horizontal(|ui| {
                    let n = self.pkg.rows.len();
                    let label = if self.pkg.rx.is_some() {
                        "更新 · 盘点中…".to_string()
                    } else if n > 0 {
                        format!("更新 {n}")
                    } else {
                        "更新".to_string()
                    };
                    if ui
                        .button(label)
                        .on_hover_text("问一次 winget 有哪些包可升级。只列清单，不代跑升级。")
                        .clicked()
                    {
                        want_pkg = true;
                    }
                });
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for row in rows {
                            let path = row.dir.path.clone();
                            let err = row.error.clone();
                            let indent = 12.0 * row.depth as f32;
                            ui.horizontal(|ui| {
                                ui.add_space(indent);
                                // 三角只管展开收起，点名字只管"去那儿"：
                                // 两件事挤在一次点击上，收起时就会顺手跳过去。
                                let mark = if row.expanded { "▾" } else { "▸" };
                                if ui
                                    .add(
                                        egui::Label::new(
                                            egui::RichText::new(mark).weak().size(11.0),
                                        )
                                        .selectable(false)
                                        .sense(egui::Sense::click()),
                                    )
                                    .clicked()
                                {
                                    toggle_to = Some(path.clone());
                                }
                                let mut label = egui::RichText::new(&row.dir.name);
                                if err.is_some() {
                                    label = label.color(Color32::from_rgb(185, 38, 48));
                                }
                                let resp = ui.add(egui::SelectableLabel::new(
                                    selected.as_deref() == Some(path.as_path()),
                                    label,
                                ));
                                resp.clone().on_hover_text(path.display().to_string());
                                match &err {
                                    // 读不了的目录必须标出来：静默显示成空，
                                    // 用户会以为这个目录真的没东西。
                                    Some(e) => {
                                        resp.clone().on_hover_text(format!("读不了这一层：{e}"));
                                    }
                                    None => {
                                        if row.known_leaf {
                                            resp.clone().on_hover_text("这个目录下没有子目录");
                                        }
                                    }
                                }
                                if resp.clicked() {
                                    open_to = Some(path);
                                }
                            });
                        }
                    });
            });

        if let Some(path) = toggle_to {
            self.tree.toggle(&path);
            // 展开态变了要再走一帧才会补数据。
            ctx.request_repaint();
        }
        if let Some(path) = open_to {
            self.tree.select(Some(path.clone()));
            self.navigate(self.active_pane, path);
        }
        if want_pkg {
            self.pkg.open = true;
            self.start_pkg_scan();
        }
    }

    /// 一栏：栏头 → 面包屑 → 过滤 → 行列表。
    ///
    /// 整行可点。原来只有文件名那几个像素是 `selectable_label` 的命中区，
    /// 点图标或行尾空白没有反应，可那一行看上去明明是一整条。
    fn pane(&mut self, ui: &mut egui::Ui, which: Pane, width: f32) {
        let path = self.path_of(which).clone();

        ui.push_id(which, |ui| {
            ui.vertical(|ui| {
                ui.set_width(width);

                // 栏头：活动栏是实心点 + 强调色。用 Tab 在两栏间切换时，
                // 这是唯一说明"现在操作的是哪一边"的可见信号。
                let active = self.active_pane == which;
                let head_text = if active {
                    egui::RichText::new(format!("● {}", path.display()))
                        .color(Color32::from_rgb(0, 122, 255))
                        .strong()
                } else {
                    egui::RichText::new(format!("○ {}", path.display())).weak()
                };
                let head = ui
                    .add(
                        egui::Label::new(head_text)
                            .truncate()
                            .sense(egui::Sense::click()),
                    )
                    .on_hover_text("设为当前栏（Tab 也可以切换）");
                if head.clicked() {
                    self.active_pane = which;
                }

                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    let crumbs = breadcrumbs(&path);
                    for (i, c) in crumbs.iter().enumerate() {
                        if ui.small_button(c).clicked() {
                            let target = crumbs_to_path(&path, i);
                            self.navigate(which, target);
                            self.active_pane = which;
                        }
                        if i + 1 < crumbs.len() {
                            ui.label("›");
                        }
                    }
                });

                let mut filter_focused = false;
                ui.horizontal(|ui| {
                    let filter = match which {
                        Pane::Left => &mut self.left_filter,
                        Pane::Right => &mut self.right_filter,
                    };
                    ui.label("🔍");
                    let resp = ui.add(
                        egui::TextEdit::singleline(filter)
                            .hint_text("按名称过滤")
                            .desired_width((width - 116.0).max(80.0)),
                    );
                    filter_focused = resp.has_focus();
                    if !filter.is_empty() && ui.button("✕").clicked() {
                        filter.clear();
                    }
                });
                self.text_focus |= filter_focused;
                ui.separator();

                // 列视图
                let base = self.path_of(which).clone();
                let error = match which {
                    Pane::Left => self.left_error.clone(),
                    Pane::Right => self.right_error.clone(),
                };
                let filter = match which {
                    Pane::Left => self.left_filter.clone(),
                    Pane::Right => self.right_filter.clone(),
                };
                let entries: Vec<&fmnr_core::Entry> = {
                    let all = match which {
                        Pane::Left => &self.left_entries,
                        Pane::Right => &self.right_entries,
                    };
                    if filter.is_empty() {
                        all.iter().collect()
                    } else {
                        let f = filter.to_lowercase();
                        all.iter()
                            .filter(|e| e.name.to_lowercase().contains(&f))
                            .collect()
                    }
                };

                // 选中的条目被过滤掉时取消选中。留着的话状态栏显示的还是刚才那条，
                // 而回车/删除作用的却是这个已经看不见的文件。
                {
                    let sel = match which {
                        Pane::Left => &mut self.left_selected,
                        Pane::Right => &mut self.right_selected,
                    };
                    if let Some(name) = sel.as_ref() {
                        if !entries.iter().any(|e| &e.name == name) {
                            *sel = None;
                        }
                    }
                }

                // 文件路径 -> 外置打开状态的索引（角标注入用）
                let badges: Vec<Option<fmnr_core::ExternalOpen>> = match self.current_volume() {
                    Some(v) => entries
                        .iter()
                        .map(|e| {
                            v.external_opens()
                                .get(base.join(&e.name).display().to_string())
                        })
                        .collect(),
                    None => entries.iter().map(|_| None).collect(),
                };

                // 图标纹理预先解析（闭包内不能可变借用 self.icons）。
                let ctx = ui.ctx().clone();
                let icon_handles: Vec<Option<egui::TextureHandle>> = entries
                    .iter()
                    .map(|e| self.icons.get(&ctx, &e.icon_key))
                    .collect();

                // 归档/远程卷里那些是虚拟路径，落地要用的本地文件系统操作对不上，
                // 所以干脆不发起拖拽——比"拖得动、松手没反应"诚实。
                // 必须在下面 `selected` 的可变借用之前问，否则 self 会被同时借两次。
                let can_drag = local_walkable(self.current_volume().map(|v| v.kind()));

                let selected = match which {
                    Pane::Left => &mut self.left_selected,
                    Pane::Right => &mut self.right_selected,
                };

                let row_h = 24.0;
                // 留 16px 给纵向滚动条：不留的话右栏整行会顶出窗口右缘，
                // 大小那一列被切掉半个字。
                let avail_w = (ui.available_width() - 16.0).max(120.0);

                // 闭包内不借用 self（避免与 selected 冲突），事件收集到闭包外处理。
                let mut dbl_pick: Option<(String, bool)> = None;
                let mut row_action: Option<(RowAction, PathBuf, bool)> = None;
                // 拖拽要跨帧：起点在这一帧记下来，落地由 update() 在释放那一帧处理。
                let dragging = self.drag.is_some();
                let pointer = ui.ctx().input(|i| i.pointer.hover_pos());
                let mut drag_began: Option<DragPayload> = None;
                let mut drop_hit: Option<PathBuf> = None;
                // 整栏区域：空白处（行下方）松手算「搬进这一栏正在看的目录」。
                let list_rect = ui.max_rect();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .show_rows(ui, row_h, entries.len(), |ui, row_range| {
                        for i in row_range {
                            let e = entries[i];
                            let badge = badges[i].as_ref();
                            let icon = icon_handles[i].clone();
                            let is_sel = selected.as_deref() == Some(e.name.as_str());
                            let full = base.join(&e.name);
                            let is_dir = e.is_dir;

                            // 命中区 = 整行，不只是文件名。
                            // `ui.horizontal(...).response` 的 Sense 是 hover，
                            // 挂在它上面的 clicked()/double_clicked() 永远为 false。
                            let row_rect = egui::Rect::from_min_size(
                                ui.cursor().min,
                                egui::vec2(avail_w, row_h),
                            );
                            let resp = ui.interact(
                                row_rect,
                                ui.id().with(("row", e.name.as_str())),
                                // click_and_drag：只有 click 的话 drag_started() 永不成立。
                                egui::Sense::click_and_drag(),
                            );
                            // 拖拽悬停判定用指针坐标，不用 resp.hovered()：
                            // 有东西正在被拖时，egui 会把别的 widget 的 hovered 压掉。
                            let is_drop_target = dragging
                                && is_dir
                                && pointer.is_some_and(|pt| row_rect.contains(pt));
                            if is_drop_target {
                                drop_hit = Some(full.clone());
                            }
                            if is_sel || resp.hovered() || is_drop_target {
                                // 底色必须画在文字之前，否则色块把字盖住。
                                let fill = if is_drop_target {
                                    ui.visuals().widgets.active.bg_fill
                                } else if is_sel {
                                    ui.visuals().selection.bg_fill
                                } else {
                                    ui.visuals().faint_bg_color
                                };
                                ui.painter().rect_filled(row_rect, 6.0, fill);
                                if is_drop_target {
                                    // 只换个底色看不出一松手会发生什么，所以补一圈描边。
                                    ui.painter().rect_stroke(
                                        row_rect,
                                        6.0,
                                        egui::Stroke::new(1.0_f32, Color32::from_rgb(0, 122, 255)),
                                        egui::StrokeKind::Middle,
                                    );
                                }
                            }

                            ui.horizontal(|ui| {
                                // 图标：自绘 SVG 纹理；缺失时退回字形占位
                                match icon {
                                    Some(h) => {
                                        ui.add(egui::Image::new((h.id(), egui::vec2(16.0, 16.0))));
                                    }
                                    None => {
                                        ui.label(if e.is_dir { "📁" } else { "📄" });
                                    }
                                }
                                // 角标：左侧圆点，仅在该进程仍存活时出现
                                if let Some(o) = badge {
                                    ui.label(
                                        egui::RichText::new("●")
                                            .size(10.0)
                                            .color(color_for(&o.program)),
                                    );
                                }
                                // 名字紧跟图标左对齐、超长才截断。
                                // `add_sized` 用的是 centered_and_justified 布局，
                                // 会把名字推到行中间；`with_layout(right_to_left)`
                                // 则不设宽度，会把大小列顶到窗口右缘外面去。
                                // 所以两列各自占固定宽，整行 = avail_w。
                                let label = ui
                                    .allocate_ui_with_layout(
                                        egui::vec2((avail_w - 130.0).max(60.0), row_h),
                                        egui::Layout::left_to_right(egui::Align::Center),
                                        |ui| {
                                            // selectable(false)：Label 默认可选文本，
                                            // 它会吃掉这一行的点击——点在名字上选中不了，
                                            // 点在名字右边的空白上却可以。
                                            ui.add(
                                                egui::Label::new(&e.name)
                                                    .selectable(false)
                                                    .truncate(),
                                            )
                                        },
                                    )
                                    .inner;
                                if let Some(o) = badge {
                                    label.on_hover_text(format!("已在 {} 中打开", o.program));
                                }
                                ui.allocate_ui_with_layout(
                                    egui::vec2(110.0, row_h),
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let size =
                                            e.size.map(fmt_size).unwrap_or_else(|| "—".to_string());
                                        ui.label(egui::RichText::new(size).weak().monospace());
                                    },
                                );
                            });

                            let tip = base.join(&e.name).display().to_string();
                            let resp = resp.on_hover_text(tip);
                            if resp.clicked() && !resp.double_clicked() {
                                *selected = Some(e.name.clone());
                            }
                            if resp.drag_started() && can_drag {
                                drag_began = Some(DragPayload {
                                    paths: vec![full.clone()],
                                });
                            }
                            // 右键先选中再弹菜单：否则菜单动的是"上一次点的行"，
                            // 而用户的眼睛停在他刚右键的那一行上。
                            if resp.secondary_clicked() {
                                *selected = Some(e.name.clone());
                            }
                            if resp.double_clicked() {
                                dbl_pick = Some((e.name.clone(), is_dir));
                            }
                            resp.context_menu(|ui| {
                                let mut pick = |ui: &mut egui::Ui, act, label: &str| {
                                    if ui.button(label).clicked() {
                                        row_action = Some((act, full.clone(), is_dir));
                                        ui.close_menu();
                                    }
                                };
                                pick(ui, RowAction::Open, if is_dir { "打开" } else { "预览" });
                                pick(ui, RowAction::OpenExternal, "用默认程序打开");
                                ui.separator();
                                pick(ui, RowAction::Copy, "复制");
                                pick(ui, RowAction::Cut, "剪切");
                                pick(ui, RowAction::Paste, "粘贴到这里");
                                ui.separator();
                                pick(ui, RowAction::Rename, "重命名");
                                pick(ui, RowAction::Delete, "删除");
                                ui.separator();
                                pick(ui, RowAction::CopyPath, "复制完整路径");
                                pick(ui, RowAction::OpenInOtherPane, "在对栏打开");
                                pick(ui, RowAction::PinToBoard, "钉到白板");
                            });
                        }
                    });

                if let Some(d) = drag_began {
                    self.drag = Some(d);
                }
                // 没落在任何文件夹行上、但落点还在这一栏里，且这一栏确实列出了
                // 内容（列失败时 base 可能根本不存在）：目标是这一栏正在看的目录。
                if drop_hit.is_none() && dragging && error.is_none() {
                    if let Some(pt) = pointer {
                        if list_rect.contains(pt) {
                            drop_hit = Some(base.clone());
                            ui.painter().rect_stroke(
                                list_rect.shrink(2.0),
                                6.0,
                                egui::Stroke::new(1.0_f32, Color32::from_rgb(0, 122, 255)),
                                egui::StrokeKind::Middle,
                            );
                        }
                    }
                }
                if drop_hit.is_some() {
                    self.drop_target = drop_hit;
                }

                // 先把要用的结论取出来：`entries` 借的是 self 的列表字段，
                // 而下面的 enter() 会重新列目录（可变借用 self），两者不能重叠。
                let visible_empty = entries.is_empty();

                if let Some((act, path, is_dir)) = row_action {
                    self.run_row_action(act, &path, is_dir, which);
                }

                if let Some((name, is_dir)) = dbl_pick {
                    // 双击哪一栏就操作哪一栏：早先用 active_path() 当基准，
                    // 在非活动栏里双击会把**活动栏**导航过去，或直接开错文件。
                    self.active_pane = which;
                    let target = base.join(&name);
                    self.enter(target, is_dir);
                }

                if let Some(err) = error {
                    let c = ui.visuals().error_fg_color;
                    ui.colored_label(c, format!("读取失败：{err}"));
                    ui.label(
                        egui::RichText::new("路径、卷、权限都可能是原因；可切「卷」或挂载后重试")
                            .weak(),
                    );
                } else if visible_empty {
                    let msg = if filter.is_empty() {
                        "这个目录是空的"
                    } else {
                        "没有匹配这个过滤词的条目"
                    };
                    ui.label(egui::RichText::new(msg).weak());
                }
            });
        });
    }

    fn render_preview(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        use crate::preview::PreviewContent;
        let Some(pv) = self.preview.as_mut() else {
            return;
        };
        match &pv.content {
            PreviewContent::Text { text, truncated } => {
                if *truncated {
                    ui.label(egui::RichText::new("⚠ 已截断（超过 1 MiB）").weak());
                }
                egui::ScrollArea::both().show(ui, |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(text.as_str()).monospace()).wrap());
                });
            }
            PreviewContent::Image(img) => {
                let handle = pv.texture.get_or_insert_with(|| {
                    ctx.load_texture("preview", img.clone(), egui::TextureOptions::LINEAR)
                });
                ui.add(
                    egui::Image::new((handle.id(), handle.size_vec2())).fit_to_exact_size({
                        let s = ui.available_size();
                        let scale = (s.x / handle.size_vec2().x)
                            .min(s.y / handle.size_vec2().y)
                            .min(1.0);
                        handle.size_vec2() * scale
                    }),
                );
            }
            PreviewContent::Binary { size } => {
                ui.label(format!("二进制文件（{size} 字节），无法文本预览"));
                ui.label(egui::RichText::new("可用外部程序打开 →").weak());
            }
            PreviewContent::Failed(msg) => {
                ui.label(egui::RichText::new(msg).color(Color32::from_rgb(220, 50, 50)));
            }
        }
    }
}

/// 一次拖放该怎么处理。
///
/// 拆成纯函数是为了能单测：`handle_dropped_files` 里全是 egui 上下文与
/// 长操作副作用，判定规则混在里面就只能靠手点验证。规则本身跟 UI 无关。
#[derive(Debug, PartialEq, Eq)]
enum DropPlan {
    /// 不动手，只给一句状态栏说明。
    Ignore(String),
    /// 拷这些源到目标目录。
    Copy(Vec<PathBuf>),
}

/// 拖放判定：给定拖入的路径与目标目录，决定拷贝哪些、或为什么不拷。
///
/// 三条拒绝规则都对应**真实的坏输入**，不是防御性编程：
/// - 路径不存在：拖放源可能来自已被删掉/移走的窗口；
/// - 源已在目标目录里：会拷成 `a.txt` 和 `a (1).txt` 之类的重复；
/// - 目标在源的子树内：`copy_tree` 会在自己刚建的目录里继续发现新内容，无限递归。
fn plan_drop(dropped: &[PathBuf], dest: &Path) -> DropPlan {
    let sources: Vec<PathBuf> = dropped.iter().filter(|p| p.exists()).cloned().collect();
    if sources.is_empty() {
        return DropPlan::Ignore("拖入的路径都不可用".to_string());
    }

    // 源已经躺在目标目录里：再拷一遍只会产出重复副本，不如直接说清楚。
    if sources.iter().all(|s| s.parent() == Some(dest)) {
        return DropPlan::Ignore("拖放目标与来源相同，未做任何改动".to_string());
    }

    // 把文件夹拖进它自己（或它的子目录）会无限递归，预检要挡在写入之前。
    if sources
        .iter()
        .any(|s| fmnr_fs::ops::dest_inside_source(s, dest))
    {
        return DropPlan::Ignore("不能把文件夹拖进它自己（或其子目录）里".to_string());
    }

    DropPlan::Copy(sources)
}

/// 把命中片段染成强调色。范围越界（理论上不该发生）时退化为整名普通显示。
fn highlight_hit(hit: &crate::search::Hit, style: &egui::Style) -> egui::WidgetText {
    let range = hit.range.clone();
    if range.start >= range.end || range.end > hit.name.len() {
        return egui::RichText::new(&hit.name).into();
    }
    let (before, rest) = hit.name.split_at(range.end);
    let (matched, after) = rest.split_at(range.end - range.start);

    // append_to 走 style 解析，字体/字号跟随主题，不写死 FontId。
    let mut job = egui::text::LayoutJob::default();
    egui::RichText::new(before).append_to(
        &mut job,
        style,
        egui::FontSelection::default(),
        egui::Align::Center,
    );
    egui::RichText::new(matched)
        .color(Color32::from_rgb(0, 122, 255))
        .strong()
        .append_to(
            &mut job,
            style,
            egui::FontSelection::default(),
            egui::Align::Center,
        );
    egui::RichText::new(after).append_to(
        &mut job,
        style,
        egui::FontSelection::default(),
        egui::Align::Center,
    );
    job.wrap.max_width = f32::INFINITY;
    egui::WidgetText::from(job)
}

/// 一根占比条。返回 `true` = 用户点它要下钻。
///
/// 条长按**占本层的比例**，标签里同时给绝对值——只有比例的话
/// "1% 但 40 GB"和"1% 但 400 MB"看起来一模一样，而那正是要分清的两种情况。
fn bar(ui: &mut egui::Ui, name: &str, bytes: u64, frac: f64, errors: u64, partial: bool) -> bool {
    let row_h = 22.0;
    let w = ui.available_width().max(120.0);
    let rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(w, row_h));
    let resp = ui.interact(
        rect,
        ui.id().with(("usage-bar", name)),
        egui::Sense::click(),
    );
    ui.painter()
        .rect_filled(rect, 5.0, ui.visuals().faint_bg_color);
    let fill_w = (rect.width() * frac.clamp(0.0, 1.0) as f32).max(2.0);
    ui.painter().rect_filled(
        egui::Rect::from_min_size(rect.min, egui::vec2(fill_w, rect.height())),
        5.0,
        if resp.hovered() {
            Color32::from_rgb(0, 102, 204)
        } else {
            Color32::from_rgb(0, 122, 255)
        },
    );
    let mut label = format!("{name}  {}  {:.1}%", fmt_size(bytes), frac * 100.0);
    if errors > 0 {
        // 读不了的目录不能算成 0：那一支的真实大小只会比显示的更大
        label.push_str(&format!("  （有 {errors} 处读不了，实际更大）"));
    }
    if partial {
        label.push_str("  ·还在算");
    }
    // 标签左对齐画在条上。用 `ui.put` 会居中——它内部是
    // `Layout::centered_and_justified`（egui 0.31 ui.rs 里写着），
    // 而占比条的文字居中会盖住"条子有多长"这个信息本身。
    // 只推进游标、不再 interact 一次：同一块矩形上挂两个身份会串 hover。
    ui.advance_cursor_after_rect(rect);
    let color = if frac > 0.35 {
        Color32::WHITE
    } else {
        ui.visuals().text_color()
    };
    let galley = ui
        .painter()
        .layout_no_wrap(label, egui::FontId::proportional(12.0), color);
    ui.painter()
        .galley(rect.min + egui::vec2(6.0, 4.0), galley, color);
    resp.clicked()
}

/// 详情里的一行：没有值就干脆不写这行，不写"（无）"占满屏。
fn field(ui: &mut egui::Ui, label: &str, value: Option<&str>) {
    if let Some(v) = value {
        ui.label(format!("{label}：{v}"));
    }
}

/// 快捷方式检查结论。分三色是因为"底账里没有"和"文件读不了"
/// 对用户的下一步完全不同：前者正常，后者要修。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Loading(String),
    Ok(String),
    Bad(String),
}

/// 一个 `.lnk` → 目标 exe → 底账里的哪条 → 那条在不在可升级清单里。
fn shortcut_verdict(
    path: &Path,
    ledger: &[fmnr_pkg::registry::InstalledApp],
    rows: &[fmnr_pkg::Upgrade],
) -> Verdict {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return Verdict::Bad(format!("读不了这个快捷方式：{e}")),
    };
    let target = match fmnr_pkg::lnk::target_of(&bytes) {
        Ok(t) => t,
        Err(e) => return Verdict::Bad(e.to_string()),
    };
    match fmnr_pkg::registry::match_target(ledger, &target) {
        None if ledger.is_empty() => Verdict::Loading(format!("{target} → 底账还没读好，稍等")),
        None => Verdict::Ok(format!(
            "{target} → 底账里没有这条（绿色版/解包副本就是这样）"
        )),
        Some(a) => {
            // 优先按注册表键名对 winget 的包 ID：那是精确匹配。
            // 名字匹配只在键名对不上时兜底，并说明是"按名字"——
            // 否则用户会以为这个结论和精确匹配一样硬。
            let by_key = a
                .reg_key
                .as_deref()
                .is_some_and(|k| rows.iter().any(|r| r.id.eq_ignore_ascii_case(k)));
            let hit = rows.iter().find(|r| {
                (by_key
                    && a.reg_key
                        .as_deref()
                        .is_some_and(|k| r.id.eq_ignore_ascii_case(k)))
                    || (!by_key && r.name.eq_ignore_ascii_case(&a.name))
            });
            let base = format!(
                "{target} → {} {}",
                a.name,
                a.version.as_deref().unwrap_or("")
            );
            match hit {
                Some(r) => Verdict::Ok(format!(
                    "{base}：可升到 {}（{}）",
                    r.available,
                    if by_key {
                        "按包 ID 对上"
                    } else {
                        "按名字对上"
                    }
                )),
                None => Verdict::Ok(format!("{base}：{}，winget 没报可升", a.family.label())),
            }
        }
    }
}

/// 这个卷的条目能不能用「遍历本地文件系统」的方式搜到。
///
/// 搜索线程走的是 `jwalk`，它只认本地路径。归档卷的根是**包内虚拟路径**：
/// zip 的根甚至是空串，而 `WalkDir::new("")` 会去遍历**进程当前目录**——
/// 于是搜的是完全另一棵树，还理直气壮地报"没有匹配项"。
/// 远程卷同理（本地路径不存在）。所以先挡住，别给假答案。
///
/// WSL 报的是 UNC 根，那是能直接 `read_dir` 的真路径 ⇒ 放行。
fn local_walkable(kind: Option<fmnr_core::VolumeKind>) -> bool {
    matches!(
        kind,
        None | Some(fmnr_core::VolumeKind::Local)
            | Some(fmnr_core::VolumeKind::Unc)
            | Some(fmnr_core::VolumeKind::Wsl)
    )
}

/// 重命名后的完整新名。
///
/// 弹窗里只放 `file_stem`（与主流文件管理器一致），所以"用户有没有改扩展名"
/// 要和**我们发给他那个 stem** 比，而不是和空比：
/// `a.tar.gz` 的 stem 是 `a.tar`（自带一个点），只比"输入里有没有点"会把它
/// 判成"用户自己写了扩展名"，于是 `.gz` 被丢掉——而用户其实只把 `a` 改成 `b`。
/// 改完名变成无扩展名/少一截扩展名，坏的是图标和"双击用什么打开"。
///
/// 拆成纯函数是为了能单测：`commit_rename` 里全是 tokio 与卷副作用。
fn rename_target(old: &str, input: &str) -> String {
    let ext_of = |s: &str| {
        Path::new(s)
            .extension()
            .map(|e| e.to_string_lossy().to_string())
    };
    let stem = Path::new(old)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| old.to_string());
    // 输入的扩展名和原 stem 的扩展名一致 ⇒ 用户没碰扩展名那一段，补回真正的扩展名。
    let untouched_ext = ext_of(input) == ext_of(&stem);
    match (ext_of(old), untouched_ext) {
        (Some(ext), true) => format!("{input}.{ext}"),
        _ => input.to_string(),
    }
}

/// 条目大小的一行显示。目录不给大小（列了也不等于 anything 有用），
/// 用 "—" 占位比留空更不容易被读成"这一列坏了"。
fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 卡片标题截断。按**字符**截而不是字节，否则中文会被切成半个字。
fn truncate_label(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// 卷 id 的显示短名：顶栏和状态栏放不下完整路径。
///
/// `local:C:\Users\lbx13` 这种在顶栏会撑爆布局，显示成 `local:C:`；
/// 归档卷本身较短（`archive:D:\x\a.zip`），保留中段以便区分同名 zip。
fn short_volume_name(id: &str) -> String {
    if let Some(rest) = id.strip_prefix("local:") {
        // 本地卷取盘符/根段；取不到就退回原样（相对路径、UNC 等）。
        let head: String = rest.chars().take(3).collect();
        return format!("local:{}", head);
    }
    truncate_label(id, 40)
}

/// 排序下拉的当前项文案。`SortOrder` 的 Debug 输出是英文枚举名，
/// 直接 `format!("{:?}")` 会让按钮上写着 `Modified` 而菜单里写着"修改时间"。
fn sort_label(order: SortOrder) -> &'static str {
    match order {
        SortOrder::Name => "名称",
        SortOrder::Size => "大小",
        SortOrder::Modified => "修改时间",
        SortOrder::Type => "类型",
    }
}

fn dirs_home() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// 用户图标覆盖目录（`<config>/fmnr/icons`），不存在时 resolver 会静默跳过。
fn overrides_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("fmnr")
        .join("icons")
}

/// 插件目录（`<config>/fmnr/plugins`），放 `*.wasm`。
/// 目录不存在 = 没装插件，是正常状态。
fn plugins_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("fmnr")
        .join("plugins")
}

fn breadcrumbs(path: &Path) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = path;
    while let Some(name) = cur.file_name() {
        parts.push(name.to_string_lossy().to_string());
        cur = cur.parent().unwrap_or(cur);
        if cur == path {
            break;
        }
    }
    parts.reverse();
    if parts.is_empty() {
        parts.push(path.display().to_string());
    }
    parts
}

fn crumbs_to_path(full: &Path, upto: usize) -> PathBuf {
    // 简化：按组件重建到第 upto 层
    let comps: Vec<_> = full.components().collect();
    let mut p = PathBuf::new();
    for c in comps.iter().take(upto + 1) {
        p.push(c.as_os_str());
    }
    p
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn editing_the_stem_keeps_the_extension() {
        // 这条锁住的是"改完名变成无扩展名文件"：图标和默认打开方式都跟着坏。
        assert_eq!(rename_target("report.txt", "summary"), "summary.txt");
    }

    #[test]
    fn typing_an_extension_overrides_the_old_one() {
        assert_eq!(rename_target("notes.txt", "notes.md"), "notes.md");
    }

    #[test]
    fn files_without_extension_are_left_alone() {
        assert_eq!(rename_target("Makefile", "make"), "make");
        assert_eq!(rename_target("LICENSE", "copying"), "copying");
    }

    #[test]
    fn dotfiles_do_not_lose_their_leading_dot() {
        // `.bashrc` 的 extension 是 None、file_stem 是 ".bashrc"，
        // 补扩展名的分支不能把它截成 "bashrc"。
        assert_eq!(rename_target(".bashrc", ".zshrc"), ".zshrc");
    }

    #[test]
    fn multi_part_extension_survives_a_stem_edit() {
        // `a.tar.gz` 发进弹窗的 stem 是 `a.tar`，它自带一个点。
        // 判"用户没改扩展名"要和这个 stem 比，否则只把 a 改成 b 也会把 .gz 吃掉。
        assert_eq!(rename_target("a.tar.gz", "b.tar"), "b.tar.gz");
    }

    #[test]
    fn a_real_extension_change_is_honoured() {
        // 同一条规则的另一侧：输入的扩展名和原 stem 不同，
        // 那是用户真在换格式，不能被自动补名盖掉。
        assert_eq!(rename_target("a.tar.gz", "b.zip"), "b.zip");
    }

    #[test]
    fn archive_and_remote_volumes_are_not_local_walkable() {
        // 归档卷根是包内虚拟路径（zip 根甚至是空串），jwalk 拿空串会去遍历
        // 进程当前目录 ⇒ 搜错树还报"没有匹配项"。这条锁住"不许给假答案"。
        assert!(!local_walkable(Some(fmnr_core::VolumeKind::Archive)));
        assert!(!local_walkable(Some(fmnr_core::VolumeKind::Remote)));
    }

    #[test]
    fn local_unc_and_wsl_roots_are_walkable() {
        // WSL 报的是 UNC 根，那是能直接 read_dir 的真路径 ⇒ 不该被挡。
        assert!(local_walkable(Some(fmnr_core::VolumeKind::Local)));
        assert!(local_walkable(Some(fmnr_core::VolumeKind::Unc)));
        assert!(local_walkable(Some(fmnr_core::VolumeKind::Wsl)));
        assert!(local_walkable(None), "没挂任何卷时不拦，面板自己会说搜不到");
    }

    #[test]
    fn sizes_switch_unit_at_the_boundary() {
        assert_eq!(fmt_size(0), "0 B");
        assert_eq!(fmt_size(1023), "1023 B");
        assert_eq!(fmt_size(1024), "1.0 KB");
        assert_eq!(fmt_size(1536), "1.5 KB");
        assert_eq!(fmt_size(5 * 1024 * 1024), "5.0 MB");
        // GB 封顶：再大也不会出现 TB 之外的空单位。
        assert_eq!(fmt_size(4000 * 1024 * 1024 * 1024), "4000.0 GB");
    }
}

#[cfg(test)]
mod drop_tests {
    use super::*;

    /// 每个用例一个独立目录树，避免并行测试互相看见对方的文件。
    fn scratch(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("fmnr-drop-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn missing_paths_are_ignored_with_a_reason() {
        let root = scratch("missing");
        let ghost = root.join("not-here.txt");
        match plan_drop(&[ghost], &root) {
            DropPlan::Ignore(reason) => assert!(!reason.is_empty(), "拒绝要给出理由"),
            other => panic!("不存在的路径不该进拷贝：{other:?}"),
        }
    }

    #[test]
    fn dropping_into_the_same_directory_changes_nothing() {
        let root = scratch("same");
        let file = root.join("a.txt");
        std::fs::write(&file, b"x").unwrap();

        match plan_drop(&[file], &root) {
            DropPlan::Ignore(reason) => {
                assert!(reason.contains("相同"), "理由应点明同目录：{reason}")
            }
            other => panic!("同目录拖放不该拷贝：{other:?}"),
        }
    }

    #[test]
    fn dropping_a_folder_into_its_own_subdir_is_refused() {
        let root = scratch("nested");
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        // root 拖进 root/sub：copy_tree 会在新建的 sub 里继续发现 root 的内容，无限递归。
        match plan_drop(std::slice::from_ref(&root), &sub) {
            DropPlan::Ignore(reason) => {
                assert!(reason.contains("自己"), "理由应点明自嵌套：{reason}")
            }
            other => panic!("自嵌套必须被挡下：{other:?}"),
        }
    }

    #[test]
    fn dropping_into_a_sibling_directory_copies() {
        let root = scratch("sibling");
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let file = a.join("doc.txt");
        std::fs::write(&file, b"hi").unwrap();

        match plan_drop(std::slice::from_ref(&file), &b) {
            DropPlan::Copy(sources) => assert_eq!(sources, vec![file]),
            other => panic!("正常拖放应当拷贝：{other:?}"),
        }
    }

    /// 混合输入：只保留存在的那部分，而不是整批作废。
    #[test]
    fn only_existing_paths_are_copied() {
        let root = scratch("mixed");
        let dest = root.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let real = root.join("real.txt");
        std::fs::write(&real, b"x").unwrap();
        let ghost = root.join("ghost.txt");

        match plan_drop(&[real.clone(), ghost], &dest) {
            DropPlan::Copy(sources) => assert_eq!(sources, vec![real]),
            other => panic!("应只拷贝存在的那个：{other:?}"),
        }
    }

    #[test]
    fn empty_drop_is_ignored() {
        let root = scratch("empty");
        match plan_drop(&[], &root) {
            DropPlan::Ignore(reason) => assert!(!reason.is_empty()),
            other => panic!("空拖放不该拷贝：{other:?}"),
        }
    }
}

/// 挂载对话框的 spec 拼装：纯逻辑，不碰 egui，可在无显示环境下测。
///
/// 这里钉住的是「用户在对话框里看到的」与「真正挂载的」之间的翻译不走样——
/// UI 层最容易出的 bug 就是把输入 trim/传参错一遍再交给挂载层。
#[cfg(test)]
mod mount_dialog_tests {
    use super::*;

    /// 绕开 `MountDialog::new()` 里的 `discover_wsl_distros()`（会真跑 wsl.exe），
    /// 直接构造确定性状态——单测不依赖本机装没装 WSL。
    fn dlg(
        kind: MountKind,
        input: &str,
        distros: Option<Result<Vec<String>, String>>,
    ) -> MountDialog {
        MountDialog {
            kind,
            input: input.to_string(),
            wsl_distros: distros,
            wsl_picked: 0,
        }
    }

    #[test]
    fn empty_input_yields_no_spec() {
        for kind in [MountKind::Archive, MountKind::Ssh] {
            let d = dlg(kind, "   ", None);
            assert!(d.spec().is_none(), "空输入不该拼出 spec（kind={kind:?}）");
        }
        // WSL 的挂载对象来自列表选中项而非输入框：列表空 + 输入空才该放弃。
        let d = dlg(MountKind::Wsl, "   ", Some(Ok(vec![])));
        assert!(d.spec().is_none(), "列表空且输入空白时不该拼出 spec");
    }

    #[test]
    fn archive_input_is_trimmed() {
        let d = dlg(MountKind::Archive, "  D:/x/y.zip  ", None);
        assert_eq!(
            d.spec(),
            Some(MountSpec::Archive {
                path: PathBuf::from("D:/x/y.zip"),
            }),
            "首尾空白必须 trim：路径粘错一个空格就是 NotFound"
        );
    }

    #[test]
    fn wsl_prefers_picked_distro_over_free_text() {
        let d = dlg(
            MountKind::Wsl,
            "手打的不该被用到",
            Some(Ok(vec!["Ubuntu-24.04".into(), "docker-desktop".into()])),
        );
        assert_eq!(
            d.spec(),
            Some(MountSpec::Wsl {
                distro: "Ubuntu-24.04".to_string(),
            })
        );
        // 选中第二个：优先级跟着选中项走。
        let mut d2 = d;
        d2.wsl_picked = 1;
        assert_eq!(
            d2.spec(),
            Some(MountSpec::Wsl {
                distro: "docker-desktop".to_string(),
            })
        );
    }

    #[test]
    fn wsl_falls_back_to_manual_when_discovery_failed() {
        let d = dlg(
            MountKind::Wsl,
            "Ubuntu-24.04",
            Some(Err("本机没装 WSL".to_string())),
        );
        assert_eq!(
            d.spec(),
            Some(MountSpec::Wsl {
                distro: "Ubuntu-24.04".to_string(),
            }),
            "发现失败后手动输入必须还能挂"
        );
    }

    #[test]
    fn wsl_picked_out_of_range_falls_back_to_text() {
        let mut d = dlg(
            MountKind::Wsl,
            "Manual-Distro",
            Some(Ok(vec!["Ubuntu-24.04".into()])),
        );
        d.wsl_picked = 99; // 列表刷新后下标可能越界，不能 panic
        assert_eq!(
            d.spec(),
            Some(MountSpec::Wsl {
                distro: "Manual-Distro".to_string(),
            })
        );
    }

    #[test]
    fn ssh_input_is_trimmed() {
        let d = dlg(MountKind::Ssh, " me@host:22 ", None);
        assert_eq!(
            d.spec(),
            Some(MountSpec::Ssh {
                endpoint: "me@host:22".to_string(),
            })
        );
    }
}

#[cfg(test)]
mod shortcut_check_tests {
    use super::*;

    /// 造一个结构正确的最小 `.lnk`：76 字节头 + DESCRIPTION 空格 + LOCALBASE。
    /// 字节布局按 MS-SHLLINK 手拼，和 `fmnr-pkg` 那边的解析器各写一遍——
    /// 两边共用一个 builder 的话，builder 错了就两边一起错，测不出东西。
    fn make_lnk(target: &str) -> Vec<u8> {
        let mut d = vec![0u8; 76];
        d[0..4].copy_from_slice(&76u32.to_le_bytes());
        d[4..20].copy_from_slice(&[
            0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x46,
        ]);
        // LinkFlags = HasTargetPrefix(0x40) ⇒ 只有 LOCALBASE 那格有用
        d[20..24].copy_from_slice(&0x40u32.to_le_bytes());
        for s in ["", target] {
            let units: Vec<u8> = s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
            d.extend_from_slice(&(s.encode_utf16().count() as u16).to_le_bytes());
            d.extend_from_slice(&units);
        }
        d
    }

    fn tmp_lnk(bytes: &[u8], tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fmnr-lnk-test-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.lnk");
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn led(name: &str, loc: &str) -> Vec<fmnr_pkg::registry::InstalledApp> {
        let mut v = std::collections::BTreeMap::new();
        v.insert("DisplayName".to_string(), name.to_string());
        v.insert("InstallLocation".to_string(), loc.to_string());
        vec![fmnr_pkg::registry::app_from_values(
            fmnr_pkg::registry::Hive::Machine,
            Some(name.to_string()),
            &v,
        )
        .unwrap()]
    }

    #[test]
    fn a_file_that_is_not_a_shortcut_is_reported_as_bad() {
        let p = tmp_lnk(b"plain text, definitely not a lnk", "bad");
        assert!(
            matches!(shortcut_verdict(&p, &[], &[]), Verdict::Bad(_)),
            "读不懂的文件要说读不懂，不能装作没这个应用"
        );
    }

    /// 底账还没读出来时要说"稍等"，不能报"底账里没有"——
    /// 那是把"还不知道"说成"知道没有"，正是这类面板最容易撒的谎。
    #[test]
    fn empty_ledger_is_loading_not_absent() {
        let p = tmp_lnk(&make_lnk(r"D:\Apps\Foo\foo.exe"), "loading");
        match shortcut_verdict(&p, &[], &[]) {
            Verdict::Loading(s) => assert!(s.contains("底账还没读好"), "{s}"),
            other => panic!("期望 Loading，拿到 {other:?}"),
        }
    }

    #[test]
    fn matched_app_is_named_with_its_family() {
        let p = tmp_lnk(&make_lnk(r"D:\Apps\Foo\foo.exe"), "hit");
        match shortcut_verdict(&p, &led("Foo", r"D:\Apps\Foo"), &[]) {
            Verdict::Ok(s) => {
                assert!(s.contains("Foo"), "{s}");
                assert!(s.contains("winget 没报可升"), "{s}");
            }
            other => panic!("期望 Ok，拿到 {other:?}"),
        }
    }

    /// 底账读好了、里面确实没有 ⇒ 说"没有"，并且不把它算成错误。
    #[test]
    fn loaded_ledger_without_a_match_is_ok_not_bad() {
        let p = tmp_lnk(&make_lnk(r"D:\Portable\bar.exe"), "miss");
        match shortcut_verdict(&p, &led("Foo", r"D:\Apps\Foo"), &[]) {
            Verdict::Ok(s) => assert!(s.contains("底账里没有这条"), "{s}"),
            other => panic!("期望 Ok，拿到 {other:?}"),
        }
    }
}
