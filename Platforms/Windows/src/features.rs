//! 运行期功能开关：用户不需要某个功能时可以在设置里**单独关掉**，重启后不再加载相关组件。
//! 关键设计：**"想要的状态"与"已加载的状态"必须分开** —— `UiState::feat_*`（落盘进 `Settings`）是用户想要的，
//! 本模块的 [`ACTIVE`] 是本次启动实际加载的，只在进程启动时固化一次；两者不一致时 UI 显示"待重启生效"。
//! 之所以不当场热卸载：模块一旦建了线程、注册了 WinRT 回调、握了 socket，"就地卸载"要处理所有在途回调的悬垂问题，
//! 那比多占一点内存严重得多；用户要的语义本来也是"重启后不加载"。只用一个 `AtomicU8` 位图，不引入特性开关库。

use std::sync::atomic::{AtomicU8, Ordering};

use crate::settings::Settings;
use crate::state::UiState;

/// 可独立开关的功能模块。新增模块只需：在这里加一个变体并在 [`ALL`] 里登记，在 [`UiState`]/[`Settings`] 里加 `feat_*`
/// 字段并在三处映射各补一行，再在该模块**初始化入口**用 [`enabled`] 早退 —— UI 遍历 [`ALL`]，开关、提示与弹窗自动带上它
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Module {
    Notifications,
    Clipboard,
    FileTransfer,
    MediaControl,
    Album,
}

pub(crate) const ALL: [Module; 5] = [
    Module::Notifications,
    Module::Clipboard,
    Module::FileTransfer,
    Module::MediaControl,
    Module::Album,
];

impl Module {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Module::Notifications => "通知同步",
            Module::Clipboard => "剪贴板同步",
            Module::FileTransfer => "文件传输",
            Module::MediaControl => "媒体控制",
            Module::Album => "相册互传",
        }
    }

    pub(crate) fn about(self) -> &'static str {
        match self {
            Module::Notifications => "接收手机通知并在电脑侧提醒",
            Module::Clipboard => "两端复制的文本互相同步",
            Module::FileTransfer => "局域网收发文件，并承载 TCP 通道与设备发现",
            Module::MediaControl => "在电脑上查看手机正在放什么并控制播放",
            Module::Album => "在电脑上浏览手机照片，多选导出或单张拖出",
        }
    }

    /// 关掉之后用户会失去什么——写进确认弹窗，别让人误关核心能力
    pub(crate) fn consequence(self) -> &'static str {
        match self {
            Module::Notifications => "手机通知将不再推送到电脑",
            Module::Clipboard => "两端复制内容将不再互相同步",
            Module::FileTransfer => "将无法收发文件，局域网通道（TCP/UDP 发现）也不会启动",
            Module::MediaControl => "电脑上将不再显示和控制手机的播放",
            Module::Album => "「相册」页将从导航里消失，照片预览、导出与拖出都不可用",
        }
    }

    const fn bit(self) -> u8 {
        match self {
            Module::Notifications => 1 << 0,
            Module::Clipboard => 1 << 1,
            Module::FileTransfer => 1 << 2,
            Module::MediaControl => 1 << 3,
            Module::Album => 1 << 4,
        }
    }

    /// 稳定的 ASCII 标识：控制面 `/state` 的 JSON 键与 `/action/feature?module=` 的取值。刻意不用中文名或位序 ——
    /// 那会让自动化脚本随文案/顺序一起碎。`allow(dead_code)`：唯一调用方是 `agent-debug` 控制面，交付构建不带该 feature
    #[allow(dead_code)]
    pub(crate) fn key(self) -> &'static str {
        match self {
            Module::Notifications => "notifications",
            Module::Clipboard => "clipboard",
            Module::FileTransfer => "file_transfer",
            Module::MediaControl => "media_control",
            Module::Album => "album",
        }
    }

    #[allow(dead_code)]
    pub(crate) fn from_key(s: &str) -> Option<Module> {
        ALL.iter().find(|m| m.key() == s.trim()).copied()
    }

    pub(crate) fn wanted(self, st: &UiState) -> bool {
        match self {
            Module::Notifications => st.feat_notifications,
            Module::Clipboard => st.feat_clipboard,
            Module::FileTransfer => st.feat_file_transfer,
            Module::MediaControl => st.feat_media,
            Module::Album => st.feat_album,
        }
    }

    pub(crate) fn set_wanted(self, st: &mut UiState, v: bool) {
        match self {
            Module::Notifications => st.feat_notifications = v,
            Module::Clipboard => st.feat_clipboard = v,
            Module::FileTransfer => st.feat_file_transfer = v,
            Module::MediaControl => st.feat_media = v,
            Module::Album => st.feat_album = v,
        }
    }
}

pub(crate) fn load_wanted(st: &mut UiState, s: &Settings) {
    for m in ALL {
        m.set_wanted(
            st,
            match m {
                Module::Notifications => s.feat_notifications,
                Module::Clipboard => s.feat_clipboard,
                Module::FileTransfer => s.feat_file_transfer,
                Module::MediaControl => s.feat_media,
                Module::Album => s.feat_album,
            },
        );
    }
}

/// 已定义模块占用的位。**必须拿它去遮 `ACTIVE`**：`ACTIVE` 的初值是"全开"，而全开只等于**已定义**的位。
/// 不遮的话未 init 时 `restart_pending`（整字节比较）会判"待重启"、`changes`（逐模块比较）却列不出条目 —— "没有提示条但那块区域仍可点"
const ALL_BITS: u8 = (1 << ALL.len()) - 1;

/// 本次启动**已加载**的模块位图。启动时由 [`init`] 写入，之后只读。
static ACTIVE: AtomicU8 = AtomicU8::new(ALL_BITS); // 未 init 前保守地全开

pub(crate) fn capture(st: &UiState) -> u8 {
    let mut b = 0u8;
    for m in ALL {
        if m.wanted(st) {
            b |= m.bit();
        }
    }
    b
}

/// 进程启动时调用一次：把"想要的状态"固化为"已加载的状态"。调用点必须**早于 worker 启动与任何模块初始化**
/// （见 `state::load_into`），否则会出现"设置说关了、模块照样起来"
pub(crate) fn init(st: &UiState) {
    ACTIVE.store(capture(st), Ordering::Relaxed);
}

pub(crate) fn enabled(m: Module) -> bool {
    active_bits() & m.bit() != 0
}

/// 是否有任何模块的"已加载"与"想要"不一致 → UI 需要显示"待重启生效"
pub(crate) fn restart_pending(st: &UiState) -> bool {
    active_bits() != capture(st)
}

pub(crate) fn changes(st: &UiState) -> Vec<(Module, bool)> {
    ALL.iter()
        .filter(|m| enabled(**m) != m.wanted(st))
        .map(|m| (*m, m.wanted(st)))
        .collect()
}

pub(crate) fn active_bits() -> u8 {
    ACTIVE.load(Ordering::Relaxed) & ALL_BITS
}

pub(crate) fn modules_of(bits: u8) -> Vec<&'static str> {
    ALL.iter()
        .filter(|m| bits & m.bit() != 0)
        .map(|m| m.label())
        .collect()
}

/// 本进程当前工作集（MB），功能页与 `/state` 用它把"现在占多少"摊给用户看。
/// 取不到返回 `None` 而不是 0：0 会被读成"这个模块真省内存"，而实测关掉模块并不可见地省内存 —— 诊断数字出错时只能显示"取不到"
pub(crate) fn working_set_mb() -> Option<u32> {
    #[cfg(windows)]
    {
        use windows::Win32::System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
        };
        use windows::Win32::System::Threading::GetCurrentProcess;

        // cb 必须显式填结构体大小，否则 API 直接拒绝（这是它唯一的入参校验）
        let mut counters = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        let ok = unsafe {
            K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                std::ptr::addr_of_mut!(counters).cast(),
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            )
        };
        if !ok.as_bool() {
            return None;
        }
        Some((counters.WorkingSetSize / (1024 * 1024)) as u32)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(n: bool, c: bool, f: bool) -> UiState {
        let mut st = UiState::default();
        st.feat_notifications = n;
        st.feat_clipboard = c;
        st.feat_file_transfer = f;
        // 媒体控制与相册恒为关：既有的三位用例就仍然只测那三位（各自的位由逐模块点亮的用例覆盖）
        st.feat_media = false;
        st.feat_album = false;
        st
    }

    /// 每个模块必须独占一位，且**只**由 `UiState` 里对应的那个字段决定 —— 映射写错时关一个功能会连带关掉另一个（UI 上看不出来）
    #[test]
    fn each_module_maps_to_exactly_one_bit_and_one_field() {
        let seen: Vec<u8> = ALL.iter().map(|m| m.bit()).collect();
        assert_eq!(
            seen.iter().collect::<std::collections::HashSet<_>>().len(),
            ALL.len(),
            "每个模块必须占独立位"
        );
        assert_eq!(ALL.len(), 5, "u8 位图最多 8 位；加模块前先确认还有空位");
        for m in ALL {
            assert_eq!(
                Module::from_key(m.key()),
                Some(m),
                "{m:?} 的控制面标识应能反查"
            );
            let mut st = state(false, false, false);
            st.feat_media = false;
            st.feat_album = false;
            m.set_wanted(&mut st, true);
            assert_eq!(capture(&st), m.bit(), "{m:?} 的位值与字段对不上");
        }
        assert_eq!(
            modules_of((1 << ALL.len()) - 1).len(),
            ALL.len(),
            "全部加载应报出全部模块"
        );
    }

    /// 控制面传进来的标识必须**精确**匹配：宽松前缀匹配会让 `file` 之类悄悄命中 `file_transfer`，把用户没打算关的功能关掉
    #[test]
    fn module_keys_are_exact() {
        assert!(Module::from_key("").is_none());
        assert!(Module::from_key("clip").is_none());
        assert!(Module::from_key("Notifications").is_none(), "大小写敏感");
        assert_eq!(Module::from_key(" clipboard "), Some(Module::Clipboard));
    }

    #[test]
    fn toggling_one_module_does_not_disturb_others() {
        let st = state(true, false, true);
        assert_eq!(capture(&st), 0b101);
        assert_eq!(modules_of(0b101), vec!["通知同步", "文件传输"]);
        assert_eq!(modules_of(0b010), vec!["剪贴板同步"]);
        assert_eq!(modules_of(0b000), Vec::<&str>::new());
    }

    /// "已加载"与"想要"分离：这是整个设计的核心，也是弹窗文案的依据。**本模块只有这一个测试碰 `ACTIVE`**：
    /// 它是进程级全局，再写一个并行的用例就会互相踩
    #[test]
    fn init_freezes_active_and_only_restart_pending_moves() {
        let all_on = state(true, true, true);
        init(&all_on);
        assert!(!restart_pending(&all_on), "全开且全加载时不该报待重启");
        let wanted_off = state(false, true, true);
        assert!(enabled(Module::Notifications), "未重启前必须仍然加载");
        assert!(restart_pending(&wanted_off), "状态不一致时应提示待重启");
        assert_eq!(
            changes(&wanted_off),
            vec![(Module::Notifications, false)],
            "待重启条目应恰好一条"
        );
        // 用户真的重启了
        init(&wanted_off);
        assert!(!enabled(Module::Notifications));
        assert!(enabled(Module::Clipboard), "其他模块不受牵连");
        assert!(!restart_pending(&wanted_off), "重启后提示应消失");
        assert!(changes(&wanted_off).is_empty());
        // 反过来：只加载了两项时，"全开"同样是不一致状态（要能提示重启补齐）
        assert!(restart_pending(&all_on));
    }
}
