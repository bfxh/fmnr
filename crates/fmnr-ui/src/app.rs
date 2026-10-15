use eframe::egui::{self, Color32, Key, Modifiers};
use fmnr_core::open::{route_open as core_route_open, OpenRoute};
use fmnr_core::{ListOpts, SortOrder, VPath, Volume};
use fmnr_fs::LocalVolume;
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
    volume: Arc<dyn Volume>,
    /// 左栏当前目录
    left_path: PathBuf,
    /// 右栏当前目录
    right_path: PathBuf,
    left_entries: Vec<fmnr_core::Entry>,
    right_entries: Vec<fmnr_core::Entry>,
    left_selected: Option<usize>,
    right_selected: Option<usize>,
    active_pane: Pane,
    show_hidden: bool,
    sort: SortOrder,
    /// 面包屑行内过滤
    left_filter: String,
    right_filter: String,
    /// 状态栏消息
    status: String,
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
    /// M2 子树搜索（None = 搜索框未激活）
    search: Option<crate::search::SearchState>,
    /// 搜索框文本
    search_text: String,
    /// M5 插件宿主：按扩展名路由预览渲染器
    plugins: fmnr_plugins::registry::PluginSet,
    /// 插件加载警告（坏插件路径 + 原因），启动时填一次，状态栏可见
    plugin_warnings: Vec<fmnr_plugins::registry::LoadWarning>,
    /// M6 空间白板（布局层，与双栏视图互斥显示）
    board: fmnr_core::board::Board,
    /// 当前视图：双栏 or 白板
    view: ViewMode,
    /// 白板相机（平移 + 缩放）。坐标变换全在 `board_view::Camera` 里。
    board_cam: crate::board_view::Camera,
    /// 白板上正在拖拽的节点下标
    board_drag: Option<usize>,
    /// 上一帧的画布区域。用于把「视图中心」换算成画布坐标（钉节点、适应全部）。
    board_viewport: egui::Rect,
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum ViewMode {
    Panes,
    Board,
}

#[derive(PartialEq, Clone, Copy)]
enum Pane {
    Left,
    Right,
}

/// 冲突弹窗：已把请求暂存，等用户选策略。
struct ConflictPrompt {
    kind: crate::ops::OpKind,
    sources: Vec<VPath>,
    dest_dir: Option<VPath>,
    conflicting: Vec<String>,
    choice: fmnr_core::ConflictPolicy,
}

impl App {
    pub fn new() -> Self {
        let home = dirs_home();
        let volume = Arc::new(LocalVolume::new(home.clone()));
        let mut app = Self {
            volume,
            left_path: home.clone(),
            right_path: home,
            left_entries: Vec::new(),
            right_entries: Vec::new(),
            left_selected: None,
            right_selected: None,
            active_pane: Pane::Left,
            show_hidden: false,
            sort: SortOrder::Name,
            left_filter: String::new(),
            right_filter: String::new(),
            status: String::new(),
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
            search_text: String::new(),
            plugins: fmnr_plugins::registry::PluginSet::new(),
            plugin_warnings: Vec::new(),
            board: fmnr_core::board::Board::new("default"),
            view: ViewMode::Panes,
            board_cam: crate::board_view::Camera::default(),
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
        app
    }

    fn opts(&self) -> ListOpts {
        ListOpts {
            show_hidden: self.show_hidden,
            sort: self.sort,
        }
    }

    fn refresh(&mut self) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let opts = self.opts();
        let l = rt.block_on(self.volume.list(&VPath(self.left_path.clone()), &opts));
        let r = rt.block_on(self.volume.list(&VPath(self.right_path.clone()), &opts));
        self.left_entries = l.unwrap_or_default();
        self.right_entries = r.unwrap_or_default();
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

    fn enter(&mut self, path: PathBuf) {
        if path.is_dir() {
            match self.active_pane {
                Pane::Left => self.left_path = path,
                Pane::Right => self.right_path = path,
            }
            self.left_filter.clear();
            self.right_filter.clear();
            self.refresh();
        } else {
            // 非目录：交给外部程序，并在 UI 挂状态角标
            self.spawn_external(&path);
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
        let volume = self.volume.clone();
        let vpath = VPath(path.to_path_buf());
        let result = rt.block_on(async move { volume.spawn_external_open(&vpath, &program).await });
        match result {
            Ok(open) => {
                // 登记进角标表：看门狗线程会在该进程退出时自动摘掉。
                let key = path.display().to_string();
                self.volume.external_opens().insert(key, open);
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
            let p = parent.to_path_buf();
            match self.active_pane {
                Pane::Left => self.left_path = p,
                Pane::Right => self.right_path = p,
            }
            self.refresh();
        }
    }

    /// M3 统一预览：读文件字节 → classify → 挂到 self.preview。
    fn open_preview(&mut self, path: &Path) {
        let cap = crate::preview::cap_for(path);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let volume = self.volume.clone();
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
    fn selected_names(&self) -> Vec<String> {
        let (entries, sel) = match self.active_pane {
            Pane::Left => (&self.left_entries, self.left_selected),
            Pane::Right => (&self.right_entries, self.right_selected),
        };
        sel.and_then(|i| entries.get(i))
            .map(|e| vec![e.name.clone()])
            .unwrap_or_default()
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

        // 目标 = 非活动栏的当前目录；单栏时就是活动栏自己。
        let dest_pane = match self.active_pane {
            Pane::Left => Pane::Right,
            Pane::Right => Pane::Left,
        };
        let dest = match dest_pane {
            Pane::Left => self.left_path.clone(),
            Pane::Right => self.right_path.clone(),
        };
        // 同栏粘贴到自身目录等于原地复制，无意义 —— 直接拒绝。
        if dest == *self.active_path() {
            self.status = "源与目标同目录，无需粘贴".into();
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
            Some(VPath(dest)),
            fmnr_core::ConflictPolicy::Skip,
        );
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
        match crate::ops::spawn_op(self.volume.clone(), req) {
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
        self.renaming = Some(stem);
    }

    fn commit_rename(&mut self) {
        let Some(input) = self.renaming.take() else {
            return;
        };
        let name = input.trim();
        let Some(old) = self.selected_names().first().cloned() else {
            return;
        };
        if name.is_empty() {
            return;
        }
        let base = self.active_path().clone();
        let from = VPath(base.join(&old));
        let to = VPath(base.join(name));
        if from == to {
            return;
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        match rt.block_on(self.volume.rename(&from, &to)) {
            Ok(()) => {
                self.status = format!("已重命名为 {name}");
                self.refresh();
            }
            Err(e) => self.status = format!("重命名失败：{e}"),
        }
    }

    fn begin_new_dir(&mut self) {
        self.new_dir_name = Some("新建文件夹".into());
    }

    fn commit_new_dir(&mut self) {
        let Some(input) = self.new_dir_name.take() else {
            return;
        };
        let name = input.trim();
        if name.is_empty() {
            return;
        }
        let target = VPath(self.active_path().join(name));
        let rt = tokio::runtime::Runtime::new().unwrap();
        match rt.block_on(self.volume.create_dir(&target)) {
            Ok(()) => {
                self.status = format!("已新建 {name}");
                self.refresh();
            }
            Err(e) => self.status = format!("新建失败：{e}"),
        }
    }

    // ---- M2 即时搜索 ----

    fn start_search(&mut self) {
        let text = self.search_text.trim().to_string();
        if text.is_empty() {
            self.search = None;
            return;
        }
        self.search = Some(crate::search::spawn_search(crate::search::Query {
            text,
            root: self.active_path().clone(),
            case_sensitive: false,
        }));
    }

    fn stop_search(&mut self) {
        self.search = None;
        self.search_text.clear();
    }

    /// 每帧拉一次搜索增量。
    fn poll_search(&mut self) {
        if let Some(s) = self.search.as_mut() {
            s.drain();
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_keys(ctx);
        self.handle_dropped_files(ctx);
        self.poll_running();
        self.poll_search();

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("fmnr");
                ui.separator();
                if ui.button("⬅ 上级").clicked() {
                    self.go_up();
                }
                ui.separator();
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
                ui.separator();
                if ui.button("🔎 搜索").clicked() {
                    self.start_search();
                }
                // 视图切换：双栏 ↔ 白板
                if ui
                    .selectable_label(self.view == ViewMode::Board, "🗺 白板")
                    .clicked()
                {
                    self.view = if self.view == ViewMode::Board {
                        ViewMode::Panes
                    } else {
                        ViewMode::Board
                    };
                }
                if ui.button("📌 钉到白板").clicked() {
                    self.pin_selected_to_board();
                }
                // 只在白板视图里有意义：平移到看不见东西后靠它找回来。
                if self.view == ViewMode::Board && ui.button("🔍 适应全部").clicked() {
                    self.fit_board();
                }
                ui.checkbox(&mut self.show_hidden, "隐藏项");
                egui::ComboBox::from_label("排序")
                    .selected_text(format!("{:?}", self.sort))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.sort, SortOrder::Name, "名称");
                        ui.selectable_value(&mut self.sort, SortOrder::Size, "大小");
                        ui.selectable_value(&mut self.sort, SortOrder::Modified, "修改时间");
                        ui.selectable_value(&mut self.sort, SortOrder::Type, "类型");
                    });
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

        // 搜索框：激活时取代中央区
        if self.search.is_some() {
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
            if !open {
                self.preview = None;
            }
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
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
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        submit = true;
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
            } else {
                self.renaming = Some(text);
            }
        }

        if let Some(mut text) = self.new_dir_name.take() {
            let mut submit = false;
            let mut cancel = false;
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
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        submit = true;
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
            } else {
                self.new_dir_name = Some(text);
            }
        }
    }

    /// 搜索面板：输入框 + 增量结果。
    fn render_search_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("searchbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("🔎");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.search_text)
                        .hint_text("在当前子树搜索…")
                        .desired_width(300.0),
                );
                // 逐字符触发增量搜索：不等回车
                if resp.changed() {
                    self.start_search();
                }
                if ui.button("关闭").clicked() {
                    self.stop_search();
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            // 摘出渲染所需字段：双击要调 self.enter，闭包不能借用 self
            let Some(hits) = self.search.as_ref().map(|s| s.hits.clone()) else {
                return;
            };
            let (n, total, done, ms) = match self.search.as_ref() {
                Some(s) => (s.hits.len(), s.total, s.done, s.elapsed_ms),
                None => return,
            };
            ui.horizontal(|ui| {
                if done {
                    ui.label(format!("{total} 个结果 · {ms} ms"));
                } else {
                    ui.label(format!("搜索中… 已找到 {n} 个"));
                }
            });
            ui.separator();

            let mut jump: Option<(PathBuf, bool)> = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for hit in &hits {
                    // 命中片段高亮：命中段用强调色，其余普通
                    let label = highlight_hit(hit, ui.style());
                    let icon = if hit.is_dir { "📁" } else { "📄" };
                    let size = if hit.is_dir {
                        egui::RichText::new("")
                    } else {
                        egui::RichText::new(format!("{} B", hit.size)).weak()
                    };
                    let resp = ui
                        .horizontal(|ui| {
                            ui.label(icon);
                            ui.label(label);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(size);
                                },
                            );
                        })
                        .response;
                    if resp.double_clicked() {
                        jump = Some((hit.path.clone(), hit.is_dir));
                        break;
                    }
                }
            });
            if let Some((path, is_dir)) = jump {
                if is_dir {
                    self.enter(path);
                } else {
                    self.spawn_external(&path);
                }
                self.stop_search();
            }
        });
    }
}

impl App {
    fn handle_keys(&mut self, ctx: &egui::Context) {
        // 弹窗打开时，键盘全归弹窗（否则 Backspace 会同时删输入和跳目录）
        if self.conflict.is_some() || self.renaming.is_some() || self.new_dir_name.is_some() {
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.conflict = None;
                self.renaming = None;
                self.new_dir_name = None;
            }
            return;
        }

        // Ctrl+F 开搜索；搜索中任意按键喂给搜索框
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::F)) {
            self.status = "输入搜索词（Esc 退出）".into();
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
                let sel = match self.active_pane {
                    Pane::Left => self.left_selected,
                    Pane::Right => self.right_selected,
                };
                let entries = match self.active_pane {
                    Pane::Left => &self.left_entries,
                    Pane::Right => &self.right_entries,
                };
                if let Some(idx) = sel {
                    if let Some(e) = entries.get(idx) {
                        let base = self.active_path().clone();
                        self.enter(base.join(&e.name));
                    }
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
                let p = node.path.clone();
                self.enter(p);
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

    fn pane(&mut self, ui: &mut egui::Ui, which: Pane, width: f32) {
        let path = match which {
            Pane::Left => self.left_path.clone(),
            Pane::Right => self.right_path.clone(),
        };
        let is_active = self.active_pane == which;
        let _ = is_active;

        ui.vertical(|ui| {
            ui.set_width(width);
            // 面包屑 + 行内过滤
            ui.horizontal(|ui| {
                let crumbs = breadcrumbs(&path);
                for (i, c) in crumbs.iter().enumerate() {
                    if ui.button(c).clicked() {
                        let target = crumbs_to_path(&path, i);
                        match which {
                            Pane::Left => self.left_path = target,
                            Pane::Right => self.right_path = target,
                        }
                        self.refresh();
                    }
                    if i + 1 < crumbs.len() {
                        ui.label("›");
                    }
                }
            });
            ui.horizontal(|ui| {
                let filter = match which {
                    Pane::Left => &mut self.left_filter,
                    Pane::Right => &mut self.right_filter,
                };
                ui.label("🔍");
                ui.add(
                    egui::TextEdit::singleline(filter)
                        .hint_text("过滤…")
                        .desired_width(width - 60.0),
                );
                if ui.button("✕").clicked() {
                    filter.clear();
                }
            });
            ui.separator();

            // 列视图
            let base = match which {
                Pane::Left => self.left_path.clone(),
                Pane::Right => self.right_path.clone(),
            };
            let entries: Vec<&fmnr_core::Entry> = {
                let all = match which {
                    Pane::Left => &self.left_entries,
                    Pane::Right => &self.right_entries,
                };
                let filter = match which {
                    Pane::Left => &self.left_filter,
                    Pane::Right => &self.right_filter,
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

            // 文件路径 -> 外置打开状态的索引（角标注入用）
            let badges: Vec<Option<fmnr_core::ExternalOpen>> = entries
                .iter()
                .map(|e| {
                    self.volume
                        .external_opens()
                        .get(base.join(&e.name).display().to_string())
                })
                .collect();

            // 图标纹理预先解析（闭包内不能可变借用 self.icons）。
            let ctx = ui.ctx().clone();
            let icon_handles: Vec<Option<egui::TextureHandle>> = entries
                .iter()
                .map(|e| self.icons.get(&ctx, &e.icon_key))
                .collect();

            let selected = match which {
                Pane::Left => &mut self.left_selected,
                Pane::Right => &mut self.right_selected,
            };

            // 闭包内不借用 self（避免与 selected 冲突），事件收集到闭包外处理。
            let mut clicked: Option<usize> = None;
            let mut dbl: Option<usize> = None;
            egui::ScrollArea::vertical().show_rows(ui, 24.0, entries.len(), |ui, row_range| {
                for i in row_range {
                    let e = entries[i];
                    let badge = badges[i].as_ref();
                    let icon = icon_handles[i].clone();
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
                        let resp = ui.selectable_label(*selected == Some(i), &e.name);
                        if let Some(o) = badge {
                            resp.clone()
                                .on_hover_text(format!("已在 {} 中打开", o.program));
                        }
                        if resp.clicked() {
                            clicked = Some(i);
                        }
                        if resp.double_clicked() {
                            dbl = Some(i);
                        }
                    });
                }
            });

            if let Some(i) = clicked {
                match which {
                    Pane::Left => self.left_selected = Some(i),
                    Pane::Right => self.right_selected = Some(i),
                }
                self.active_pane = which;
            }
            if let Some(i) = dbl {
                if let Some(e) = entries.get(i) {
                    let base = self.active_path().clone();
                    self.enter(base.join(&e.name));
                }
            }
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

/// 卡片标题截断。按**字符**截而不是字节，否则中文会被切成半个字。
fn truncate_label(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
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
