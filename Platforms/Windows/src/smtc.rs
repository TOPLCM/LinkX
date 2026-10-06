//! 系统媒体控制卡（SMTC）：把手机正在播放的东西投影到 Windows 自己的媒体界面
//! （音量面板那张卡、锁屏、游戏栏），卡上的按钮再变回播放指令发给手机。
//!
//! 只做"投影"：卡片由系统绘制，本模块喂元数据与进度并接按钮事件。**不投封面图** —— 未打包应用
//! 能不能让系统跨进程读到缩略图，从来没有在真人眼前证实过一次，两轮真机都是空白，所以这条
//! 链路整个撤掉了（手机不再压图上传，省每首歌几十 KB 与一块解码位图）。按钮是系统固定的
//! 那几个，**没有自定义按钮、也没有音量按钮**，所以不把快进快退挪用成音量 —— 卡片画那个图标，
//! 点下去就得干那件事。
//!
//! 全在 UI 线程（消息循环那条），按钮事件由系统派发回本线程。锁纪律见 [`tick`]。

use std::cell::{Cell, RefCell};
use std::os::raw::c_void;
use std::time::{Duration, Instant};

use windows::core::HSTRING;
use windows::Foundation::TypedEventHandler;
use windows::Media::{
    MediaPlaybackStatus, MediaPlaybackType, SystemMediaTransportControls,
    SystemMediaTransportControlsButtonPressedEventArgs,
    SystemMediaTransportControlsTimelineProperties,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::WinRT::{ISystemMediaTransportControlsInterop, RoGetActivationFactory};

use crate::state::{media_track_key, UiState};

/// 一轮更新所需的全部数据（持锁时拷出来，之后不再碰 UiState）
struct Update {
    hwnd_raw: isize,
    title: String,
    artist: String,
    album: String,
    playing: bool,
    pos_sec: i64,
    duration_sec: i64,
    published: String,
}

struct Card {
    controls: SystemMediaTransportControls,
    published: String,
    pos_sec: i64,
}

thread_local! {
    static CARD: RefCell<Option<Card>> = const { RefCell::new(None) };
    /// 建卡失败后的下次重试时刻：失败多半是暂时的，永久放弃会让卡片整轮不开
    static NEXT_TRY: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// 建不出来时多久再试一次（每 33 ms 撞一次只会刷满日志）
const RETRY: Duration = Duration::from_secs(5);

/// 按下按钮后卡片按"用户要的目标态"显示的最长时间；超过就回落到手机报回来的真状态。
/// 必须长过手机送回真状态的最坏路径（常规采样 3 秒一轮 + 指令后追采 1.5 秒，见 `MediaControl.SETTLE_DELAYS_MS`）。
pub(crate) const OPTIMISTIC_WINDOW: Duration = Duration::from_secs(5);

/// UI 定时器入口：有播放状态就投影，没有就把卡片撤下。
///
/// 锁纪律：先取名句再动手。写成 `match take_update(&arc.lock().unwrap()) { ... }` 会让这把锁
/// 活到整个 match 结束，于是每个跨进程调用都压着全进程状态锁 —— 重入 `CARD` 的可变借用还能
/// 把进程 abort 掉。
pub(crate) fn tick() {
    let Some(arc) = crate::window::shared_state() else {
        return;
    };
    let update = take_update(&mut arc.lock().unwrap());
    match update {
        Some(u) => apply(&u),
        None => clear(),
    }
}

/// 该不该投影、投影什么。判据用 `link_paired()`（此刻真的说得上话）而不是"曾经配对过"：
/// 手机被强杀后引擎还会停在 Paired 几十秒，跟着 `media` 走就留下一张"正在播放"的僵尸卡。
/// 卡片该显示"在放"还是"暂停"：手机上报的那个值，加上刚按下那条指令想要的那个值。
/// 过期或已经对上的记账当场清掉 —— 留着它，用户直接在手机上按播放会被这条旧记录压住。
/// 单独成函数是为了让这条判据能绕开"媒体模块开没开"那个进程级全局来测。
fn effective_playing(st: &mut UiState) -> bool {
    let reported = st.media.as_ref().is_some_and(|m| m.playing);
    match st.media_cmd_want {
        Some((want, at)) if at.elapsed() < OPTIMISTIC_WINDOW && reported != want => want,
        Some(_) => {
            st.media_cmd_want = None;
            reported
        }
        None => reported,
    }
}

fn take_update(st: &mut UiState) -> Option<Update> {
    if !st.link_paired() || !crate::features::enabled(crate::features::Module::MediaControl) {
        return None;
    }
    let playing = effective_playing(st);
    let m = st.media.as_ref()?;
    Some(Update {
        hwnd_raw: st.hwnd_raw,
        title: m.title.clone(),
        artist: m.artist.clone(),
        album: m.album.clone(),
        playing,
        pos_sec: m.position_ms / 1000,
        duration_sec: m.duration_ms / 1000,
        published: format!(
            "{}|{}|{}",
            media_track_key(&m.package, &m.title, &m.artist),
            m.album,
            playing
        ),
    })
}

fn apply(u: &Update) {
    if NEXT_TRY.with(|t| t.get().is_some_and(|at| Instant::now() < at)) {
        return;
    }
    CARD.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            match build(u.hwnd_raw) {
                Some(card) => {
                    *slot = Some(card);
                    NEXT_TRY.with(|t| t.set(None));
                }
                None => {
                    NEXT_TRY.with(|t| t.set(Some(Instant::now() + RETRY)));
                    return;
                }
            }
        }
        let Some(card) = slot.as_mut() else {
            return;
        };
        run(card, u);
    });
}

/// 把这一轮的差异写进卡片。换歌或播放态翻转才整轮重建；只有进度在走时走轻路径。
fn run(card: &mut Card, u: &Update) {
    if card.published != u.published {
        let _ = card.controls.SetIsEnabled(true);
        let _ = card.controls.SetPlaybackStatus(if u.playing {
            MediaPlaybackStatus::Playing
        } else {
            MediaPlaybackStatus::Paused
        });
        if let Ok(up) = card.controls.DisplayUpdater() {
            let _ = up.SetType(MediaPlaybackType::Music);
            if let Ok(mp) = up.MusicProperties() {
                let _ = mp.SetTitle(&HSTRING::from(u.title.as_str()));
                let _ = mp.SetArtist(&HSTRING::from(u.artist.as_str()));
                let _ = mp.SetAlbumTitle(&HSTRING::from(u.album.as_str()));
            }
            let note = match up.Update() {
                Ok(()) => "ok".to_string(),
                Err(e) => e.to_string(),
            };
            debuglog::log!(
                if note == "ok" {
                    debuglog::Level::Info
                } else {
                    debuglog::Level::Warn
                },
                "ui",
                "smtc.publish",
                &[("note", &note)]
            );
        }
        // 记账落在 DisplayUpdater 之外：连"拿不到更新器"这种失败也不该变成每帧重敲系统
        card.published.clone_from(&u.published);
    }
    if card.pos_sec != u.pos_sec {
        write_timeline(&card.controls, u.pos_sec, u.duration_sec);
        card.pos_sec = u.pos_sec;
    }
}

fn write_timeline(controls: &SystemMediaTransportControls, pos_sec: i64, duration_sec: i64) {
    let Ok(t) = SystemMediaTransportControlsTimelineProperties::new() else {
        return;
    };
    use windows::Foundation::TimeSpan;
    // TimeSpan.Duration 的单位是 100 纳秒。秒数必须夹住：这两个数来自对端的帧，
    // 不夹就是 `i64::MAX` 一乘就翻成负数时间轴（dev 直接 abort，release 静默画错）。24 小时
    // 是"再长也不是歌"的上限。
    let secs = |s: i64| TimeSpan {
        Duration: s.clamp(0, 86_400) * 10_000_000,
    };
    let _ = t.SetStartTime(secs(0));
    let _ = t.SetPosition(secs(pos_sec));
    let _ = t.SetMinSeekTime(secs(0));
    let _ = t.SetEndTime(secs(duration_sec));
    let _ = t.SetMaxSeekTime(secs(duration_sec));
    let _ = controls.UpdateTimelineProperties(&t);
}

/// 撤卡：断链、解绑、关掉"媒体控制"都走到这里，否则系统里会留一张僵尸卡。
///
/// 控件对象**留着不复建**：`GetForWindow` 给的是系统按窗口缓存的同一个实例，撤卡时丢掉、
/// 下次再注册 `ButtonPressed` 就会攒出多个活回调 —— 按一次暂停，我们下发多条指令。
pub(crate) fn clear() {
    CARD.with(|slot| {
        if let Some(card) = slot.borrow_mut().as_mut() {
            let _ = card.controls.SetIsEnabled(false);
            card.published.clear();
            card.pos_sec = -1;
        }
    });
}

fn build(hwnd_raw: isize) -> Option<Card> {
    if hwnd_raw == 0 {
        return None;
    }
    let sender = crate::window::shared_state().cloned()?;
    unsafe {
        // 与 wic.rs / dialog.rs 同一口径：只初始化、不配对卸载
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let factory = RoGetActivationFactory::<ISystemMediaTransportControlsInterop>(
            &HSTRING::from("Windows.Media.SystemMediaTransportControls"),
        )
        .ok()?;
        let controls: SystemMediaTransportControls = factory
            .GetForWindow(HWND(hwnd_raw as *mut c_void))
            .map_err(|e| {
                debuglog::log!(
                    debuglog::Level::Warn,
                    "ui",
                    "smtc.get_for_window",
                    &[("err", &e.to_string())]
                )
            })
            .ok()?;
        let _ = controls.SetIsEnabled(true);
        let _ = controls.SetIsPlayEnabled(true);
        let _ = controls.SetIsPauseEnabled(true);
        let _ = controls.SetIsStopEnabled(true);
        let _ = controls.SetIsNextEnabled(true);
        let _ = controls.SetIsPreviousEnabled(true);
        // 只点亮"确实是那件事"的按钮：快进/快退/录音没有可信的手机语义，就不画出来
        let _ = controls.SetIsRecordEnabled(false);
        let _ = controls.SetIsFastForwardEnabled(false);
        let _ = controls.SetIsRewindEnabled(false);

        let handler = TypedEventHandler::new(
            move |_c: &Option<SystemMediaTransportControls>,
                  e: &Option<SystemMediaTransportControlsButtonPressedEventArgs>| {
                let Some(button) = e.as_ref().and_then(|e| e.Button().ok()) else {
                    return Ok(());
                };
                // 系统给的是目标态（Play 与 Pause 是两个按钮），就按目标态下发，不拿
                // PLAY_PAUSE 兜底：卡片显示什么，点下去就变成什么。
                let action = match button.0 {
                    act::PLAY => Some(act::PLAY),
                    act::PAUSE => Some(act::PAUSE),
                    act::STOP => Some(act::STOP),
                    act::NEXT => Some(act::NEXT),
                    act::PREV => Some(act::PREV),
                    _ => None,
                };
                if let Some(a) = action {
                    let mut st = sender.lock().unwrap();
                    // 播放/暂停/停止有明确目标态；上一首/下一首不改变"在不在放"，不动这条
                    match a {
                        act::PLAY => st.media_cmd_want = Some((true, Instant::now())),
                        act::PAUSE | act::STOP => st.media_cmd_want = Some((false, Instant::now())),
                        _ => {}
                    }
                    st.media_cmd_req = Some((a, 0, 0));
                    debuglog::log!(
                        debuglog::Level::Info,
                        "ui",
                        "smtc.button",
                        &[("action", &a.to_string())]
                    );
                }
                Ok(())
            },
        );
        let _ = controls.ButtonPressed(&handler);
        Some(Card {
            controls,
            published: String::new(),
            pos_sec: -1,
        })
    }
}

/// `MediaCommand.Action`（`Proto/linkx/v1/media.proto`）里卡片按钮用得上的几个值
mod act {
    pub(super) const PLAY: i32 = 1;
    pub(super) const PAUSE: i32 = 2;
    pub(super) const NEXT: i32 = 3;
    pub(super) const PREV: i32 = 4;
    pub(super) const STOP: i32 = 5;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MediaView;
    use linkx_session::engine::state_code;

    fn view() -> MediaView {
        MediaView {
            package: "music.example".into(),
            title: "虎口脱险".into(),
            artist: "老狼".into(),
            album: "恋恋风尘".into(),
            playing: true,
            position_ms: 12_000,
            duration_ms: 245_000,
            speed_x100: 100,
            volume: 40,
        }
    }

    fn state(paired: bool, conn: u8, silent_ms: u64) -> UiState {
        let mut st = UiState::default();
        st.paired = paired;
        st.conn_state = conn;
        st.rx_silent_ms = silent_ms;
        st.media = Some(view());
        st
    }

    /// 卡片只投影"此刻真连着的那台手机在放什么"。判据用 `link_paired()` 而不是锁存的
    /// `paired`：手机被强杀后引擎还会停在 Paired 几十秒，跟着 `media` 走就留下一张
    /// "正在播放"的僵尸卡。
    ///
    /// `ACTIVE` 是进程级全局（功能模块开关），并行用例里不能去改它，所以这里只在
    /// "媒体模块恰好是开的"时候断言；被别的测试关了就跳过，假失败比不测更糟。
    #[test]
    fn card_follows_the_live_link_not_the_latched_pairing() {
        if !crate::features::enabled(crate::features::Module::MediaControl) {
            return;
        }
        let mut live = state(true, state_code::PAIRED, 0);
        assert!(
            take_update(&mut live).is_some(),
            "真连着且在放歌：该投影到卡片"
        );
        let u = take_update(&mut live).unwrap();
        assert_eq!(
            (u.title.as_str(), u.artist.as_str(), u.album.as_str()),
            ("虎口脱险", "老狼", "恋恋风尘")
        );
        assert_eq!((u.pos_sec, u.duration_sec), (12, 245), "秒数换算");

        // 链路一旦说不上话，卡片必须当场撤下（解绑、引擎退出 Paired、静默超时三种表现）
        for dead in [
            state(false, state_code::PAIRED, 0),
            state(true, state_code::CLOSED, 0),
            state(true, state_code::PAIRED, 999_999),
        ] {
            let mut dead = dead;
            assert!(
                take_update(&mut dead).is_none(),
                "链路不活着时不该继续投影（paired={} conn={} silent={}ms）",
                dead.paired,
                dead.conn_state,
                dead.rx_silent_ms
            );
        }
    }

    /// 卡片上按下暂停后要**停在暂停**直到手机把真状态报回来：手机常规采样 3 秒一轮，中间必然
    /// 有几轮旧上报到达，跟着它们重画就是真机报的"按下去半秒又跳回播放"。
    #[test]
    fn a_fresh_command_holds_the_card_until_the_phone_agrees() {
        let mut st = state(true, state_code::PAIRED, 0);
        st.media.as_mut().unwrap().playing = true;
        st.media_cmd_want = Some((false, std::time::Instant::now()));
        assert!(!effective_playing(&mut st), "刚按了暂停：卡片停在暂停");
        assert!(
            st.media_cmd_want.is_some(),
            "窗口内不能提前松手，否则旧上报会顶上来"
        );

        st.media.as_mut().unwrap().playing = false;
        assert!(!effective_playing(&mut st));
        assert!(st.media_cmd_want.is_none(), "手机对上了就松手");

        // 手机没执行（播放器不配合）：窗口过后回落到真状态。要**超出**窗口，压在边界上会偶发翻脸
        st.media.as_mut().unwrap().playing = true;
        st.media_cmd_want = Some((
            false,
            std::time::Instant::now() - OPTIMISTIC_WINDOW - Duration::from_secs(1),
        ));
        assert!(effective_playing(&mut st), "过期之后以手机上报为准");
        assert!(st.media_cmd_want.is_none(), "过期即清，不留悬挂记录");
    }
}
