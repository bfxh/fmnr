//! 卷注册表：把多个 `Volume` 聚合成一个可列出/可切换的单元。
//!
//! 当前 `LocalVolume` 只代表**一个**盘；本模块将它提升为「仓」（registry），
//! 让 UI、CLI、MCP 共享同一套「卷列表」与「选卷」语义。
//!
//! M8 目标：UI/CLI 真正能打开归档、切换 WSL、连 SSH——这些卷都要
//! 先注册进本仓，才能被 UI 拉出来选择。

use crate::{Volume, VolumeId, VolumeKind};
use std::path::Path;
use std::sync::Arc;

pub struct VolumeInfo {
    /// 卷的唯一 id（数据源是 `Volume::id()`），CLI/MCP 用它寻址。
    pub id: VolumeId,
    /// 对外展示的短名，优先用 ID 的人类可读段（例如 local:C、archive:/a.zip）。
    pub name: String,
    pub kind: VolumeKind,
    pub writable: bool,
    pub note: String,
}

/// 空注册表：功能最小但完整。
#[derive(Default)]
pub struct VolumeRegistry {
    map: std::sync::RwLock<Vec<Arc<dyn Volume>>>,
}

impl VolumeRegistry {
    pub fn new() -> Self {
        Self {
            map: std::sync::RwLock::new(Vec::new()),
        }
    }

    /// 向仓里加入一个卷。返回该卷自生的 id（与 `Volume::id()` 相同）。
    pub fn add(&self, volume: Arc<dyn Volume>) -> VolumeId {
        let id = volume.id();
        self.map.write().unwrap().push(volume);
        id
    }

    /// 列出仓里所有卷（不对外暴露裸 `Arc<dyn Volume>`）。
    pub fn list(&self) -> Vec<VolumeInfo> {
        let vols = self.map.read().unwrap();
        vols.iter()
            .map(|v| VolumeInfo {
                id: v.id(),
                name: display_name(&**v),
                kind: v.kind(),
                writable: v.writable(),
                note: note_for(&**v),
            })
            .collect()
    }

    /// 按 id 取一个卷（共享引用，UI 读、MCP 读都用它）。
    pub fn get(&self, id: &VolumeId) -> Option<Arc<dyn Volume>> {
        self.map
            .read()
            .unwrap()
            .iter()
            .find(|v| v.id() == *id)
            .cloned()
    }

    /// 按 id 取一个卷（可变引用，CLI 选卷/切卷）。非本仓的调用方不要用。
    pub fn get_mut(&self, id: &VolumeId) -> Option<Arc<dyn Volume>> {
        self.map
            .write()
            .unwrap()
            .iter()
            .find(|v| v.id() == *id)
            .cloned()
    }

    /// 关掉所有卷，重建空仓。
    pub fn clear(&self) {
        *self.map.write().unwrap() = Vec::new();
    }

    /// 按「参数里出现的路径」挑卷：谁的 id 前缀匹配得最长就用谁。
    ///
    /// 这条规则原来长在 CLI 里。提到这里是因为它必须**三个入口共用一份**：
    /// 每个客户端各写一遍，迟早出现"CLI 能读包内文件、MCP 说找不到"，
    /// 而那种分叉看起来像某个入口坏了，实际是两份规则漂移了。
    /// 匹配不到返回 None，由调用方回退默认卷（行为与单卷时代一致）。
    pub fn route_for(&self, samples: &[String]) -> Option<Arc<dyn Volume>> {
        let ids: Vec<VolumeId> = self.list().into_iter().map(|v| v.id).collect();
        let mut best: Option<VolumeId> = None;
        for s in samples {
            for id in &ids {
                if s.starts_with(&id.0) && best.as_ref().is_none_or(|b| id.0.len() > b.0.len()) {
                    best = Some(id.clone());
                }
            }
        }
        best.and_then(|id| self.get(&id))
    }

    /// 直接从工具参数 JSON 路由（字段名单一见 [`path_samples`]）。
    pub fn route_for_args(&self, args: &serde_json::Value) -> Option<Arc<dyn Volume>> {
        self.route_for(&path_samples(args))
    }

    /// 只拿**一条**路径去路由——跨卷搬运用它判断"目标目录是不是另一个卷"。
    ///
    /// 刻意只是 [`route_for`] 的一层薄壳：匹配规则必须只有那一份，
    /// 两处各写就会漂移出"源侧认这个卷、目标侧认那个卷"的怪结果。
    pub fn route_for_sample(&self, sample: &str) -> Option<Arc<dyn Volume>> {
        self.route_for(&[sample.to_string()])
    }

    /// 目标目录属于哪个卷——跨卷搬运的**第二条**路由，比 [`route_for_sample`] 多认一层。
    ///
    /// 为什么不能只用前者：前者的规则是"谁的 **id** 是这条字符串的前缀"，而
    /// `D:\tmp\out` 不是任何 id 的前缀（id 长这样：`local:D:\tmp\e2e`）。于是
    /// 1. 显式带卷名前缀的写法（`local:D:\out`、`wsl:Ubuntu-24.04/home`）照旧走 id 匹配；
    /// 2. 裸**绝对路径**再按"哪个本地卷的根是它的目录级前缀"挑最长的；
    /// 3. 都不中时，**只注册了一个本地卷**就用它（一台机器上的"本地"就是它，
    ///    且 `LocalVolume` 本来就接受自己根之外的绝对路径）；注册了多个盘则返回 None——
    ///    这时目标必须由用户显式指名，猜一个盘会把文件搬到没点过的地方去。
    pub fn route_for_dest(&self, dest: &str) -> Option<Arc<dyn Volume>> {
        if let Some(v) = self.route_for_sample(dest) {
            return Some(v);
        }
        let dest_path = std::path::Path::new(dest);
        if !dest_path.is_absolute() {
            return None;
        }
        let mut best: Option<(usize, VolumeId)> = None;
        let mut locals = Vec::new();
        for info in self.list() {
            // 判据只有一条：**id 是不是 `local:` 开头的**。归档/WSL/远程卷的 id
            // 各自是 `archive:`/`wsl:`/`ssh:`，剥不出 `local:` 就自然被跳过——
            // 所以这里**不再另加一个 `kind != Local` 的条件**：那条今天没有分辨力
            // （变异"摘掉 kind 筛选"照样全绿就是证据），留着只会让人以为它在承重。
            // 也不看 `writable`：那件事交给 `copy_across` 开头那句「目标卷是只读的」，
            // 它有门，消息也点名到目标卷。
            let Some(root) = info.id.0.strip_prefix("local:") else {
                continue;
            };
            if let Some(depth) = dir_prefix_depth(Path::new(root), dest_path) {
                if best.as_ref().is_none_or(|(d, _)| depth > *d) {
                    best = Some((depth, info.id.clone()));
                }
            }
            locals.push(info.id);
        }
        if let Some((_, id)) = best {
            return self.get(&id);
        }
        if locals.len() == 1 {
            return self.get(&locals[0]);
        }
        None
    }

    /// 路由不上时的兜底卷：注册表里的第一个。
    ///
    /// **不硬编码 id**：`LocalVolume::new(cwd)` 自生的 id 是 `local:<绝对路径>`，
    /// 写死 `local:.` 永远查不到，只能靠这条兜底新建卷——那注册表就白建了。
    pub fn default_volume(&self) -> Option<Arc<dyn Volume>> {
        self.map.read().unwrap().first().cloned()
    }

    /// 「按参数选卷，选不上回退默认卷」。三个入口都该只调这一个：
    /// 分开写就会有人忘了兜底（空表时 panic）或忘了路由（归档路径被当本地路径拼）。
    pub fn route_or_default(&self, args: &serde_json::Value) -> Option<Arc<dyn Volume>> {
        self.route_for_args(args).or_else(|| self.default_volume())
    }

    /// **这次操作的「源卷」**——`copy`/`move` 只看 `sources`，其余工具沿用 [`route_or_default`]。
    ///
    /// 为什么不看 `dest_dir`：这两个工具同时带「从哪拿」和「往哪放」两个方向，而
    /// [`route_for`] 取的是**最长的 id 前缀** ⇒ 目标根比源根更深时，**目标卷会被选成源卷**。
    /// 实测（两个本地卷 `local:C:\a` 与 `local:C:\a\b`，源 `local:C:\a\x.txt`，
    /// 目标 `local:C:\a\b\out`）探针打印 `源卷=local:C:\a\b 同卷=true`
    /// ⇒ 跨卷那一步根本走不到，这条被冒充成同卷搬运。
    /// 伤害面到此为止：本地↔本地退回单卷 `copy` 之后**盘上照样成功**（`LocalVolume` 收自己根之外的
    /// 绝对路径）——变异复打时 `fmnr-ai` 那条分派层门照样绿就是这个意思。更糟的「源在错卷里
    /// 解析成 NotFound」要跨 kinds 的卷对（本地卷 id 比归档卷 id 更长才抢得赢），本机造不出那种形状，**未证**。
    /// `sources` 缺失或一条都没有时**保持老行为**（按全样本路由）：那是非法调用，
    /// 由工具层的参数校验去报，不该在这里改选卷规则。
    pub fn route_source(&self, tool: &str, args: &serde_json::Value) -> Option<Arc<dyn Volume>> {
        let base = match tool {
            "copy" | "move" => {
                let sources = source_samples(args);
                if sources.is_empty() {
                    self.route_for_args(args)
                } else {
                    self.route_for(&sources)
                }
            }
            _ => self.route_for_args(args),
        };
        base.or_else(|| self.default_volume())
    }
}

/// 只取 `copy`/`move` 的**源侧**样本（`sources` 数组），不碰目标字段。
/// 与 [`path_samples`] 的分工：那份清单回答「这条调用涉及哪些卷」，
/// 这一份回答「要被动的那些条目住在哪个卷」——跨卷搬运里这两个答案可以不同。
pub fn source_samples(args: &serde_json::Value) -> Vec<String> {
    args.get("sources")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 工具参数里"哪些字段是路径"的**唯一**清单。
///
/// 只认已知字段名，不递归猜：拼错字段（`srcs` 写成 `source`）时应当
/// 回退默认卷并让调用方看见，而不是被当成路径样本路由到莫名其妙的卷上。
pub fn path_samples(args: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    for k in ["path", "root", "dest_dir", "to", "from"] {
        if let Some(s) = args.get(k).and_then(|v| v.as_str()) {
            out.push(s.to_string());
        }
    }
    if let Some(arr) = args.get("sources").and_then(|v| v.as_array()) {
        out.extend(arr.iter().filter_map(|v| v.as_str().map(str::to_string)));
    }
    out
}

/// `root` 是否是 `dest` 的**目录级**前缀；是则返回 `root` 的成分数（用来挑最长的那一个）。
///
/// 两处刻意：① **按成分比不按字符串**——`D:\tmp\e2` 不是 `D:\tmp\e20\out` 的前缀，
/// 用 `starts_with` 会把文件搬进隔壁盘目录；② 盘符大小写归一（Windows 上
/// `d:\x` 与 `D:\x` 是同一个地方，而注册表里两个写法都会出现）。
/// 只归一**大小写**，不做别的规范化。
fn dir_prefix_depth(root: &Path, dest: &Path) -> Option<usize> {
    let comps = |p: &Path| -> Vec<String> {
        p.components()
            .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
            .collect::<Vec<_>>()
    };
    let (r, d) = (comps(root), comps(dest));
    if r.is_empty() || r.len() > d.len() {
        return None;
    }
    (d[..r.len()] == r[..]).then_some(r.len())
}

/// 一个「卷来源」的对外描述：挂载尝试的结果。
///
/// 存在的理由：挂载会失败（文件没了、归档坏了、SSH 配置错），
/// 而失败的**原因**必须能被 UI/CLI/MCP 显示出来，
/// 不能只是从列表里静默消失——那用户只会看到「我用不了」却不知道为什么。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VolumeSourceInfo {
    /// 来源的人类可读名（`archive:D:\x\a.zip`、`ssh://u@h`）。
    pub display: String,
    /// 是否真的挂上了。
    pub mounted: bool,
    /// 挂载失败的原因；成功时为 None。
    pub error: Option<String>,
    /// 挂载成功后该卷的类型。
    pub kind: VolumeKind,
}

fn display_name(v: &dyn Volume) -> String {
    // 直接用 id 的字符串，最稳：本地盘是 local:C、归档是 archive:/x/a.zip
    // 这类名字人类就可读，虚拟卷也在枚举里一目了然。
    v.id().0.clone()
}

fn note_for(v: &dyn Volume) -> String {
    match v.kind() {
        crate::VolumeKind::Archive => "只读挂载，进卷时建好条目索引".to_string(),
        crate::VolumeKind::Wsl => "WSL 卷".to_string(),
        crate::VolumeKind::Remote => "SSH/SFTP 远程卷".to_string(),
        _ => String::new(),
    }
}

// 空仓是「没有数据」；包装 `list()` 以便 CLI 打印「没有卷」。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ListOpts, Result, VPath, Volume};

    struct DummyVol {
        id: VolumeId,
        kind: VolumeKind,
        entries: Vec<crate::Entry>,
    }

    #[async_trait::async_trait]
    impl Volume for DummyVol {
        fn id(&self) -> VolumeId {
            self.id.clone()
        }
        fn kind(&self) -> VolumeKind {
            self.kind
        }
        async fn list(&self, _path: &VPath, _opts: &ListOpts) -> Result<Vec<crate::Entry>> {
            Ok(self.entries.clone())
        }
        async fn read_stream(
            &self,
            _path: &VPath,
        ) -> Result<
            std::boxed::Box<dyn futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin>,
        > {
            panic!("not used in dummy");
        }
        async fn write(&self, _path: &VPath, _data: &[u8]) -> Result<()> {
            panic!("not used in dummy");
        }
        fn writable(&self) -> bool {
            true
        }
        fn native_path(&self, _path: &VPath) -> Option<std::path::PathBuf> {
            None
        }
        async fn spawn_external_open(
            &self,
            _file_path: &VPath,
            _program: &str,
        ) -> Result<crate::ExternalOpen> {
            panic!("not used in dummy");
        }
        fn external_opens(&self) -> &crate::ExternalOpens {
            panic!("not used in dummy");
        }
        async fn copy(
            &self,
            _sources: &[VPath],
            _dest_dir: &VPath,
            _policy: crate::ConflictPolicy,
            _progress: std::sync::Arc<crate::Progress>,
        ) -> Result<crate::OpReport> {
            panic!("not used in dummy");
        }
        async fn move_entries(
            &self,
            _sources: &[VPath],
            _dest_dir: &VPath,
            _policy: crate::ConflictPolicy,
            _progress: std::sync::Arc<crate::Progress>,
        ) -> Result<crate::OpReport> {
            panic!("not used in dummy");
        }
        async fn delete(
            &self,
            _targets: &[VPath],
            _progress: std::sync::Arc<crate::Progress>,
        ) -> Result<crate::OpReport> {
            panic!("not used in dummy");
        }
        async fn rename(&self, _from: &VPath, _to: &VPath) -> Result<()> {
            panic!("not used in dummy");
        }
        async fn create_dir(&self, _path: &VPath) -> Result<()> {
            panic!("not used in dummy");
        }
        async fn search(
            &self,
            _root: &VPath,
            _needle: String,
            _limit: usize,
        ) -> Result<Vec<VPath>> {
            panic!("not used in dummy");
        }
    }

    /// 侧栏与 CLI 上那句归档说明，必须是**量出来的**事实。
    ///
    /// 🔴 原话是「只读挂载，懒加载目录」，而实测 `ArchiveVolume::open` 走的就是
    /// `index()`：进卷时一次性把全量条目索引建好（111,100 条 / 11,100 层的合成 zip
    /// 要 1,247 ms；9.3 MiB 的 tar.gz 因为没有中央目录要整条扫过，要 509 ms），
    /// 真正按需的只有成员内容。数字与尺见 `docs/NEXT-BATCH.md` §21-2。
    /// 这条门同时钉住"不许再说懒加载目录"——同一句事实写两份文案就是本轮改错的源头。
    #[test]
    fn archive_note_states_eager_index_not_lazy_listing() {
        let reg = VolumeRegistry::new();
        reg.add(Arc::new(DummyVol {
            id: VolumeId("archive:/x/a.zip".into()),
            kind: crate::VolumeKind::Archive,
            entries: Vec::new(),
        }));
        let info = reg
            .list()
            .into_iter()
            .find(|v| v.kind == crate::VolumeKind::Archive)
            .expect("归档卷该列得出来");
        assert!(info.note.contains("只读"), "{}", info.note);
        assert!(info.note.contains("索引"), "{}", info.note);
        assert!(
            !info.note.contains("懒加载"),
            "{}：条目索引是进卷时一次建好的，「懒加载目录」是说反了",
            info.note
        );
    }

    #[test]
    fn registry_unique_ids_across_volumes() {
        let reg = VolumeRegistry::new();
        let a = Arc::new(DummyVol {
            id: VolumeId("v1".into()),
            kind: crate::VolumeKind::Local,
            entries: Vec::new(),
        });
        let b = Arc::new(DummyVol {
            id: VolumeId("v2".into()),
            kind: crate::VolumeKind::Archive,
            entries: Vec::new(),
        });
        reg.add(a);
        reg.add(b);
        assert_eq!(reg.list().len(), 2);
        assert!(reg.get(&VolumeId("v1".into())).is_some());
        assert!(reg.get(&VolumeId("v2".into())).is_some());
        assert!(reg.get(&VolumeId("v3".into())).is_none());
    }

    /// 回归：`add()` 必须回传**卷自生的** id。
    ///
    /// UI 侧曾把当前卷 id 硬编码成 `local:.`，而 `LocalVolume::new(home)` 自生的
    /// id 是 `local:<home 绝对路径>`——两者永不相等，`get()` 永远落空，
    /// 只能靠 fallback 兜底。这个 bug 不报错、不 panic，只让「卷接线」静默失效。
    /// 断言 `add` 的返回值本身就是可查得的 key。
    #[test]
    fn add_returns_lookup_key() {
        let reg = VolumeRegistry::new();
        let id = reg.add(Arc::new(DummyVol {
            id: VolumeId("local:C:\\Users\\x".into()),
            kind: crate::VolumeKind::Local,
            entries: Vec::new(),
        }));
        assert_eq!(id, VolumeId("local:C:\\Users\\x".into()));
        assert!(reg.get(&id).is_some(), "add() 回传的 id 必须能直接查回该卷");
        assert_eq!(reg.list().len(), 1);
        assert_eq!(reg.list()[0].id, id);
    }

    /// 只读卷必须在 `VolumeInfo` 上如实标注：UI 靠它给只读卷加「（只读）」后缀。
    #[test]
    fn readonly_is_surfaced() {
        let reg = VolumeRegistry::new();
        reg.add(Arc::new(DummyVol {
            id: VolumeId("archive:a.zip".into()),
            kind: crate::VolumeKind::Archive,
            entries: Vec::new(),
        }));
        let info = &reg.list()[0];
        assert_eq!(info.kind, crate::VolumeKind::Archive);
        assert!(info.writable, "DummyVol 自报可写，这里断言字段被如实透传");
        assert!(!info.note.is_empty(), "归档卷应有说明文案");
    }

    fn dummy(id: &str) -> Arc<DummyVol> {
        Arc::new(DummyVol {
            id: VolumeId(id.into()),
            kind: crate::VolumeKind::Local,
            entries: Vec::new(),
        })
    }

    /// 🔴 目标路由的第二条易错点：**按目录成分比，不按字符串**。
    /// 用 `starts_with` 的话 `local:C:\Foo` 会把 `C:\Football\a.txt` 也认成自己的，
    /// 于是文件被搬进隔壁那个盘目录——同一个形状在 `fmnr-pkg::owner` 里已经栽过一次，
    /// 那里有专门的 `C:\Foo` vs `C:\Football` 测试。
    #[test]
    fn dest_routing_matches_directory_not_string_prefix() {
        let reg = VolumeRegistry::new();
        reg.add(dummy("local:C:\\Foo"));
        reg.add(dummy("local:C:\\Other"));
        assert!(
            reg.route_for_dest("C:\\Football\\a.txt").is_none(),
            "字符串前缀会把 Football 认进 C:\\Foo 的根里——两个都不含它就谁也不是，不许猜"
        );

        // 真在根下的要认对，并且更深的那个根要赢（最长匹配）。
        let deep = VolumeRegistry::new();
        deep.add(dummy("local:C:\\Users"));
        deep.add(dummy("local:C:\\Users\\lbx13"));
        assert_eq!(
            deep.route_for_dest("C:\\Users\\lbx13\\out")
                .expect("更深的根要赢")
                .id()
                .0,
            "local:C:\\Users\\lbx13"
        );
        // 盘符大小写不同是同一个地方
        assert_eq!(
            deep.route_for_dest("c:\\users\\lbx13\\out")
                .expect("盘符与根的大小写要归一")
                .id()
                .0,
            "local:C:\\Users\\lbx13"
        );
    }

    /// 只注册了**一个**可写本地卷时，裸绝对路径可以退到它；两个就不许猜。
    #[test]
    fn dest_routing_falls_back_only_when_a_single_local_volume_exists() {
        let one = VolumeRegistry::new();
        one.add(dummy("local:C:\\Users\\lbx13"));
        assert_eq!(
            one.route_for_dest("D:\\somewhere\\out")
                .expect("唯一一个本地卷时兜底到它")
                .id()
                .0,
            "local:C:\\Users\\lbx13"
        );

        let two = VolumeRegistry::new();
        two.add(dummy("local:C:\\Users\\lbx13"));
        two.add(dummy("local:D:\\Data"));
        assert!(
            two.route_for_dest("E:\\out").is_none(),
            "两个本地盘都装不下 E:\\out 时不许猜一个——那时目标必须显式带卷名前缀"
        );
    }

    /// 🔴 **相对路径的目标一律不路由**，哪怕本机只注册了一个本地卷。
    /// 摘掉 `is_absolute` 那道闸的话，兜底那一档会把 `--to out` 也算给那个唯一的本地卷：
    /// 源在归档卷时看起来"更好用"，但同一个写法在**装了几个本地盘的两台机器上落点不同**
    /// （一个卷时跨卷写到 CWD，两个卷时报错），而"不带前缀 = 仍在源卷里解析"这条对外
    /// 规则会被改成跟机器配置有关。宁可让源卷自己报「本卷只读」。
    /// 这条是变异复打量出来的：刀口"去掉绝对性检查"原本**全绿**。
    #[test]
    fn a_relative_dest_is_never_routed() {
        let one = VolumeRegistry::new();
        one.add(dummy("local:C:\\Users\\lbx13"));
        assert!(
            one.route_for_dest("out").is_none(),
            "相对路径不许走『唯一一个本地卷』的兜底"
        );
        assert!(
            one.route_for_dest("sub\\out").is_none(),
            "带分隔符的相对路径也不许"
        );
    }

    /// 🔴 **源卷的选择不许看目标**（`copy`/`move`）。这条是先有实测才有门的：
    /// 两个本地卷 `local:C:\a`（浅）与 `local:C:\a\b`（深），源在浅根、目标在深根时，
    /// 老规则（全样本 + 最长 id 前缀）会把**深根那个选成源卷**——
    /// 于是「源卷 == 目标卷」，跨卷那一步根本走不到（会不会进一步解析错，按卷的种类而定，
    /// 见上面 `route_source` 的注释）。
    /// 测试里把两条规则都跑一遍：拿到不同答案才说明这条门真的在分辨"看不看目标"。
    #[test]
    fn the_source_volume_ignores_the_destination() {
        let reg = VolumeRegistry::new();
        reg.add(dummy("local:C:\\a"));
        reg.add(dummy("local:C:\\a\\b"));
        let args = serde_json::json!({
            "sources": ["local:C:\\a\\x.txt"],
            "dest_dir": "local:C:\\a\\b\\out"
        });
        // 老规则（照旧可用，别的工具仍走它）：目标把源卷抢走了——这一条钉的是"它确实会抢"
        assert_eq!(
            reg.route_or_default(&args).expect("有卷").id().0,
            "local:C:\\a\\b",
            "全样本规则本来就会被更深的目标根带走；这条断言一旦变绿说明规则漂了"
        );
        // 新规则：源只看 sources
        assert_eq!(
            reg.route_source("copy", &args).expect("有卷").id().0,
            "local:C:\\a",
            "源卷必须由 sources 决定，不许被 dest_dir 带走"
        );
        assert_eq!(
            reg.route_source("move", &args).expect("有卷").id().0,
            "local:C:\\a"
        );
        // 其它工具没有"两个方向"，仍按全样本走（规则不许在这儿顺手改掉）
        assert_eq!(
            reg.route_source("delete", &args).expect("有卷").id().0,
            "local:C:\\a\\b"
        );
        // sources 缺失时保持老行为：那是非法调用，该由参数校验报，不该在选卷这层猜
        let no_sources = serde_json::json!({ "dest_dir": "local:C:\\a\\b\\out" });
        assert_eq!(
            reg.route_source("copy", &no_sources).expect("有卷").id().0,
            "local:C:\\a\\b"
        );
    }

    /// 卷路由的两个易错点之一：**最长 id 前缀**优先。
    /// 选错卷不报错，只会静默走错（表现为莫名的 NotFound），所以钉死。
    #[test]
    fn route_picks_the_longest_matching_volume_id() {
        let reg = VolumeRegistry::new();
        reg.add(dummy("archive:D:/x.zip"));
        reg.add(dummy("archive:D:/x.zip/deep.zip"));
        reg.add(dummy("local:C:"));
        let got = reg
            .route_for(&["archive:D:/x.zip/deep.zip/a/b".to_string()])
            .expect("应当命中某个卷");
        assert_eq!(got.id().0, "archive:D:/x.zip/deep.zip");
        let got2 = reg
            .route_for(&["archive:D:/x.zip/top.txt".to_string()])
            .expect("应当命中某个卷");
        assert_eq!(got2.id().0, "archive:D:/x.zip");
    }

    /// 谁都不匹配时返回 None，让调用方回退默认卷——
    /// 直接报错会让"没挂归档卷"的普通用法变红。
    #[test]
    fn route_returns_none_when_nothing_matches() {
        let reg = VolumeRegistry::new();
        reg.add(dummy("local:C:"));
        assert!(reg.route_for(&["D:/elsewhere".to_string()]).is_none());
        assert!(reg.route_for(&[]).is_none());
    }

    /// 另一个易错点：**只认已知路径字段**。
    /// 搜索词混进样本会让一个跟卷无关的 query 意外决定走哪个卷。
    #[test]
    fn path_samples_reads_only_known_fields() {
        let args = serde_json::json!({
            "path": "archive:D:/x.zip/a",
            "root": "archive:D:/x.zip",
            "from": "archive:D:/x.zip/b",
            "sources": ["a.txt", "archive:D:/x.zip/c"],
            "query": "archive:D:/x.zip",
            "note": "archive:D:/x.zip",
        });
        let got = path_samples(&args);
        for want in [
            "archive:D:/x.zip/a",
            "archive:D:/x.zip",
            "archive:D:/x.zip/b",
            "archive:D:/x.zip/c",
            "a.txt",
        ] {
            assert!(
                got.iter().any(|s| s == want),
                "{want} 应当是样本，实得 {got:?}"
            );
        }
        // query/note 的值换成不像路径的串再断一次：
        // 上面那条 "archive:D:/x.zip" 恰好与 root 同值，单独看它判不出有没有混进来。
        let noisy = serde_json::json!({"query": "hello world", "note": "hello world", "path": "p"});
        assert!(
            !path_samples(&noisy).iter().any(|s| s == "hello world"),
            "非路径字段不该进样本：{noisy:?}"
        );
    }
}
