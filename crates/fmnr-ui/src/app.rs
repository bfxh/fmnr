use eframe::egui::{self, Color32, Key};
use fmnr_core::{ListOpts, SortOrder, VPath, Volume};
use fmnr_fs::LocalVolume;
use std::path::PathBuf;
use std::sync::Arc;

fn color_for(exp: Option<&fmnr_core::ExternalOpen>) -> Color32 {
    if let Some(o) = exp {
        if o.is_expired() {
            return Color32::from_rgba_unmultiplied(255, 0, 0, 90);
        }
        // 已摄入 UI：用 program 前缀做主题色
        match o.program.to_lowercase().as_str() {
            p if p.contains("chrome") | p.contains("edge") | p.contains("firefox") | p.contains("brave") => {
                Color32::from_rgb(0, 122, 255)
            }
            p if p.contains("code") | p.contains("vscode") | p.contains("cursor") => {
                Color32::from_rgb(230, 191, 0)
            }
            p if p.contains("notepad") | p.contains("word") | p.contains("excel") | p.contains("powerpoint") => {
                Color32::from_rgb(230, 89, 38)
            }
            _ => Color32::from_rgb(0, 128, 255),
        }
    } else {
        Color32::TRANSPARENT
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
}

#[derive(PartialEq, Clone, Copy)]
enum Pane {
    Left,
    Right,
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
        };
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

    fn spawn_external(&mut self, path: &PathBuf) {
        let program = default_program_for(&path);
        let program_for_status = program.clone();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let volume = self.volume.clone();
        let path = path.clone();
        let result = rt.block_on(async move {
            volume
                .spawn_external_open(&fmnr_core::VPath(path.clone().into()), &program)
                .await
        });
        match result {
            Ok(open) => {
                let pid = open.handle.as_deref().unwrap_or("?");
                self.active_pane = Pane::Left;
                self.status = format!("已在 {} 中打开（pid {}）", program_for_status, pid);
                // 给 UI 挂状态；实际角标以 volume.external_opens() 驱动
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
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_keys(ctx);

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("fmnr");
                ui.separator();
                if ui.button("⬅ 上级").clicked() {
                    self.go_up();
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
            ui.label(&self.status);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let avail = ui.available_size();
            let half = avail.x / 2.0;
            ui.horizontal_top(|ui| {
                self.pane(ui, Pane::Left, half);
                self.pane(ui, Pane::Right, half);
            });
        });
    }
}

impl App {
    fn handle_keys(&mut self, ctx: &egui::Context) {
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
    }

    fn pane(&mut self, ui: &mut egui::Ui, which: Pane, width: f32) {
        // 每帧清理一次（≤60次/秒），在 UI 线程里做，不阻塞 IO 线程池。
        self.volume.clear_expired();

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
                ui.add(egui::TextEdit::singleline(filter).hint_text("过滤…").desired_width(width - 60.0));
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
                        .get(&(base.join(&e.name).display().to_string()))
                })
                .collect();

            let selected = match which {
                Pane::Left => &mut self.left_selected,
                Pane::Right => &mut self.right_selected,
            };

            // 闭包内不借用 self（避免与 selected 冲突），事件收集到闭包外处理。
            let mut clicked: Option<usize> = None;
            let mut dbl: Option<usize> = None;
            egui::ScrollArea::vertical().show_rows(
                ui,
                24.0,
                entries.len(),
                |ui, row_range| {
                    for i in row_range {
                        let e = entries[i];
                        let badge = badges[i].as_ref();
                        let label = if e.is_dir {
                            format!("📁 {}", e.name)
                        } else {
                            format!("📄 {}", e.name)
                        };
                        ui.horizontal(|ui| {
                            // 角标：左侧圆点，指示外部打开状态
                            if let Some(o) = badge {
                                let dot = egui::RichText::new("●").size(10.0).color(if o.is_expired() {
                                    // 失活：红色，半透明
                                    Color32::from_rgba_unmultiplied(255, 0, 0, 102)
                                } else {
                                    // 活跃：按程序命色
                                    color_for(badge)
                                });
                                ui.label(dot);
                            }
                            let resp = ui.selectable_label(*selected == Some(i), label);
                            if let Some(o) = badge {
                                resp.clone().on_hover_text(format!("已在 {} 中打开", o.program));
                            }
                            if resp.clicked() {
                                clicked = Some(i);
                            }
                            if resp.double_clicked() {
                                dbl = Some(i);
                            }
                        });
                    }
                },
            );

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
}

fn dirs_home() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn breadcrumbs(path: &PathBuf) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = path.as_path();
    while let Some(name) = cur.file_name() {
        parts.push(name.to_string_lossy().to_string());
        cur = cur.parent().unwrap_or(cur);
        if cur == path.as_path() {
            break;
        }
    }
    parts.reverse();
    if parts.is_empty() {
        parts.push(path.display().to_string());
    }
    parts
}

fn crumbs_to_path(full: &PathBuf, upto: usize) -> PathBuf {
    // 简化：按组件重建到第 upto 层
    let comps: Vec<_> = full.components().collect();
    let mut p = PathBuf::new();
    for c in comps.iter().take(upto + 1) {
        p.push(c.as_os_str());
    }
    p
}

/// 默认外部程序：未知类型交给系统默认（以扩展名匹配），已知文本类型先走内置预览。
/// 暂时退回「全部交给系统默认」，M3 统一预览面板上线后按扩展名分流。
fn default_program_for(_path: &PathBuf) -> String {
    std::env::var("COMSPEC")
        .or_else(|_| std::env::var("SHELL"))
        .unwrap_or_else(|_| "cmd.exe".into())
}
