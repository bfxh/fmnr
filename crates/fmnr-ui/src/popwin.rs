//! 弹出的独立窗口（docs/UI-REWORK.md §5 最小验证）：同进程多 OS 窗口 + 从主窗口拖入。
//!
//! 为什么状态不放在 `App` 里：`Context::show_viewport_deferred` 的回调签名是
//! `Fn(&Context, ViewportClass) + Send + Sync + 'static`，借不到 `App`。
//! egui 的文档自己写着"state 要包在 `Arc<Mutex<T>>` 里"。这里再收一层：
//! **一个窗口一把锁**，回调画图时只碰自己那把。
//!
//! 两条锁纪律，都是会自锁死的那种，不是洁癖：
//! 1. 调 `show_viewport_deferred` 时**不能持有** `PopWin` 的锁——
//!    `embed_viewports()` 为真时它会当场回调进来，同一把锁再锁一次。
//! 2. 任何时刻不同时持有两把锁（`Hub` 与 `PopWin`）：取快照 → 放开 → 再进另一把。
//!
//! 还有一条 egui 通用的：`ui.input(..)` 闭包里只准读，不准画图。
//! `Context::input` 拿的是 Context 写锁，`painter()`/`style()` 也要这把锁，
//! 写在闭包里不是报错而是整条 UI 线程自锁死（主窗口踩过一次）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use fmnr_core::volreg::VolumeRegistry;
use fmnr_core::{ConflictPolicy, Entry, ListOpts, SortOrder, VolumeId};

use crate::app::{fmt_size, plan_drop, App, DropPlan};

/// 一次跨窗口拖拽的在途负载。
///
/// 归 `Hub` 而不是归发起它的窗口：拖过窗口边界之后，松手那一下只会投递给
/// 光标所在的那个窗口，发起方**收不到 release**。所以规则是"谁收到 release
/// 谁落地"，另一方只看 `Hub::drag` 有没有被取走。
///
/// 只存路径，不存"是复制还是移动"：Ctrl 常常是拖起来之后才按下的，
/// 所以这个标志必须由**看到松手的那一帧**自己采样（跟主窗口同一条口径）。
#[derive(Clone, Debug)]
pub struct Drag {
    pub paths: Vec<PathBuf>,
}

/// 各窗口之间的那点共享：在途拖拽 + 待认领的落地 + "主窗口该重列了"。
#[derive(Default)]
pub struct Hub {
    pub drag: Option<Drag>,
    /// 主窗口松手时指针已经不在它自己窗口内 → 这批东西要交给
    /// "屏幕坐标包含这个点"的那个弹出窗口去落地。见 `Landing`。
    pub landing: Option<Landing>,
    /// 弹出窗口搬完置真。主窗口下一帧重列两栏并清零——不然源目录还挂着
    /// 一排已经搬走的行，看着像没搬成。
    pub dirty: bool,
}

/// 一次"跨窗口落地"的委托。
///
/// 为什么需要它（本机实测出来的，不是设计推演）：Windows 把鼠标**隐式捕获**给
/// 收到按下那一下的窗口，所以指针越过后另一个窗口**收不到 release**，
/// 只有发起方收到。发起方手上的"悬在哪个目录"是它自己窗口里最后一次悬停的结果——
/// 于是跨窗口拖放会把文件搬进那个**早已离屏的旧目标**。
/// 实测：从主窗口拖 `beta.txt` 到弹出窗口，主窗口状态栏报
/// 「移动 beta.txt → E:\Program Files：完成 1 项」，文件真的进了 Program Files。
/// 所以 release 只能由发起方处理，落地目标要按**松手那一刻的屏幕坐标**交给
/// 覆盖该点的那个窗口。
#[derive(Clone, Debug)]
pub struct Landing {
    pub paths: Vec<PathBuf>,
    pub copy: bool,
    /// 松手点的屏幕坐标。
    pub pt: egui::Pos2,
}

/// 攒到锁外再做的事：起线程搬文件。
struct Job {
    sources: Vec<PathBuf>,
    dest: PathBuf,
    copy: bool,
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 一个弹出窗口的全部状态。
pub struct PopWin {
    pub id: egui::ViewportId,
    /// 弹出时主窗口所在的视口。落地后要点名催它重绘：它自己收不到事件的话
    /// 会一直睡，`drag` 与 `dirty` 都清不掉。
    pub parent: egui::ViewportId,
    pub volume_id: VolumeId,
    pub path: PathBuf,
    pub closed: bool,
    entries: Vec<Entry>,
    selected: Option<String>,
    filter: String,
    error: Option<String>,
    status: String,
    show_hidden: bool,
    /// 搬运线程还在跑：不再收第二次拖入，免得两个线程写同一个目录。
    busy: bool,
    /// 本窗口在屏幕上的矩形，每帧由自己回填。主窗口要靠它判断
    /// "松手那一下是不是落在我身上"（它自己收不到跨窗口的 release 归属）。
    pub screen_rect: Option<egui::Rect>,
    /// 已经发给 OS 标题栏的那串字。标题只在建窗口时设过一次，
    /// 导航之后不改的话任务栏和标题栏说的还是弹出来的那个目录——
    /// 而"哪个窗口是哪个目录"正是分窗口之后唯一靠标题辨认的信息。
    shown_title: String,
}

impl PopWin {
    /// 新建状态并**当场列一次目录**：回调可能在主窗口下一次 `update` 之前
    /// 就被单独催起来画图，那时手上只有这份状态，拿不到别的条目来源。
    pub fn new(
        parent: egui::ViewportId,
        volume_id: VolumeId,
        path: PathBuf,
        show_hidden: bool,
        reg: &VolumeRegistry,
    ) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let id = egui::ViewportId::from_hash_of(("fmnr-pop", parent, seq));
        let mut win = Self {
            id,
            parent,
            volume_id,
            path,
            closed: false,
            entries: Vec::new(),
            selected: None,
            filter: String::new(),
            error: None,
            status: String::new(),
            show_hidden,
            busy: false,
            screen_rect: None,
            shown_title: String::new(),
        };
        win.reload(reg);
        win
    }

    pub fn title(&self) -> String {
        format!("fmnr · {}", self.path.display())
    }

    fn opts(&self) -> ListOpts {
        ListOpts {
            show_hidden: self.show_hidden,
            sort: SortOrder::Name,
        }
    }

    /// 重列当前目录。失败原因留着显示，不吞成"这个目录是空的"。
    pub fn reload(&mut self, reg: &VolumeRegistry) {
        let Some(volume) = reg.get(&self.volume_id) else {
            self.entries = Vec::new();
            self.error = Some("这个卷已经卸载了".to_string());
            return;
        };
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                self.entries = Vec::new();
                self.error = Some(format!("起不了列目录用的运行库：{e}"));
                return;
            }
        };
        let (entries, error) = App::list_dir(&rt, &volume, &self.path, &self.opts());
        self.entries = entries;
        self.error = error;
    }

    fn goto(&mut self, to: PathBuf, reg: &VolumeRegistry) {
        self.path = to;
        self.selected = None;
        self.reload(reg);
    }

    /// 画一帧。返回这一帧该落地的搬运，交给调用方在锁外起线程。
    ///
    /// `rename_title` 只在真有独立窗口时为真：嵌入式回退下改标题会把
    /// **主窗口**的名字改掉。
    fn frame(
        &mut self,
        ui: &mut egui::Ui,
        incoming: Option<&Drag>,
        claim: Option<&Landing>,
        reg: &VolumeRegistry,
        rename_title: bool,
    ) -> Option<Job> {
        self.screen_rect = ui.input(|i| i.viewport().inner_rect);
        if rename_title {
            let want = self.title();
            if want != self.shown_title {
                self.shown_title = want.clone();
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Title(want));
            }
        }
        self.header(ui, reg);

        // 指针与修饰键一次读出来。画图必须在闭包外（见模块头第 4 条）。
        let (released, hover, copy) = ui.input(|i| {
            (
                i.pointer.button_released(egui::PointerButton::Primary),
                i.pointer.hover_pos(),
                i.modifiers.command,
            )
        });

        let list = self.list(ui, reg);

        let hot = !self.busy && incoming.is_some() && hover.is_some_and(|p| list.contains(p));
        if hot {
            ui.painter().rect_stroke(
                list.shrink(2.0),
                6.0,
                egui::Stroke::new(1.5_f32, egui::Color32::from_rgb(0, 122, 255)),
                egui::StrokeKind::Middle,
            );
        }
        self.footer(ui);

        // 落地有两条来源：
        //  - `claim`：主窗口按**屏幕坐标**把这批东西委托给本窗口。跨窗口只有
        //    发起方收得到 release（Windows 隐式捕获），所以这条是主路径。
        //  - `hot && released`：本窗口自己收到 release（从本窗口发起的拖拽落到
        //    本窗口，或将来弹出窗口之间互拖）。
        if self.busy {
            return None;
        }
        let landed = match claim {
            Some(l) => Some((l.paths.clone(), l.copy)),
            None if hot => released
                .then(|| incoming.map(|d| (d.paths.clone(), copy)))
                .flatten(),
            None => None,
        };
        let (paths, copy) = landed?;
        let dest = self.path.clone();
        // 规则跟主窗口同一条：源不可用、原地复制、文件夹拖进自己子树，
        // 都由同一个 `plan_drop` 挡在写入之前。
        match plan_drop(&paths, &dest) {
            DropPlan::Ignore(why) => {
                self.status = why;
                None
            }
            DropPlan::Copy(sources) => {
                self.busy = true;
                self.status = "搬运中…".to_string();
                Some(Job {
                    sources,
                    dest,
                    copy,
                })
            }
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, reg: &VolumeRegistry) {
        ui.horizontal(|ui| {
            let up = self.path.parent().map(PathBuf::from);
            if ui
                .add_enabled(up.is_some(), egui::Button::new("上一级"))
                .clicked()
            {
                if let Some(p) = up {
                    self.goto(p, reg);
                }
            }
            if ui.small_button("重列").clicked() {
                self.reload(reg);
            }
            ui.label(egui::RichText::new(self.path.display().to_string()).strong());
        });
        ui.horizontal(|ui| {
            ui.label("🔍");
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("按名称过滤")
                    .desired_width((ui.available_width() - 30.0).max(60.0)),
            );
            if ui.checkbox(&mut self.show_hidden, "隐藏项").changed() {
                self.reload(reg);
            }
        });
        ui.separator();
    }

    /// 画列表，返回它的矩形——判定"松手是不是落在这一块里"要用。
    fn list(&mut self, ui: &mut egui::Ui, reg: &VolumeRegistry) -> egui::Rect {
        let needle = self.filter.to_lowercase();
        let rows: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| needle.is_empty() || e.name.to_lowercase().contains(&needle))
            .collect();

        let mut descend: Option<PathBuf> = None;
        let mut select: Option<Option<String>> = None;

        let resp = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if let Some(err) = &self.error {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 60, 60),
                        format!("列不出来：{err}"),
                    );
                }
                if rows.is_empty() && self.error.is_none() {
                    ui.weak("这里没有文件");
                }
                for (i, e) in rows.iter().enumerate() {
                    let selected = self.selected.as_deref() == Some(e.name.as_str());
                    let is_dir = e.is_dir;
                    let name = e.name.clone();
                    let row = ui.horizontal(|ui| {
                        ui.label(if selected { "▸" } else { " " });
                        ui.label(egui::RichText::new(&name));
                        if !is_dir {
                            if let Some(sz) = e.size {
                                ui.weak(fmt_size(sz));
                            }
                        }
                    });
                    // 整行的点击要自己盖一层 `interact`：`Label` 默认的 Sense 只有
                    // hover，挂在它上面的 `clicked()`/`double_clicked()` 恒 false。
                    // 主窗口踩过同一个坑，这里按旧笔记的"解法"又踩了一次
                    // （`selectable(false)` 不解决这件事，它只是不让文字被选中）。
                    // `interact` 不推进光标，覆盖已画出来的区域是安全的。
                    let hit = ui.interact(
                        row.response.rect,
                        egui::Id::new(("poprow", i, name.as_str())),
                        egui::Sense::click(),
                    );
                    if hit.clicked() {
                        select = Some(Some(name.clone()));
                    }
                    if hit.double_clicked() && is_dir {
                        let mut p = self.path.clone();
                        p.push(&name);
                        descend = Some(p);
                    }
                }
            });

        if let Some(name) = select {
            self.selected = name;
        }
        if let Some(to) = descend {
            self.goto(to, reg);
        }
        resp.inner_rect
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        if self.status.is_empty() {
            return;
        }
        ui.separator();
        ui.label(self.status.clone());
    }
}

/// 每帧调用，把弹出窗口"显示"出来。
///
/// `show_viewport_deferred` 要求**每一帧都调**，否则那个子视口会被拆掉。
/// 调用方此刻不能持有 `win` 的锁（见模块头的纪律 1）。
pub fn show_every_frame(
    ctx: &egui::Context,
    win: &Arc<Mutex<PopWin>>,
    hub: &Arc<Mutex<Hub>>,
    reg: &Arc<VolumeRegistry>,
) {
    let (id, title) = {
        let g = win.lock().unwrap();
        (g.id, g.title())
    };
    let builder = egui::ViewportBuilder::default()
        .with_inner_size([620.0, 460.0])
        .with_title(title);
    let (w, h, r) = (win.clone(), hub.clone(), reg.clone());
    ctx.show_viewport_deferred(id, builder, move |c, class| {
        render(&w, &h, &r, c, class);
    });
}

fn render(
    win: &Arc<Mutex<PopWin>>,
    hub: &Arc<Mutex<Hub>>,
    reg: &Arc<VolumeRegistry>,
    ctx: &egui::Context,
    class: egui::ViewportClass,
) {
    if class != egui::ViewportClass::Embedded && ctx.input(|i| i.viewport().close_requested()) {
        let parent = {
            let mut g = lock(win);
            g.closed = true;
            g.parent
        };
        // 摘掉这个窗口是主窗口的活，催它跑一帧。
        ctx.request_repaint_of(parent);
        return;
    }

    // 本窗口在屏幕上的矩形。认领委托要靠它跟"松手点的屏幕坐标"比。
    // 读它必须在进锁之前：`ctx.input` 也要 Context 的锁。
    let own = ctx.input(|i| i.viewport().inner_rect);
    let rename = class != egui::ViewportClass::Embedded;
    let draw = |ui: &mut egui::Ui| -> Option<Job> {
        // 一次锁内取齐"在途拖拽"和"给本窗口的委托"，然后放开——
        // 任何时刻不同时持两把锁。
        let (snap, claim) = {
            let mut h = hub.lock().unwrap();
            let claim = h
                .landing
                .as_ref()
                .filter(|l| own.is_some_and(|r| r.contains(l.pt)))
                .cloned();
            if claim.is_some() {
                h.landing = None;
            }
            (h.drag.clone(), claim)
        };
        let job = lock(win).frame(ui, snap.as_ref(), claim.as_ref(), reg, rename);
        if job.is_some() {
            // 当场认领，不等搬运跑完：从这一刻起这批路径归这个窗口，
            // 主窗口下一帧看到 `drag` 没了就知道该结束自己的拖拽态。
            hub.lock().unwrap().drag = None;
        }
        job
    };

    let job = match class {
        // 后端不支持多窗口时内容不能凭空消失：就地开一个浮动 Window。
        egui::ViewportClass::Embedded => {
            let title = lock(win).title();
            egui::Window::new(title)
                .default_pos([90.0, 130.0])
                .show(ctx, draw)
                .and_then(|ir| ir.inner)
                .flatten()
        }
        _ => egui::CentralPanel::default().show(ctx, draw).inner,
    };

    if let Some(job) = job {
        start_transfer(job, win.clone(), hub.clone(), reg.clone(), ctx.clone());
    }
}

fn lock(win: &Arc<Mutex<PopWin>>) -> MutexGuard<'_, PopWin> {
    win.lock().unwrap()
}

/// 落地：起线程搬，搬完回填状态并通知主窗口。
///
/// 不在 UI 线程里搬——`transfer` 是同步的，大目录会把整个进程的事件循环卡住
/// （弹出窗口和主窗口同一条线程，一起停帧）。
fn start_transfer(
    job: Job,
    win: Arc<Mutex<PopWin>>,
    hub: Arc<Mutex<Hub>>,
    reg: Arc<VolumeRegistry>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let progress = fmnr_core::Progress::new();
        let res = fmnr_fs::ops::transfer(
            &job.sources,
            &job.dest,
            ConflictPolicy::Skip,
            &progress,
            !job.copy,
        );
        let verb = if job.copy { "复制" } else { "移动" };
        let parent = {
            let mut g = lock(&win);
            g.busy = false;
            g.status = match res {
                Ok(rep) if rep.skipped > 0 => format!(
                    "{verb} {} 项，跳过 {} 项同名 → {}",
                    rep.done,
                    rep.skipped,
                    g.path.display()
                ),
                Ok(rep) => format!("{verb} {} 项 → {}", rep.done, g.path.display()),
                Err(e) => format!("搬运失败：{e}"),
            };
            g.reload(&reg);
            g.parent
        };
        {
            // 只通知主窗口重列：`drag` 在认领那一刻就已经清掉了。
            hub.lock().unwrap().dirty = true;
        }
        ctx.request_repaint_of(parent);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> VolumeRegistry {
        VolumeRegistry::default()
    }

    /// 空注册表里查不到卷：要报"这个卷已经卸载了"，不能显示成空目录。
    #[test]
    fn missing_volume_reports_instead_of_looking_empty() {
        let win = PopWin::new(
            egui::ViewportId::ROOT,
            fmnr_core::VolumeId("local:gone".into()),
            PathBuf::from("C:\\"),
            false,
            &reg(),
        );
        assert_eq!(win.error.as_deref(), Some("这个卷已经卸载了"));
        assert!(win.entries.is_empty());
    }

    /// 同一批路径弹两次要拿到两个不同的视口 id，否则第二个窗口会顶掉第一个。
    #[test]
    fn two_pops_of_the_same_path_get_different_ids() {
        let p = PathBuf::from("C:\\tmp");
        let a = PopWin::new(
            egui::ViewportId::ROOT,
            fmnr_core::VolumeId("v".into()),
            p.clone(),
            false,
            &reg(),
        );
        let b = PopWin::new(
            egui::ViewportId::ROOT,
            fmnr_core::VolumeId("v".into()),
            p,
            false,
            &reg(),
        );
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn title_carries_the_path() {
        let win = PopWin::new(
            egui::ViewportId::ROOT,
            fmnr_core::VolumeId("v".into()),
            PathBuf::from("C:\\Users\\me"),
            false,
            &reg(),
        );
        assert_eq!(win.title(), "fmnr · C:\\Users\\me");
    }
}
