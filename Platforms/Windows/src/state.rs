//! UI/worker 共享状态：UI 线程（WndProc）与 BLE worker 通过 `Arc<Mutex<UiState>>` 单向交换数据。
//! 上行（worker 写、UI 读）= 扫描结果 / 连接状态 / 事件 / 错误；下行（UI 写、worker 读并清空）= 命令。
//! 跨线程只经此结构；另有 WinRT 通知线程仅把字节推入 worker 私有队列。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::settings::{self, CloseBehavior, Theme};

/// 设备条目"还活着"的有效期：超过这个时间没再收到广播就摘掉（低延迟广播几十毫秒一次，
/// 12 s 足够容忍偶发丢包）。必须有 TTL：广播用的是**随机可解析地址（RPA）**，手机重启或周期性
/// 轮换都换新地址；只按地址去重、不过期 → 列表堆满同一台手机的历史地址，按"最后一条"取
/// 还会选中已死的地址
pub(crate) const DEVICE_TTL: std::time::Duration = std::time::Duration::from_secs(12);

// 各列表只留最近若干条；气泡上限专防断线重连补漏时刷屏
pub(crate) const MAX_NOTIFICATIONS: usize = 20;
pub(crate) const MAX_FILE_TASKS: usize = 10;
pub(crate) const MAX_ERRORS: usize = 3;
pub(crate) const MAX_PENDING_TOASTS: usize = 4;
pub(crate) const COPIED_HINT_TTL: std::time::Duration = std::time::Duration::from_millis(1600);
pub(crate) const MAX_INPUT_CHARS: usize = 400;

pub(crate) const TASK_DIR_SEND: u8 = 0;
pub(crate) const TASK_DIR_RECV: u8 = 1;

/// 一条最近错误 + 它连续重复了几次。必须计数而不是各占一行：`MAX_ERRORS` 只有 3，
/// 链路重建时同一个原因会连着报三四次，把整块错误区刷满、真正的错误反而挤出去。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ErrorRow {
    pub(crate) msg: String,
    pub(crate) count: u32,
}

impl ErrorRow {
    pub(crate) fn text(&self) -> String {
        if self.count > 1 {
            format!("{}（重复 {} 次）", self.msg, self.count)
        } else {
            self.msg.clone()
        }
    }
}

// 文本输入焦点：无 / 文件页「发送路径」/ 连接页「手动 IP」/ 通知页「回复」
pub(crate) const FOCUS_NONE: u8 = 0;
pub(crate) const FOCUS_SEND_PATH: u8 = 1;
pub(crate) const FOCUS_MANUAL_IP: u8 = 2;
pub(crate) const FOCUS_REPLY: u8 = 3;

/// 回复结果提示的停留时长：比"复制成功"那类反馈长一档，因为它常常是一句要人读明白的失败原因
pub(crate) const REPLY_HINT_TTL: std::time::Duration = std::time::Duration::from_millis(4000);
/// 等手机回执的上限。老版本手机不认识这条请求、只会自己弹一句"未知消息类型"，电脑这边
/// 若不等出结果就是"点了没反应"——那正是本项目最反对的静默失败形态。
pub(crate) const REPLY_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

// ---------- 相册（图片互传）：几条内存与带宽闸门 ----------

/// 一页请求多少张：要铺满网格视口（本壳没有滚动视图，超出视口既看不见也不该下载）
pub(crate) const ALBUM_PER_PAGE: u32 = 24;
pub(crate) const ALBUM_THUMB_EDGE: u32 = 256;
/// 缩略图缓存的**条数**与**字节**两条线都要有：条数防几千张照片把缓存无限撑大，
/// 字节防 240 条 × 手机侧允许的 200 KB = 48 MB。按字节封顶后这一层占用与相册大小**无关**
pub(crate) const ALBUM_THUMB_MAX: usize = 240;
pub(crate) const ALBUM_THUMB_BYTES_MAX: usize = 6 * 1024 * 1024;
/// 同时在途请求上限（FIFO）：不设上限时一次翻页把 24 个请求压给手机，解码队列满了后到的先回
pub(crate) const ALBUM_THUMB_INFLIGHT: usize = 4;
pub(crate) const ALBUM_THUMB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// 拖出载荷保留数的软上限与**硬顶**（见 `AlbumView::drag_ready`）：留 0 张大文件永远拖不出去，
/// 留得多就是往临时目录堆几 GB 视频。多选拖出的淘汰会绕开选中的那几张，全选逐张预取能一路
/// 绕开软上限，所以还要硬顶兜住（一张视频 200 MB 量级）
pub(crate) const ALBUM_DRAG_KEEP: usize = 2;
pub(crate) const ALBUM_DRAG_KEEP_HARD: usize = 6;

/// 相册清单里的一条（`AlbumItem` 的界面投影；协议结构体不进 UI 状态，否则 UI 依赖 prost 类型）
#[derive(Debug, Clone)]
pub(crate) struct AlbumItemView {
    pub id: u64,
    pub name: String,
    pub size_bytes: i64,
    pub mtime_ms: i64,
    pub width: u32,
    pub height: u32,
    /// 0 = 照片，1 = 视频。**未知值一律按照片渲染**：手机版本比电脑新时，
    /// 最坏结果是"视频没时长角标"，而不是整格空白
    pub kind: u32,
    pub duration_ms: i64,
}

pub(crate) const ALBUM_KIND_VIDEO: u32 = 1;

impl AlbumItemView {
    pub(crate) fn is_video(&self) -> bool {
        self.kind == ALBUM_KIND_VIDEO
    }

    /// 时长写法：不足一小时 `0:23`，超过则 `1:02:07`。秒数向下取整 —— 四舍五入会把 9.6 秒
    /// 显示成 10 秒，而"看着比实际长"更容易被当成导出卡住
    pub(crate) fn duration_label(&self) -> String {
        let secs = (self.duration_ms.max(0) / 1000) as u64;
        if secs >= 3600 {
            format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
        } else {
            format!("{}:{:02}", secs / 60, secs % 60)
        }
    }
}

/// 一格缩略图的状态。`Failed` 带原因文本并画在格子上：失败是每张独立发生的，
/// 统一显示成空白就等于告诉用户"这台手机没这张照片"
#[derive(Debug, Clone)]
pub(crate) enum ThumbSlot {
    Pending,
    Ready(Arc<Vec<u8>>),
    Failed(String),
}

impl ThumbSlot {
    pub(crate) fn bytes(&self) -> usize {
        match self {
            ThumbSlot::Ready(j) => j.len(),
            _ => 0,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ThumbEntry {
    pub slot: ThumbSlot,
    pub seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AlbumPurpose {
    Export,
    /// 拖出到资源管理器/微信的临时载荷，拖完即删
    Drag,
}

/// 相册页状态：清单在内存、缩略图**只在内存**（产品口径"关掉就没有"，这条路径上
/// 不允许任何缩略图写盘，只有导出/拖出的原图落盘）
#[derive(Debug, Default)]
pub(crate) struct AlbumView {
    pub items: Vec<AlbumItemView>,
    pub total: u32,
    pub page: u32,
    /// 当前页按每页多少张取回的：翻页必须沿用这个数，否则 resize 后页边界移动、中间照片被永久跳过
    pub per_page: u32,
    pub loading: bool,
    /// 手机侧原话逐字显示："权限没给""相册为空""读失败"是三件事，收敛成"加载失败"就丢了可行动信息
    pub error: String,
    pub thumbs: HashMap<u64, ThumbEntry>,
    /// 页码代际：翻页/离开页签时自增，旧代际的在途应答一律丢弃，否则"上一页的图闪进这一页"
    pub generation: u64,
    pub seq: u64,
    pub selected: Vec<u64>,
    pub export_dir: String,
    /// 原图落盘路由：album_id → 用途。没登记的 id 来了就是**意外**，必须报错而不是顺手丢进收件目录
    pub routes: HashMap<u64, AlbumPurpose>,
    /// 本机**主动撤销**过的拖出：原图可能已经在路上，到货时要能区分"手机乱发"（拦住报错）
    /// 和"我自己撤了"（安静丢掉）
    pub cancelled: HashSet<u64>,
    // ---- 命令（UI 写、worker 取走并清空）----
    pub req_list: Option<(u32, u32)>,
    pub thumb_one_req: Option<u64>,
    pub req_thumbs: Vec<u64>,
    pub req_full: Option<Vec<u64>>,
    pub drag_req: Vec<u64>,
    pub drag_result: Option<(u64, Result<String, String>)>,
    /// 已取回、**留着等"再拖一次"**的载荷 `(相册 id, 落盘路径)`，最旧的在前：第一次拖只让取回跑完并留下结果，
    /// 第二次拖直接命中、立刻出去。超出保留数删最旧；启动时 %TEMP%\LinkX 整体清扫预取垃圾
    pub drag_ready: Vec<(u64, String)>,
    /// 拖出取回进度 `相册 id → (已收, 总)`：界面用它画环，圆环跑完就是可以拖。
    /// 到货、失败、撤单都要清掉，留着会让一格永远转下去
    pub drag_progress: HashMap<u64, (u64, u64)>,
    pub inflight: usize,
    pub visible_start: usize,
    pub visible_count: usize,
    /// 网格纵向滚动的**起始行**（一行 `cols` 格）：绘制、命中、悬停、下载窗口四处必须读同一个值，
    /// 否则"点到的格子亮的是别格"
    pub scroll_row: usize,
}

impl AlbumView {
    pub(crate) fn slot(&self, id: u64) -> Option<&ThumbSlot> {
        self.thumbs.get(&id).map(|e| &e.slot)
    }

    /// 只有"从没请求过"才排队：`Pending` 已在途、`Failed` 是手机侧明确回过失败的，
    /// 都不该在每次重绘时再发一次（那会变成对着坏图无限重试）
    pub(crate) fn needs_thumb(&self, id: u64) -> bool {
        !self.thumbs.contains_key(&id)
    }

    pub(crate) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.req_thumbs.clear();
    }

    /// 用户点「刷新」时丢掉所有失败格：`needs_thumb` 故意不自动重试坏图，
    /// 链路抖动造成的永久失败得有个人工重来的入口，否则界面一直红着
    pub(crate) fn drop_failed_thumbs(&mut self) {
        self.thumbs
            .retain(|_, e| !matches!(e.slot, ThumbSlot::Failed(_)));
    }

    /// 取消一次拖出的原图预取。预取是"按下格子就发"的，而多数按下其实只是**点一下选中**，
    /// 那份原图没人会用，留在临时目录里就是几百 MB 的垃圾
    pub(crate) fn cancel_drag(&mut self, id: u64) {
        // 「已经取回、留着等再拖一次」的那一份不算预取垃圾，撤点击时不许顺手删掉
        if matches!(&self.drag_result, Some((gid, Ok(_))) if *gid == id)
            && self.drag_ready_path(id).is_none()
        {
            if let Some((_, Ok(path))) = self.drag_result.take() {
                let _ = std::fs::remove_file(path);
            }
        }
        // 路由一撤，稍后才到达的原图会被拒收（不会再往临时目录里写半个文件）
        if self.routes.remove(&id).is_some() {
            self.cancelled.insert(id);
        }
        self.drag_req.retain(|q| *q != id);
        // 撤了单这一格就没有"转完可以拖"这回事了，进度环必须跟着停
        self.drag_progress.remove(&id);
    }

    pub(crate) fn take_cancelled(&mut self, id: u64) -> bool {
        self.cancelled.remove(&id)
    }

    pub(crate) fn keep_drag(&mut self, id: u64, path: String) {
        self.drag_ready.retain(|(gid, p)| {
            let _ = gid;
            std::path::Path::new(p).is_file()
        });
        self.drag_ready.retain(|(gid, _)| *gid != id);
        self.drag_ready.push((id, path));
        // 超出保留名额时**先挑没被选中的下手**：多选拖出要求"选中的每一张都还在本地"，
        // 否则选到第三张就把第一张的载荷删了，拖出去必然少文件、还一句话不说。
        // 全都在选中集里就宁可超一点 —— 那批是用户明确要的东西，上限只防"随手乱点"
        while self.drag_ready.len() > ALBUM_DRAG_KEEP {
            match self
                .drag_ready
                .iter()
                .position(|(gid, _)| !self.selected.contains(gid))
            {
                Some(i) => {
                    let (_, old) = self.drag_ready.remove(i);
                    let _ = std::fs::remove_file(old);
                }
                None => break,
            }
        }
        // 硬顶：上面那条"宁可超一点"在全选逐张预取时会一路超到几十张全尺寸原图
        while self.drag_ready.len() > ALBUM_DRAG_KEEP_HARD {
            let (gid, old) = self.drag_ready.remove(0);
            self.drag_progress.remove(&gid);
            let _ = std::fs::remove_file(old);
        }
    }

    pub(crate) fn drag_ready_path(&self, id: u64) -> Option<String> {
        self.drag_ready
            .iter()
            .find(|(gid, _)| *gid == id)
            .map(|(_, p)| p.clone())
            .filter(|p| std::path::Path::new(p).is_file())
    }

    pub(crate) fn forget_drag(&mut self, id: u64) {
        self.drag_ready.retain(|(gid, _)| *gid != id);
    }

    pub(crate) fn set_drag_progress(&mut self, id: u64, got: u64, total: u64) -> bool {
        let pct = |g: u64, t: u64| g.saturating_mul(100).checked_div(t).unwrap_or(0).min(100);
        let changed = self
            .drag_progress
            .get(&id)
            .is_none_or(|(g, t)| pct(*g, *t) != pct(got, total));
        self.drag_progress.insert(id, (got, total));
        changed
    }

    pub(crate) fn drag_progress_of(&self, id: u64) -> Option<(u64, u64)> {
        self.drag_progress.get(&id).copied()
    }

    pub(crate) fn clear_drag_progress(&mut self, id: u64) {
        self.drag_progress.remove(&id);
    }

    pub(crate) fn put_thumb(&mut self, id: u64, slot: ThumbSlot) {
        self.seq = self.seq.wrapping_add(1);
        self.thumbs.insert(
            id,
            ThumbEntry {
                slot,
                seq: self.seq,
            },
        );
        self.evict_thumbs();
    }

    pub(crate) fn mark_thumb_pending(&mut self, id: u64) {
        self.seq = self.seq.wrapping_add(1);
        self.thumbs.insert(
            id,
            ThumbEntry {
                slot: ThumbSlot::Pending,
                seq: self.seq,
            },
        );
        self.evict_thumbs();
    }

    pub(crate) fn thumb_bytes(&self) -> usize {
        self.thumbs.values().map(|e| e.slot.bytes()).sum()
    }

    /// 超上限时淘汰**最旧的、当前不可见的**条目：正在看的那几张绝不能被淘汰后又在下一帧
    /// 重新请求（那会造成"图片闪一下"的循环）。条数与字节两条线都要看，红线不能靠运气
    fn evict_thumbs(&mut self) {
        let mut count = self.thumbs.len();
        let mut bytes = self.thumb_bytes();
        if count <= ALBUM_THUMB_MAX && bytes <= ALBUM_THUMB_BYTES_MAX {
            return;
        }
        let visible: Vec<u64> = self
            .items
            .iter()
            .skip(self.visible_start)
            .take(self.visible_count)
            .map(|i| i.id)
            .collect();
        while count > ALBUM_THUMB_MAX || bytes > ALBUM_THUMB_BYTES_MAX {
            let victim = self
                .thumbs
                .iter()
                .filter(|(id, _)| !visible.contains(id))
                .min_by_key(|(_, e)| e.seq)
                .map(|(id, e)| (*id, e.slot.bytes()));
            match victim {
                Some((id, n)) => {
                    self.thumbs.remove(&id);
                    count -= 1;
                    bytes -= n;
                }
                // 全在可见窗口内（视口比缓存上限还大，理论上不会发生）：宁可短暂超上限也不能抽走正在显示的图
                None => break,
            }
        }
    }

    pub(crate) fn toggle_selected(&mut self, id: u64) {
        match self.selected.iter().position(|s| *s == id) {
            Some(i) => {
                self.selected.remove(i);
            }
            None => self.selected.push(id),
        }
    }

    pub(crate) fn visible_ids(&self) -> &[AlbumItemView] {
        let start = self.visible_start.min(self.items.len());
        &self.items[start
            ..start
                .saturating_add(self.visible_count)
                .min(self.items.len())]
    }

    pub(crate) fn busy(&self) -> bool {
        self.loading || self.inflight > 0 || !self.req_thumbs.is_empty()
    }

    /// 登记一次"把这些原图取到某个目录"：目标目录 + 待收 id 集合 + 命令一起写。
    /// 导出、单张拖出、`/action/album-export` 三条入口**只走这里** —— 路由表和命令字段是
    /// 一套状态的两个半边，各写各的就会出现"请求发出去了但路由没登记"（文件被当成意外拒掉）
    pub(crate) fn plan_fetch(&mut self, ids: &[u64], dir: String, purpose: AlbumPurpose) {
        if matches!(purpose, AlbumPurpose::Export) {
            self.export_dir = dir;
        }
        for id in ids {
            self.routes.insert(*id, purpose);
            // 新请求作废旧的撤单标记，否则撤单后再点同一格，第二次到货会被当成
            // "我自己撤过的"而**静默拒收**
            self.cancelled.remove(id);
        }
        match purpose {
            AlbumPurpose::Export => self.req_full = Some(ids.to_vec()),
            AlbumPurpose::Drag => {
                self.drag_result = None;
                self.drag_req = ids.to_vec();
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct NotificationItem {
    pub package: String,
    pub title: String,
    pub text: String,
    pub ts_ms: i64,
    /// 对端通知的稳定 key 哈希（0 = 无）：同应用同 key 的通知就地合并更新
    pub key_hash: u32,
    /// 回复定位三元组：手机侧凭 pkg + tag + id 在"仍在通知栏"的条目里现找
    pub tag: String,
    pub notification_id: i32,
    /// 应用自己挂了 RemoteInput。为假就**不许**出现回复入口 —— 那是应用的选择，不是我们没做
    pub can_reply: bool,
    pub reply_action_index: i32,
    pub reply_result_key: String,
}

/// 一条待回复通知的定位（与 `NotificationItem` 的同名字段同源，单独成结构是为了让
/// "点哪一行 → 输入 → 发送 → 回执对上哪一行"这条链只带一份键，不各算一遍）
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplyTarget {
    pub package: String,
    pub tag: String,
    pub notification_id: i32,
    pub action_index: i32,
    pub result_key: String,
}

/// 一次回复请求（UI 写、worker 取走发给手机）
#[derive(Debug, Clone)]
pub(crate) struct ReplyRequest {
    pub reply_id: u32,
    pub target: ReplyTarget,
    pub text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct FileTaskView {
    pub name: String,
    pub direction: u8,
    pub percent: u8,
    pub state: String,
    /// 文件总字节数（0 = 未知，速度就不显示，避免瞎算）
    pub size: u64,
    /// 平滑后的瞬时速度（KB/s）：由 `update_file_task` 推算，界面只读它
    pub speed_kbps: u32,
    prev_percent: u8,
    prev_at: Option<std::time::Instant>,
}

/// 在途状态白名单：只有这些状态下「取消」真的还能掐断点什么。
/// 等对端回执 / 已落终态的行也画按钮就是骗人 —— 分块早交出去了，点下去什么也停不下来
const CANCELLABLE_TASK_STATES: [&str; 6] = [
    "发送中",
    "等待 TCP 通道",
    "等待链路排空",
    "续传中",
    "接收中",
    "重传中",
];

impl FileTaskView {
    /// 这一行是否可取消。**绘制「取消」与命中「取消」必须共用这一个判据**，分两处各判就容易出现"看得见点不到"
    pub(crate) fn is_cancellable(&self) -> bool {
        self.percent < 100 && CANCELLABLE_TASK_STATES.contains(&self.state.as_str())
    }
}

/// 「同名设备呈递了新身份」待用户决策的提示数据：引擎检测到指纹变化后发
/// `EngineEvent::IdentityChanged`，UI 据此弹确认块（复用 SAS 比对区版式），
/// 「信任」→ 回到 Pairing 重走 SAS 复核，「取消」→ 断开。
/// **检测与 UI 派发必须同源**，否则就成了"检测到指纹变化却不弹窗"
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdentityChangeView {
    pub name: String,
    /// 信任库中旧指纹 / 本次握手呈递的新指纹（16 位小写 hex）
    pub old_fp: String,
    pub new_fp: String,
}

/// 询问弹窗上按下的是哪个按钮：鼠标点、回车、Esc 三个入口都归到这三选一
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseChoice {
    Minimize,
    Exit,
    Cancel,
}

/// 关闭按钮该做出的动作
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseAction {
    /// 弹询问窗
    Ask,
    /// 收进托盘：隐藏窗口，进程与托盘图标都留着
    Minimize,
    /// 走既有的退出收尾
    Exit,
    /// 什么都不做
    Stay,
}

/// 一次关闭操作的结果：动作 + 要不要把这次选择记进设置
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CloseDecision {
    pub(crate) action: CloseAction,
    /// `None` = 设置一个字不动
    pub(crate) persist: Option<CloseBehavior>,
}

/// 关闭行为的唯一决策口：`choice = None` 是"刚点了关闭按钮"，`Some` 是"弹窗上有了回答"。
/// 勾了"记住"也只在这一次真的选了一边时才写设置 —— 取消时没有任何值得记住的选择
pub(crate) fn decide_close(
    behavior: CloseBehavior,
    remember: bool,
    choice: Option<CloseChoice>,
) -> CloseDecision {
    let Some(made) = choice else {
        return CloseDecision {
            action: match behavior {
                CloseBehavior::Ask => CloseAction::Ask,
                CloseBehavior::Minimize => CloseAction::Minimize,
                CloseBehavior::Exit => CloseAction::Exit,
            },
            persist: None,
        };
    };
    match made {
        CloseChoice::Cancel => CloseDecision {
            action: CloseAction::Stay,
            persist: None,
        },
        c => CloseDecision {
            action: match c {
                CloseChoice::Minimize => CloseAction::Minimize,
                CloseChoice::Exit => CloseAction::Exit,
                CloseChoice::Cancel => CloseAction::Stay,
            },
            persist: remember.then_some(match c {
                CloseChoice::Minimize => CloseBehavior::Minimize,
                CloseChoice::Exit => CloseBehavior::Exit,
                CloseChoice::Cancel => CloseBehavior::Ask,
            }),
        },
    }
}

/// UI 与 worker 共享状态（单实例，`Arc<Mutex<..>>`）
#[derive(Debug)]
pub(crate) struct UiState {
    // ---- 扫描 / 连接（worker 写，UI 读）----
    /// 扫描到的 LinkX 设备 `(address, name)`：**按最近可见排序，越靠后越新**
    pub devices: Vec<(u64, String)>,
    device_seen: HashMap<u64, Instant>,
    pub selected: Option<u64>,
    pub conn_state: u8,
    pub peer_name: String,
    /// 对端系统标识（1 = Android，2 = Windows）
    pub peer_os: u8,
    pub sas: Option<u32>,
    /// 只是"曾经配对成功"的锁存（只有解绑才清）；判"此刻连着"必须用 `link_paired()`
    pub paired: bool,
    /// 距上一次**收到对端任何一帧**过了多少毫秒。引擎判掉线要等几十秒超时，
    /// 这是界面上唯一"快起来"的存活证据
    pub rx_silent_ms: u64,
    pub peer_fp: Option<String>,
    pub local_fp: String,
    pub identity_change: Option<IdentityChangeView>,
    pub debug_enabled: bool,

    // ---- TCP 通道 / 文件传输 / 设备管理（worker 写，UI 读）----
    pub tcp_ready: bool,
    pub peer_lan_ip: String,
    pub manual_ip_input: String,
    pub file_tasks: Vec<FileTaskView>,
    pub send_path_input: String,
    pub inbox_dir: String,
    pub bound_devices: Vec<(String, String)>,

    pub notifications: Vec<NotificationItem>,
    /// 通知列表滚动起始下标：绘制、命中、悬停三处必须读同一个值（同相册 `scroll_row` 的纪律）
    pub notify_scroll: usize,
    /// 通知回复：当前选中的那条（None = 回复框不出现）
    pub reply_target: Option<ReplyTarget>,
    pub reply_input: String,
    /// 待下发的回复请求（UI 写、worker 取走）
    pub reply_req: Option<ReplyRequest>,
    /// 已发出、正等手机回执的请求：`(reply_id, 发出时刻)`。
    /// 存序号而不是行号：新通知会插到最前，行号会漂。
    pub reply_pending: Vec<(u32, std::time::Instant)>,
    pub reply_next_id: u32,
    /// 回复结果提示（文本, 成功与否, 产生时刻）
    pub reply_hint: Option<(String, bool, std::time::Instant)>,
    pub clip_in: String,
    pub clip_out: String,
    pub clip_sync: bool,
    pub auto_connect: bool,
    pub toast_enabled: bool,
    /// 运行期功能开关：用户**想要**的状态（实际加载见 `crate::features`），关掉后需重启才生效
    pub feat_notifications: bool,
    pub feat_clipboard: bool,
    pub feat_file_transfer: bool,
    pub feat_media: bool,
    pub feat_album: bool,
    pub restart_prompt: bool,
    /// 请求重启自身：置位后关窗，`main` 在托盘摘除、单实例互斥体释放**之后**才拉起新实例
    /// （否则新实例会把自己当成第二实例直接退出）。UI 与 `agent-debug` 线程都只在这把锁内写
    pub restart_req: bool,
    pub toast_show_content: bool,
    pub theme: Theme,
    /// 开机自启动：**注册表里那条启动项的实际状态**（进设置页时回读，`settings.ini` 只是上一次记忆）
    pub autostart: bool,
    /// 启动项内容是否就是本机现在该写的那一份；被外部改过时开关旁要说实话而不是"已开启"
    pub autostart_is_ours: bool,
    /// 点关闭按钮时做什么
    pub close_behavior: CloseBehavior,
    /// 「最小化到托盘还是退出程序」询问弹窗是否开着（模态：盖住背后的一切点击）
    pub close_prompt: bool,
    /// 询问弹窗里「记住我的选择，不再询问」的勾选
    pub close_remember: bool,
    pub errors: Vec<ErrorRow>,
    pub pending_toasts: Vec<(String, String)>,
    /// 界面可见状态的**变化序号**：worker 每轮末尾比对，变了才 `post_state_changed`。
    /// 文件进度写进 `file_tasks` 时不产生任何 Windows 消息，而重绘闸门只看动画/输入焦点 ——
    /// 进度条就一直停在旧值，直到用户点一下窗口才跳，看着像"和安卓端不同步"
    pub ui_rev: u64,
    pub copied_at: Option<Instant>,

    // ---- 命令（UI 写，worker 读并清空）----
    pub connect_req: Option<u64>,
    pub confirm_sas_req: bool,
    pub reject_sas_req: bool,
    pub send_clip_req: Option<String>,
    pub send_file_req: bool,
    pub manual_ip_req: bool,
    /// 取消一条在途任务 `(方向, 文件名)`：与 `update_file_task` 的键同口径，另存一套键两处必然漂
    pub cancel_file_req: Option<(u8, String)>,
    /// 故障注入（只由 `POST /action/drop-*` 设定，发布包没有入口能写它）：把这一号的出/入站分块丢掉
    pub debug_drop_chunk_at: Option<u32>,
    pub debug_drop_recv_chunk_at: Option<u32>,
    pub unbind_requested: bool,
    pub accept_identity_req: bool,
    pub reject_identity_req: bool,

    // ---- UI 线程自持 ----
    pub active_tab: usize,
    pub nav_hover: Option<usize>,
    pub list_hover: Option<(u8, usize)>,
    pub anim_start: Option<Instant>,
    pub hwnd_raw: isize,
    /// 已应用的对端剪贴板内容（防回声：与 WM_CLIPBOARDUPDATE 读到的相同时忽略一次）
    pub last_applied_clip: String,
    pub input_focus: u8,

    // ---- 媒体控制 / 手机状态（worker 写状态，UI 写命令）----
    pub media: Option<MediaView>,
    /// 当前曲目的封面（手机只在局域网通时推来）。按 `track_key` 归属，见 [`UiState::cover_of`]。
    pub media_cover: Option<MediaCoverView>,
    pub media_cmd_req: Option<(i32, i32, i64)>,

    pub battery: Option<BatteryView>,

    pub album: AlbumView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BatteryView {
    pub level: i32,
    pub charging: bool,
    /// 手机侧产生该读数的时刻（epoch 毫秒）：状态帧走 BLE 还是 TCP 没有顺序保证，只认最新全靠它
    pub at_ms: i64,
}

/// 一首歌的身份证：包名 + 曲名 + 艺术家。手机端 `MediaControl.pushCover` 用的是同一拼法，
/// 协议注释里钉死了字段顺序，改这里必须同时改那边。
pub(crate) fn media_track_key(pkg: &str, title: &str, artist: &str) -> String {
    format!("{pkg}|{title}|{artist}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MediaCoverView {
    pub track_key: String,
    pub jpeg: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MediaView {
    pub package: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    pub position_ms: i64,
    pub duration_ms: i64,
    /// 倍速 ×100（1.25x = 125）
    pub speed_x100: i32,
    pub volume: i32,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            devices: Vec::new(),
            device_seen: HashMap::new(),
            selected: None,
            conn_state: 0,
            peer_name: String::new(),
            peer_os: 0,
            sas: None,
            paired: false,
            rx_silent_ms: 0,
            peer_fp: None,
            local_fp: String::new(),
            identity_change: None,
            debug_enabled: false,
            tcp_ready: false,
            peer_lan_ip: String::new(),
            manual_ip_input: String::new(),
            file_tasks: Vec::new(),
            send_path_input: String::new(),
            inbox_dir: default_inbox_dir(),
            bound_devices: Vec::new(),
            notifications: Vec::new(),
            notify_scroll: 0,
            reply_target: None,
            reply_input: String::new(),
            reply_req: None,
            reply_pending: Vec::new(),
            reply_next_id: 1,
            reply_hint: None,
            clip_in: String::new(),
            clip_out: String::new(),
            clip_sync: true,
            toast_enabled: true,
            auto_connect: true,
            feat_notifications: true,
            feat_clipboard: true,
            feat_file_transfer: true,
            feat_media: true,
            feat_album: true,
            restart_prompt: false,
            restart_req: false,
            toast_show_content: true,
            theme: Theme::default(),
            autostart: false,
            autostart_is_ours: true,
            close_behavior: CloseBehavior::default(),
            close_prompt: false,
            close_remember: false,
            errors: Vec::new(),
            pending_toasts: Vec::new(),
            ui_rev: 0,
            copied_at: None,
            connect_req: None,
            confirm_sas_req: false,
            reject_sas_req: false,
            send_clip_req: None,
            send_file_req: false,
            manual_ip_req: false,
            cancel_file_req: None,
            debug_drop_chunk_at: None,
            debug_drop_recv_chunk_at: None,
            unbind_requested: false,
            accept_identity_req: false,
            reject_identity_req: false,
            active_tab: 0,
            nav_hover: None,
            list_hover: None,
            anim_start: None,
            hwnd_raw: 0,
            last_applied_clip: String::new(),
            input_focus: FOCUS_NONE,
            media: None,
            media_cover: None,
            media_cmd_req: None,
            battery: None,
            album: AlbumView {
                per_page: ALBUM_PER_PAGE,
                ..Default::default()
            },
        }
    }
}

/// 默认收件目录 %USERPROFILE%\Downloads\LinkX；该变量缺失时只退回可展示的相对名
pub(crate) fn default_inbox_dir() -> String {
    match std::env::var("USERPROFILE") {
        Ok(v) if !v.trim().is_empty() => {
            format!("{}\\Downloads\\LinkX", v.trim_end_matches(['\\', '/']))
        }
        _ => "LinkX".to_string(),
    }
}

pub(crate) type SharedState = Arc<Mutex<UiState>>;

/// 超过这么久没收到对端任何一帧（心跳 10 s，取 2.5 个周期），界面就不再声称"已配对"
pub(crate) const LINK_SILENCE_MS: u64 = 25_000;

impl UiState {
    /// 现在这首歌的封面，**按 key 认领**：对不上就当作没有。宁可卡片没图，也不能拿上一首的图配现在的歌名。
    pub(crate) fn cover_of_current(&self) -> Option<&MediaCoverView> {
        let m = self.media.as_ref()?;
        let c = self.media_cover.as_ref()?;
        (c.track_key == media_track_key(&m.package, &m.title, &m.artist)).then_some(c)
    }

    /// 「已配对」的唯一判据：**此刻真的能跟那台手机说话**：`paired` 只是"曾经配对成功"的锁存，
    /// 只看它会把"手机已断开"显示成"已配对"；引擎的 `conn_state` 又要等自己的几十秒超时才承认掉线，
    /// 于是再补两条当场看得见的证据：收不到任何帧、对端身份待确认，都算"此刻没连着"
    pub(crate) fn link_paired(&self) -> bool {
        self.paired
            && self.conn_state == linkx_session::engine::state_code::PAIRED
            && self.identity_change.is_none()
            && self.rx_silent_ms < LINK_SILENCE_MS
    }

    /// `link_paired()` 为假时界面该说的那句原因（**只许说真话**："未配对"和"已配对但没连上"是两件事）
    pub(crate) fn link_lie_reason(&self) -> &'static str {
        if !self.paired {
            // 说"还没有配对"会在冷启动时撒谎：本机可能记着好几台已绑定设备，只是这次没连上
            "还没连上手机（点下方设备行连接）"
        } else if self.identity_change.is_some() {
            "对端身份已变化，等你确认"
        } else if self.rx_silent_ms >= LINK_SILENCE_MS {
            "已经收不到手机的消息"
        } else {
            "链路还没起来"
        }
    }

    /// 记录一条扫描到的设备：刷新可见时刻并挪到列表末尾（= 最近可见）；返回 `true` 表示新出现的地址
    pub(crate) fn push_device(&mut self, address: u64, name: String) -> bool {
        let now = Instant::now();
        self.device_seen.insert(address, now);
        match self.devices.iter().position(|(a, _)| *a == address) {
            Some(i) => {
                // 已存在也要挪到末尾，否则"最后一条 = 最新可见"的约定被打破（用户和脚本都按它挑设备）
                let (_, old) = self.devices.remove(i);
                self.devices
                    .push((address, if name.is_empty() { old } else { name }));
                false
            }
            None => {
                self.devices.push((address, name));
                true
            }
        }
    }

    /// 摘掉超过 [`DEVICE_TTL`] 没再广播的设备：RPA 地址不会自己消失，不过期就等于无限堆积历史设备
    pub(crate) fn sweep_devices(&mut self) -> usize {
        let now = Instant::now();
        let before = self.devices.len();
        self.devices.retain(|(a, _)| {
            self.device_seen
                .get(a)
                .is_some_and(|t| now.duration_since(*t) < DEVICE_TTL)
        });
        let dropped = before - self.devices.len();
        if dropped > 0 {
            // 同步清时间戳，避免 device_seen 单向膨胀
            let live: Vec<u64> = self.devices.iter().map(|(a, _)| *a).collect();
            self.device_seen.retain(|a, _| live.contains(a));
        }
        dropped
    }

    /// 记录一条通知：`key_hash` 非 0 且同应用视为**同一条通知的更新**（聊天类应用反复推同 key，
    /// 直接新增就刷屏）→ 就地替换标题/正文/时间并**保持原有位置**；其余插到最前
    pub(crate) fn push_notification(&mut self, item: NotificationItem) {
        if item.key_hash != 0 {
            if let Some(old) = self
                .notifications
                .iter_mut()
                .find(|n| n.key_hash == item.key_hash && n.package == item.package)
            {
                // 回复定位一并替换：聊天类应用常在"同一条通知"的下一次推送里才挂上 RemoteInput，
                // 只换正文不换入口，用户就永远等不到那个回复框。
                *old = item;
                return;
            }
        }
        self.notifications.insert(0, item);
        self.notifications.truncate(MAX_NOTIFICATIONS);
    }

    pub(crate) fn push_file_task(&mut self, item: FileTaskView) {
        self.file_tasks.insert(0, item);
        self.file_tasks.truncate(MAX_FILE_TASKS);
    }

    pub(crate) fn update_file_task(&mut self, name: &str, direction: u8, percent: u8, state: &str) {
        let now = std::time::Instant::now();
        if let Some(t) = self
            .file_tasks
            .iter_mut()
            .find(|t| t.direction == direction && t.name == name)
        {
            // 速度：按"百分比增量 × 总大小 ÷ 实际用时"算瞬时值再指数平滑，不平滑每 200ms 跳一次数字
            if t.size > 0 {
                if let Some(prev_at) = t.prev_at {
                    let dt = now.duration_since(prev_at).as_secs_f64();
                    if dt >= 0.2 {
                        let delta = percent as i16 - t.prev_percent as i16;
                        if delta > 0 {
                            let bytes = t.size as f64 * delta as f64 / 100.0;
                            let inst = (bytes / dt) / 1024.0;
                            t.speed_kbps = if t.speed_kbps == 0 {
                                inst as u32
                            } else {
                                ((t.speed_kbps as f64) * 0.6 + inst * 0.4) as u32
                            };
                        }
                        t.prev_percent = percent;
                        t.prev_at = Some(now);
                    }
                } else {
                    t.prev_percent = percent;
                    t.prev_at = Some(now);
                }
            }
            let changed = t.percent != percent || t.state != state;
            t.percent = percent;
            t.state = state.to_string();
            if changed {
                self.ui_rev += 1;
            }
            return;
        }
        self.ui_rev += 1;
        self.push_file_task(FileTaskView {
            name: name.to_string(),
            direction,
            percent,
            state: state.to_string(),
            size: 0,
            speed_kbps: 0,
            prev_percent: percent,
            prev_at: Some(now),
        });
    }

    pub(crate) fn set_file_task_size(&mut self, name: &str, direction: u8, size: u64) {
        if let Some(t) = self
            .file_tasks
            .iter_mut()
            .find(|t| t.direction == direction && t.name == name)
        {
            if t.size != size {
                t.size = size;
                self.ui_rev += 1;
            }
        }
    }

    pub(crate) fn remember_bound_device(&mut self, fingerprint: &str, name: &str) {
        if let Some((_, n)) = self
            .bound_devices
            .iter_mut()
            .find(|(fp, _)| fp == fingerprint)
        {
            if !name.is_empty() {
                *n = name.to_string();
            }
            return;
        }
        self.bound_devices
            .push((fingerprint.to_string(), name.to_string()));
    }

    pub(crate) fn push_error(&mut self, msg: String) {
        if let Some(first) = self.errors.first_mut() {
            if first.msg == msg {
                first.count += 1;
                return;
            }
        }
        self.errors.insert(0, ErrorRow { msg, count: 1 });
        self.errors.truncate(MAX_ERRORS);
    }

    pub(crate) fn push_toast(&mut self, title: String, text: String) {
        if self.pending_toasts.len() >= MAX_PENDING_TOASTS {
            self.pending_toasts.remove(0);
        }
        self.pending_toasts.push((title, text));
    }

    pub(crate) fn focused_input_mut(&mut self) -> Option<&mut String> {
        match self.input_focus {
            FOCUS_SEND_PATH => Some(&mut self.send_path_input),
            FOCUS_MANUAL_IP => Some(&mut self.manual_ip_input),
            FOCUS_REPLY => Some(&mut self.reply_input),
            _ => None,
        }
    }

    /// 收到一条回复回执：对上号就把结果落成用户看得见的一句话。
    /// 对不上号也要留痕 —— 那说明手机回了一条我们没在等的东西，静默吞掉是最难查的形态。
    pub(crate) fn apply_reply_ack(&mut self, reply_id: u32, ok: bool, error: &str) -> bool {
        let Some(idx) = self
            .reply_pending
            .iter()
            .position(|(id, _)| *id == reply_id)
        else {
            return false;
        };
        self.reply_pending.remove(idx);
        let text = if ok {
            "已发送".to_string()
        } else if error.trim().is_empty() {
            "回复失败".to_string()
        } else {
            error.trim().to_string()
        };
        self.reply_hint = Some((text, ok, std::time::Instant::now()));
        // 回执到达不产生任何 Windows 消息：不 bump 序号，回复结果就一直停在旧值直到用户点一下窗口
        self.ui_rev += 1;
        true
    }

    /// 收掉等不到回执的回复（对端版本不认识这条请求、或链路中途掉了）。
    /// 返回清掉的条数：worker 只在真有事时去敲一次重绘。
    pub(crate) fn expire_reply_timeouts(&mut self, now: std::time::Instant) -> usize {
        let stale: Vec<u32> = self
            .reply_pending
            .iter()
            .filter(|(_, sent_at)| now - *sent_at > REPLY_ACK_TIMEOUT)
            .map(|(id, _)| *id)
            .collect();
        let n = stale.len();
        for id in stale {
            self.apply_reply_ack(id, false, "手机没有回应（请把手机端 LinkX 升到最新版）");
        }
        n
    }

    /// 收起通知页的回复条：撤目标、清输入、把键盘焦点交回窗口。**两处收口都只走这里**
    /// （用户再点一次那条、手机报来那条通知已消失）。
    /// 收焦点这一步不能省：输入框已经不画了，留着 `FOCUS_REPLY` 会让用户接下来的每一次敲键
    /// 都写进一个屏幕上看不见的缓冲
    pub(crate) fn close_reply_bar(&mut self) {
        self.reply_target = None;
        self.reply_input.clear();
        if self.input_focus == FOCUS_REPLY {
            self.input_focus = FOCUS_NONE;
        }
    }

    /// 手机报来「这条通知已经不在了」：**只撤回复入口，不删这一行** —— 正文还能复制，
    /// 电脑上的"最近通知"不是通知栏的镜像。留着入口就是等用户点出一次失败，所以顺手
    /// 把正在编辑的那条输入框也收掉。返回是否有改动（没改动就不必敲重绘）。
    pub(crate) fn mark_notification_gone(&mut self, package: &str, tag: &str, id: i32) -> bool {
        let hit = |pkg: &str, t: &str, n: i32| pkg == package && t == tag && n == id;
        let mut changed = false;
        for item in &mut self.notifications {
            if hit(&item.package, &item.tag, item.notification_id) && item.can_reply {
                item.can_reply = false;
                changed = true;
            }
        }
        if self
            .reply_target
            .as_ref()
            .is_some_and(|t| hit(&t.package, &t.tag, t.notification_id))
        {
            self.close_reply_bar();
            changed = true;
        }
        if changed {
            self.ui_rev += 1;
        }
        changed
    }
}

pub(crate) fn new_shared() -> SharedState {
    Arc::new(Mutex::new(load_into(UiState::default())))
}

fn load_into(mut st: UiState) -> UiState {
    let s = settings::load();
    st.toast_enabled = s.toast_enabled;
    st.toast_show_content = s.toast_show_content;
    st.clip_sync = s.clip_sync;
    st.auto_connect = s.auto_connect;
    st.theme = s.theme;
    st.debug_enabled = s.debug_enabled;
    st.close_behavior = s.close_behavior;
    // 开机自启动先按 ini 那份记忆摆出来，进设置页时再由 `autostart::observe()` 用注册表真值覆盖
    st.autostart = s.autostart;
    st.autostart_is_ours = s.autostart;
    crate::features::load_wanted(&mut st, &s);
    // **就在下一行**把"想要的状态"固化成"本次加载的状态"：必须早于 worker 启动
    // 与任何模块初始化，否则会出现"设置说关了、模块照样起来"
    crate::features::init(&st);
    if !s.inbox.trim().is_empty() {
        st.inbox_dir = s.inbox;
    }
    st
}

/// 设计走查预览态：环境变量 `LINKX_UI_PREVIEW` 触发（sas（默认）/paired/album/identity），
/// 无 BLE 硬件时也用**真实版式**渲染"有数据"的各页。只在 debug 构建里编进去，交付包里
/// 设了也不生效。**不参与真机链路**：预览模式下 main.rs 不启动 BLE worker。
/// 走查辅助（同样只在预览态生效）：`LINKX_UI_PAGE` 指定初始页签（钳到 `TAB_ABOUT`），
/// `LINKX_THEME=light|dark|system` 覆盖主题。
#[cfg(debug_assertions)]
pub(crate) fn preview_shared() -> Option<SharedState> {
    let mode = std::env::var("LINKX_UI_PREVIEW").ok()?;
    if mode.is_empty() {
        return None;
    }
    let mut st = load_into(UiState::default());
    if let Ok(page) = std::env::var("LINKX_UI_PAGE") {
        if let Ok(n) = page.trim().parse::<usize>() {
            st.active_tab = n.min(crate::render::TAB_ABOUT);
        }
    }
    if let Ok(t) = std::env::var("LINKX_THEME") {
        st.theme = match t.trim().to_ascii_lowercase().as_str() {
            "light" => Theme::Light,
            "dark" => Theme::Dark,
            _ => Theme::System,
        };
    }
    st.devices = vec![
        (0x5C_F3_6D_21_4A_88, "Pixel 8 Pro".to_string()),
        (0x8A_11_02_7E_C4_9B, "小米 14 Ultra".to_string()),
        (0x1C_2A_B3_55_D0_77, "Galaxy S23".to_string()),
    ];
    st.selected = Some(0x5C_F3_6D_21_4A_88);
    st.peer_name = "Pixel 8 Pro".to_string();
    st.peer_os = 1;
    st.local_fp = "9c4b7e02a1f83d65".to_string();
    st.notifications = vec![
        NotificationItem {
            package: "com.tencent.mm".to_string(),
            title: "张伟".to_string(),
            text: "晚上七点老地方碰面，记得把那份合同带上，我这边已经和其他人确认过时间了"
                .to_string(),
            ts_ms: 1_790_000_000_000,
            key_hash: 0x51AB_0001, // 同 key 的后续推送应就地合并
            ..Default::default()
        },
        NotificationItem {
            package: "com.android.mms".to_string(),
            title: "10690018".to_string(),
            text: String::new(), // 敏感来源不同步正文 → 界面应显示"（内容未同步）"
            ts_ms: 1_789_999_940_000,
            key_hash: 0,
            ..Default::default()
        },
        NotificationItem {
            package: "com.microsoft.office.outlook".to_string(),
            title: "GitHub".to_string(),
            text: "[linkx] PR 已合并".to_string(),
            ts_ms: 1_789_999_880_000,
            key_hash: 0,
            ..Default::default()
        },
        NotificationItem {
            package: "com.netease.cloudmusic".to_string(),
            title: String::new(), // 无标题 → 应显示"(无标题)"
            text: "正在播放：夜曲".to_string(),
            ts_ms: 1_789_999_820_000,
            key_hash: 0x7C22_0002,
            ..Default::default()
        },
    ];
    st.clip_in =
        "https://example.com/docs/linkx/protocol/v1?section=channel-binding&rev=2026-09-24"
            .to_string();
    st.clip_out = "hello from pc".to_string();
    st.clip_sync = true;
    st.errors = vec![ErrorRow {
        msg: "[BLE 初始化失败: 0x80040154]".to_string(),
        count: 1,
    }];
    st.tcp_ready = true;
    st.battery = Some(BatteryView {
        level: 88,
        charging: false,
        at_ms: 1_790_000_000_000,
    });
    st.peer_lan_ip = "192.168.1.23".to_string();
    st.manual_ip_input = "192.168.1.23".to_string();
    st.send_path_input = "C:\\Users\\linkx\\Desktop\\季度汇报.pptx".to_string();
    st.inbox_dir = "C:\\Users\\linkx\\Downloads\\LinkX".to_string();
    st.file_tasks = vec![
        FileTaskView {
            name: "季度汇报.pptx".to_string(),
            direction: TASK_DIR_SEND,
            percent: 62,
            state: "发送中".to_string(),
            size: 0,
            speed_kbps: 0,
            prev_percent: 0,
            prev_at: None,
        },
        FileTaskView {
            name: "IMG_20260924_193012.jpg".to_string(),
            direction: TASK_DIR_RECV,
            percent: 100,
            state: "已完成".to_string(),
            size: 0,
            speed_kbps: 0,
            prev_percent: 0,
            prev_at: None,
        },
        FileTaskView {
            name: "架构评审纪要.docx".to_string(),
            direction: TASK_DIR_RECV,
            percent: 18,
            state: "接收中".to_string(),
            size: 0,
            speed_kbps: 0,
            prev_percent: 0,
            prev_at: None,
        },
    ];
    st.bound_devices = vec![
        ("3fa76f6244743c27".to_string(), "Pixel 8 Pro".to_string()),
        ("b091d4e77a2c10ff".to_string(), "小米 14 Ultra".to_string()),
    ];
    if mode == "paired" {
        st.conn_state = linkx_session::engine::state_code::PAIRED;
        st.paired = true;
        st.peer_fp = Some("3fa76f6244743c27".to_string());
    } else if mode == "album" {
        // 走查态：缩略图用 **WIC 现造的真 JPEG**，截图验的是「解码 → DIB → StretchBlt」这条生产路径本身
        st.conn_state = linkx_session::engine::state_code::PAIRED;
        st.paired = true;
        st.tcp_ready = true;
        st.peer_fp = Some("3fa76f6244743c27".to_string());
        st.feat_album = true;
        // 按"相册开着"重新固化一次，否则 settings.ini 里关着相册时这页连导航项都不出现
        crate::features::init(&st);
        let palette = [
            0x2E_7D_5B_u32,
            0xB5_4A_2A,
            0x2A_5B_B5,
            0x8A_8F_98,
            0xD4_A8_3C,
            0x4B_2E_7D,
        ];
        st.album.per_page = 12;
        st.album.total = 12;
        const VIDEO_DUR: [i64; 3] = [23_450, 633_000, 3_723_456];
        st.album.items = (0..12u64)
            .map(|i| {
                let video = i % 4 == 2;
                AlbumItemView {
                    id: 1000 + i,
                    name: if video {
                        format!("VID_{i:04}.mp4")
                    } else {
                        format!("IMG_{:04}.jpg", 2026 + i)
                    },
                    size_bytes: 1_800_000 + i as i64 * 420_000,
                    mtime_ms: 1_790_000_000_000 - i as i64 * 3_600_000,
                    width: 4032,
                    height: 3024,
                    kind: if video { ALBUM_KIND_VIDEO } else { 0 },
                    duration_ms: if video {
                        VIDEO_DUR[(i / 4) as usize % 3]
                    } else {
                        0
                    },
                }
            })
            .collect();
        st.album.visible_count = 12;
        // 前 6 张给真图；第 7 张给坏数据（走"解码失败"态）；第 8 张给手机侧失败；
        // 第 9 张停在载入中；最后 3 张连队列都没排上 —— 一格界面该有的五种状态同屏可见
        for (i, rgb) in palette.iter().enumerate() {
            if let Ok(jpeg) = crate::wic::solid_jpeg(180, 120, *rgb) {
                st.album
                    .put_thumb(1000 + i as u64, ThumbSlot::Ready(Arc::new(jpeg)));
            }
        }
        st.album
            .put_thumb(1006, ThumbSlot::Ready(Arc::new(vec![0u8; 24])));
        st.album.put_thumb(
            1007,
            ThumbSlot::Failed("手机侧无法为这张生成缩略图（原图已不在本机）".to_string()),
        );
        st.album.mark_thumb_pending(1008);
    } else if mode == "identity" {
        // 走查态：对端同名新身份待决策（重装场景）
        st.conn_state = linkx_session::engine::state_code::REPAIRED;
        st.peer_name = "Pixel 8 Pro".to_string();
        st.peer_os = 1;
        st.identity_change = Some(IdentityChangeView {
            name: "Pixel 8 Pro".to_string(),
            old_fp: "3fa76f6244743c27".to_string(),
            new_fp: "ad8ee9d83dec93d8".to_string(),
        });
    } else {
        st.conn_state = linkx_session::engine::state_code::SAS_COMPARE;
        st.sas = Some(482_913);
    }
    Some(Arc::new(Mutex::new(st)))
}

/// 交付构建里没有预览态：`LINKX_UI_PREVIEW` 设了也不生效，走查请装调试包。
#[cfg(not(debug_assertions))]
pub(crate) fn preview_shared() -> Option<SharedState> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(package: &str, title: &str, text: &str, key: u32) -> NotificationItem {
        NotificationItem {
            package: package.to_string(),
            title: title.to_string(),
            text: text.to_string(),
            ts_ms: 1,
            key_hash: key,
            ..Default::default()
        }
    }

    #[test]
    fn notification_merge_also_refreshes_the_reply_handle() {
        // 聊天应用常在"同一条通知"的下一次推送里才挂上 RemoteInput：只换正文不换入口，
        // 用户就永远等不到回复框
        let mut st = UiState::default();
        st.push_notification(item("com.a", "旧标题", "旧正文", 7));
        st.push_notification(NotificationItem {
            key_hash: 7,
            package: "com.a".to_string(),
            can_reply: true,
            notification_id: 42,
            reply_result_key: "key_reply".to_string(),
            ..Default::default()
        });
        assert_eq!(st.notifications.len(), 1);
        assert!(st.notifications[0].can_reply);
        assert_eq!(st.notifications[0].notification_id, 42);
    }

    #[test]
    fn a_dismissed_notification_loses_its_reply_entry_but_stays_listed() {
        let mut st = UiState::default();
        st.push_notification(NotificationItem {
            package: "com.a".into(),
            title: "张三".into(),
            text: "在吗".into(),
            key_hash: 7,
            tag: "sms".into(),
            notification_id: 42,
            can_reply: true,
            reply_action_index: 0,
            reply_result_key: "key_reply".into(),
            ..Default::default()
        });
        st.reply_target = Some(ReplyTarget {
            package: "com.a".into(),
            tag: "sms".into(),
            notification_id: 42,
            action_index: 0,
            result_key: "key_reply".into(),
        });
        st.reply_input = "马上到".into();
        let rev = st.ui_rev;
        assert!(st.mark_notification_gone("com.a", "sms", 42));
        assert_eq!(
            st.ui_rev,
            rev + 1,
            "撤了入口就得敲重绘，否则屏幕上还留着按钮"
        );
        assert!(!st.notifications[0].can_reply);
        assert_eq!(st.notifications[0].text, "在吗", "正文要留着，复制仍然有用");
        assert!(st.reply_target.is_none() && st.reply_input.is_empty());
        assert!(
            !st.mark_notification_gone("com.a", "sms", 42),
            "重复上报不算改动"
        );
        st.notifications[0].can_reply = true;
        assert!(
            !st.mark_notification_gone("com.a", "其他 tag", 42)
                && !st.mark_notification_gone("com.a", "sms", 43),
            "定位三元组差一项也不许误伤别的通知"
        );
        assert!(st.notifications[0].can_reply);
    }

    #[test]
    fn reply_ack_lands_only_on_the_request_it_belongs_to() {
        use std::time::Instant;
        let mut st = UiState::default();
        st.reply_pending.push((3, Instant::now()));
        // 对不上号的回执：不写提示，返回 false 让调用方出声
        assert!(!st.apply_reply_ack(9, true, ""));
        assert!(st.reply_hint.is_none());
        assert!(st.apply_reply_ack(3, false, "该应用不支持回复"));
        assert!(st.reply_pending.is_empty());
        let (text, ok, _) = st.reply_hint.clone().unwrap();
        assert_eq!(text, "该应用不支持回复");
        assert!(!ok);
        // 成功就三个字，失败但手机什么也没说时不能显示空白
        st.reply_pending.push((4, Instant::now()));
        assert!(st.apply_reply_ack(4, true, ""));
        assert_eq!(st.reply_hint.as_ref().unwrap().0, "已发送");
        st.reply_pending.push((5, Instant::now()));
        assert!(st.apply_reply_ack(5, false, "   "));
        assert_eq!(st.reply_hint.as_ref().unwrap().0, "回复失败");
    }

    #[test]
    fn a_reply_that_never_gets_an_ack_expires_instead_of_hanging() {
        use std::time::{Duration, Instant};
        let mut st = UiState::default();
        let sent = Instant::now() - REPLY_ACK_TIMEOUT - Duration::from_secs(1);
        st.reply_pending.push((1, sent));
        st.reply_pending.push((2, Instant::now()));
        assert_eq!(st.expire_reply_timeouts(Instant::now()), 1);
        assert_eq!(st.reply_pending.len(), 1, "还在时限内的不得被收掉");
        assert_eq!(
            st.reply_hint.as_ref().unwrap().0,
            "手机没有回应（请把手机端 LinkX 升到最新版）"
        );
        // 全清完就不再报"有事"，worker 不会每轮空敲重绘
        st.reply_pending.clear();
        assert_eq!(st.expire_reply_timeouts(Instant::now()), 0);
    }

    #[test]
    fn notification_same_key_merges_in_place() {
        let mut st = UiState::default();
        st.push_notification(item("com.a", "旧标题", "旧正文", 7));
        st.push_notification(item("com.b", "另一条", "别的内容", 8));
        st.push_notification(item("com.a", "新标题", "新正文", 7));
        assert_eq!(st.notifications.len(), 2, "同 key 不得新增条目");
        assert_eq!(st.notifications[0].package, "com.b");
        assert_eq!(st.notifications[1].title, "新标题");
        assert_eq!(st.notifications[1].text, "新正文");
    }

    #[test]
    fn notification_without_key_or_cross_package_inserts() {
        let mut st = UiState::default();
        st.push_notification(item("com.a", "t1", "x", 0));
        st.push_notification(item("com.a", "t2", "y", 0));
        assert_eq!(st.notifications.len(), 2);
        assert_eq!(st.notifications[0].title, "t2");
        st.push_notification(item("com.c", "t3", "z", 7));
        st.push_notification(item("com.d", "t4", "w", 7));
        assert_eq!(st.notifications.len(), 4);
    }

    #[test]
    fn notification_list_is_capped() {
        let mut st = UiState::default();
        for i in 0..(MAX_NOTIFICATIONS + 5) {
            st.push_notification(item("com.a", &format!("t{i}"), "x", 0));
        }
        assert_eq!(st.notifications.len(), MAX_NOTIFICATIONS);
    }

    #[test]
    fn file_task_list_capped_and_updated() {
        let mut st = UiState::default();
        st.update_file_task("a.bin", TASK_DIR_SEND, 10, "发送中");
        st.update_file_task("a.bin", TASK_DIR_SEND, 90, "已完成");
        assert_eq!(st.file_tasks.len(), 1);
        assert_eq!(st.file_tasks[0].percent, 90);
        assert_eq!(st.file_tasks[0].state, "已完成");
        for i in 0..(MAX_FILE_TASKS + 3) {
            st.push_file_task(FileTaskView {
                name: format!("f{i}.bin"),
                direction: TASK_DIR_RECV,
                percent: 0,
                state: "接收中".to_string(),
                size: 0,
                speed_kbps: 0,
                prev_percent: 0,
                prev_at: None,
            });
        }
        assert_eq!(st.file_tasks.len(), MAX_FILE_TASKS);
    }

    /// 进度写进 `UiState` 必须推进 `ui_rev`（否则"要点一下窗口才动"）；
    /// 反向也要成立：重复写同值不得推进，不然每轮都重绘 = 白烧 CPU
    #[test]
    fn progress_change_bumps_ui_rev_and_idempotent_writes_do_not() {
        let mut st = UiState::default();
        let base = st.ui_rev;
        st.update_file_task("a.bin", TASK_DIR_SEND, 0, "发送中");
        assert_eq!(st.ui_rev, base + 1, "新建任务应推进一次");

        let before = st.ui_rev;
        st.update_file_task("a.bin", TASK_DIR_SEND, 0, "发送中");
        assert_eq!(st.ui_rev, before, "同值重复写不该推进（否则会空转重绘）");

        st.update_file_task("a.bin", TASK_DIR_SEND, 1, "发送中");
        assert_eq!(st.ui_rev, before + 1, "百分比变了要重绘");
        st.update_file_task("a.bin", TASK_DIR_SEND, 1, "已完成");
        assert_eq!(st.ui_rev, before + 2, "状态文字变了同样要重绘");

        st.set_file_task_size("a.bin", TASK_DIR_SEND, 4096);
        assert_eq!(
            st.ui_rev,
            before + 3,
            "登记大小让速度从无到有，也算画面变了"
        );
        st.set_file_task_size("a.bin", TASK_DIR_SEND, 4096);
        assert_eq!(st.ui_rev, before + 3, "大小没变不该再推进");
    }

    #[test]
    fn devices_expire_and_stay_newest_last() {
        let mut st = UiState::default();
        assert!(st.push_device(0xAAAA, "Redmi".into()));
        assert!(st.push_device(0xBBBB, "Redmi".into()));
        assert_eq!(st.devices.len(), 2, "同一台手机的两个 RPA 地址会并存");

        assert!(!st.push_device(0xAAAA, "Redmi".into()));
        assert_eq!(st.devices.len(), 2);
        assert_eq!(
            st.devices.last().unwrap().0,
            0xAAAA,
            "再次可见的设备必须排在最后"
        );

        let stale = Instant::now() - DEVICE_TTL - std::time::Duration::from_secs(1);
        st.device_seen.insert(0xAAAA, stale);
        assert_eq!(st.sweep_devices(), 1, "应摘掉恰好一条过期设备");
        assert_eq!(st.devices.len(), 1);
        assert_eq!(st.devices[0].0, 0xBBBB);
        assert!(
            !st.device_seen.contains_key(&0xAAAA),
            "过期设备的时间戳必须一并清掉，否则 map 单向膨胀"
        );
    }

    #[test]
    fn bound_devices_dedupe_by_fingerprint() {
        let mut st = UiState::default();
        st.remember_bound_device("abc", "Pixel");
        st.remember_bound_device("abc", "Pixel 8 Pro");
        st.remember_bound_device("def", "Mi 14");
        assert_eq!(st.bound_devices.len(), 2);
        assert_eq!(st.bound_devices[0].1, "Pixel 8 Pro");
    }

    #[test]
    fn identity_change_prompt_is_one_shot() {
        let mut st = UiState::default();
        assert!(st.identity_change.is_none());
        st.identity_change = Some(IdentityChangeView {
            name: "Pixel".to_string(),
            old_fp: "aaaa".to_string(),
            new_fp: "bbbb".to_string(),
        });
        st.identity_change = None;
        assert!(st.identity_change.is_none());
    }

    /// 关闭行为的三条入口（点 X、弹窗按钮、回车/Esc）都走这一个决策口：分成两处判，
    /// "勾了记住但按了取消"这类组合迟早各说各话
    #[test]
    fn deciding_close_without_a_choice_follows_the_saved_behavior() {
        for (b, want) in [
            (CloseBehavior::Ask, CloseAction::Ask),
            (CloseBehavior::Minimize, CloseAction::Minimize),
            (CloseBehavior::Exit, CloseAction::Exit),
        ] {
            let d = decide_close(b, false, None);
            assert_eq!((d.action, d.persist), (want, None), "{b:?} 时不该动设置");
            // 已经记住过行为的用户，勾没勾"记住"都不该再被打扰
            assert_eq!(decide_close(b, true, None).action, want);
        }
    }

    #[test]
    fn remembering_only_persists_a_real_choice() {
        let d = decide_close(CloseBehavior::Ask, true, Some(CloseChoice::Minimize));
        assert_eq!(
            (d.action, d.persist),
            (CloseAction::Minimize, Some(CloseBehavior::Minimize))
        );
        let d = decide_close(CloseBehavior::Ask, true, Some(CloseChoice::Exit));
        assert_eq!(
            (d.action, d.persist),
            (CloseAction::Exit, Some(CloseBehavior::Exit))
        );
        // 没勾"记住"就只执行这一次，设置一个字都不改
        for c in [CloseChoice::Minimize, CloseChoice::Exit] {
            assert_eq!(
                decide_close(CloseBehavior::Ask, false, Some(c)).persist,
                None
            );
        }
        // 取消时没有任何值得记住的选择：勾了也不许把行为写成别的值
        let d = decide_close(CloseBehavior::Ask, true, Some(CloseChoice::Cancel));
        assert_eq!((d.action, d.persist), (CloseAction::Stay, None));
    }

    #[test]
    fn focus_selects_expected_input() {
        let mut st = UiState::default();
        assert!(st.focused_input_mut().is_none());
        st.input_focus = FOCUS_SEND_PATH;
        st.focused_input_mut().unwrap().push_str("C:\\a.txt");
        assert_eq!(st.send_path_input, "C:\\a.txt");
        assert!(st.manual_ip_input.is_empty());
        st.input_focus = FOCUS_MANUAL_IP;
        st.focused_input_mut().unwrap().push_str("10.0.0.5");
        assert_eq!(st.manual_ip_input, "10.0.0.5");
        assert_eq!(st.send_path_input, "C:\\a.txt");
    }
    #[test]
    fn paired_badge_follows_the_live_link_not_the_latch() {
        use linkx_session::engine::state_code;
        let mut st = UiState {
            paired: true,
            ..Default::default()
        };
        for dead in [
            state_code::DISCOVER,
            state_code::RECONNECTING,
            state_code::CLOSED,
        ] {
            st.conn_state = dead;
            assert!(!st.link_paired(), "会话状态 {dead} 下不该显示已配对");
        }
        st.conn_state = state_code::PAIRED;
        assert!(st.link_paired(), "引擎报 PAIRED 才算已配对");
    }

    #[test]
    fn paired_badge_also_needs_live_traffic() {
        use linkx_session::engine::state_code;
        let mut st = UiState {
            paired: true,
            conn_state: state_code::PAIRED,
            ..Default::default()
        };
        assert!(st.link_paired(), "刚收到过消息 + 引擎 PAIRED = 真的连着");

        st.rx_silent_ms = LINK_SILENCE_MS;
        assert!(
            !st.link_paired(),
            "整段静默窗口没收到一帧，不该再声称已配对"
        );
        assert_eq!(st.link_lie_reason(), "已经收不到手机的消息");

        st.rx_silent_ms = 0;
        st.identity_change = Some(IdentityChangeView {
            name: "22041216C".to_string(),
            old_fp: "aaaa".to_string(),
            new_fp: "bbbb".to_string(),
        });
        assert!(!st.link_paired(), "身份待确认期间链路不算可用");
        assert_eq!(st.link_lie_reason(), "对端身份已变化，等你确认");
    }

    #[test]
    fn refresh_requeues_failed_thumbs_only() {
        let mut v = AlbumView::default();
        v.put_thumb(1, ThumbSlot::Ready(std::sync::Arc::new(vec![0xFFu8])));
        v.put_thumb(2, ThumbSlot::Failed("链路超时".to_string()));
        v.mark_thumb_pending(3);
        v.drop_failed_thumbs();
        assert!(
            v.slot(1).is_some(),
            "成功格保留：不该因为别格失败而重新生成"
        );
        assert!(
            v.slot(2).is_none(),
            "失败格清空，下一次 needs_thumb 才会为真"
        );
        assert!(v.slot(3).is_some(), "在途格不动");
        assert!(v.needs_thumb(2), "清空后这一格该重新排队");
        assert!(!v.needs_thumb(1), "已有图的不该再排队");
    }

    /// 红线口径：缩略图缓存的占用必须**与相册大小无关**，且正在看的那一页一张都不许被抽走
    #[test]
    fn thumb_cache_is_bounded_by_bytes_not_by_luck() {
        let mut v = AlbumView {
            items: (0..2000u64)
                .map(|i| AlbumItemView {
                    id: i,
                    name: format!("IMG_{i}.jpg"),
                    size_bytes: 4_000_000,
                    mtime_ms: 0,
                    width: 4000,
                    height: 3000,
                    kind: 0,
                    duration_ms: 0,
                })
                .collect(),
            visible_count: 24,
            ..Default::default()
        };
        let one = vec![7u8; 200 * 1024];
        for i in 0..2000u64 {
            v.put_thumb(i, ThumbSlot::Ready(std::sync::Arc::new(one.clone())));
        }
        assert!(
            v.thumb_bytes() <= ALBUM_THUMB_BYTES_MAX,
            "灌到第 2000 张时缓存 {} MB，已经越过 {} 字节那条线",
            v.thumb_bytes() / 1024 / 1024,
            ALBUM_THUMB_BYTES_MAX
        );
        assert!(v.thumbs.len() <= ALBUM_THUMB_MAX, "条数线也要守住");
        for i in 0..24u64 {
            assert!(v.slot(i).is_some(), "可见页第 {i} 张被抽走了：会闪白格");
        }
    }

    #[test]
    fn cancel_drag_drops_route_and_marks_arrival_as_expected() {
        let mut v = AlbumView::default();
        v.plan_fetch(&[7], "C:/nonexistent".to_string(), AlbumPurpose::Drag);
        assert!(v.routes.contains_key(&7), "预取登记了落盘路由");
        assert!(v.set_drag_progress(7, 4096, 100_000), "进度先记上");
        v.cancel_drag(7);
        assert!(!v.routes.contains_key(&7), "路由撤了：晚到的原图不会落盘");
        assert!(v.drag_req.is_empty(), "还没发出的命令也撤了");
        assert!(v.drag_progress_of(7).is_none(), "撤单后圆环要停，不能空转");
        assert!(v.take_cancelled(7), "到货时该认出这是本机撤的");
        assert!(!v.take_cancelled(7), "只认一次，之后不吞别人的到货");
        assert!(!v.take_cancelled(8), "没撤过的不能顺手认掉");
    }

    #[test]
    fn cancel_drag_deletes_landed_payload() {
        let path = std::env::temp_dir().join(format!(
            "linkx-cancel-drag-{}-{}.jpg",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, b"fake").unwrap();
        let mut v = AlbumView {
            drag_result: Some((9, Ok(path.display().to_string()))),
            ..Default::default()
        };
        v.cancel_drag(9);
        assert!(!path.exists(), "已落地的预取原图必须删掉");
        assert!(v.drag_result.is_none());
    }

    /// 大文件那一次"没等到"的拖出必须把取回**留下**，第二次拖才可能立刻成功；
    /// 留下的份数也必须有上限，否则临时目录会变成视频仓库
    #[test]
    fn drag_cache_keeps_two_and_evicts_oldest() {
        let mk = |n: &str| {
            let p = std::env::temp_dir().join(format!("linkx-drag-{}-{n}", std::process::id()));
            std::fs::write(&p, b"x").unwrap();
            p
        };
        let (a, b, c) = (mk("a"), mk("b"), mk("c"));
        let s = |p: &std::path::Path| p.display().to_string();
        let mut v = AlbumView::default();
        v.keep_drag(1, s(&a));
        v.keep_drag(2, s(&b));
        assert_eq!(v.drag_ready_path(1), Some(s(&a)));
        assert_eq!(v.drag_ready_path(2), Some(s(&b)));
        v.keep_drag(3, s(&c));
        assert!(
            !a.exists(),
            "超出 ALBUM_DRAG_KEEP 的最旧一份要当场删掉：留着的是一整段视频，不是几 KB"
        );
        assert_eq!(v.drag_ready_path(1), None);
        assert_eq!(v.drag_ready_path(3), Some(s(&c)));
        let _ = std::fs::remove_file(&b);
        assert_eq!(
            v.drag_ready_path(2),
            None,
            "路径还在表里但文件没了，必须算没命中"
        );
        let d = mk("d");
        v.keep_drag(4, s(&d));
        assert_eq!(
            v.drag_ready.len(),
            ALBUM_DRAG_KEEP,
            "只剩活着的两份（3 和 4）：文件已消失的那条不该占名额"
        );
        assert_eq!(v.drag_ready_path(3), Some(s(&c)));
        assert_eq!(v.drag_ready_path(4), Some(s(&d)));
        // 已留下的那一份不许被"点一下选中"的撤单顺手删掉（撤点击只管没打算拖的预取）
        v.drag_result = Some((3, Ok(s(&c))));
        v.routes.insert(3, AlbumPurpose::Drag);
        v.cancel_drag(3);
        assert!(
            c.exists(),
            "已经取回并留着的载荷归 drag_ready 管，撤点击不该删它"
        );
        let _ = std::fs::remove_file(&c);
        let _ = std::fs::remove_file(&d);
    }
}
