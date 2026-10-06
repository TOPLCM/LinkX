//! Windows 系统通知卡（toast）：托盘气泡结构上没有按钮位，「复制验证码」与「回复这条」两颗只能走它。
//!
//! 三个前提缺一不可：未打包应用要靠开始菜单那条快捷方式背上 `System.AppUserModel.ID` 才有身份
//! （缺它 `Show()` 照样成功、屏幕上却什么都没有）；按钮回话要靠安装器注册到 HKCU 的 `linkx://`，
//! 点击时系统另起 `linkx.exe "linkx://…"`，由第二实例把 URI 转交给运行中的实例（见 `ipc`）。
//! 卡上不放输入框：`hint-inputId` 只把按钮摆到输入框旁边，取内容要靠打包应用的激活回调，免安装的
//! exe 收到的会是字面量 `{reply}`。安全：本机任何进程都能调用它，所以按钮参数只带一次性随机令牌
//! —— 弹卡时发放、用过即废、超时作废（见 `UiState::take_toast_token`）。

use std::cell::Cell;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use windows::core::{GUID, HSTRING, PCWSTR, PROPVARIANT};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IPropertyStore, SHGetPropertyStoreFromParsingName, GETPROPERTYSTOREFLAGS, PROPERTYKEY,
};
use windows::UI::Notifications::{
    ToastFailedEventArgs, ToastNotification, ToastNotificationManager,
};

use crate::state::{ReplyTarget, SharedState, ToastCard, ToastTarget};

/// 应用身份。系统是从开始菜单那条 .lnk 上把身份读回去的，两边不一致就等于没登记
pub(crate) const AUMID: &str = "LinkX.LinkX";
/// 协议前缀，与安装器注册的那条 `linkx` 键值同源
pub(crate) const SCHEME: &str = "linkx://";
/// toast 被系统拒过一次之后，隔多久之内不再白试 WinRT、直接走气泡
const COOLDOWN_MS: u64 = 300_000;
/// `FMTID_ShellLegacy` + `PID_SYSTEM_APPUSERMODEL_ID`
const AUMID_FMTID: u128 = 0x9f4c_2855_9f79_4b39_a8d0_e1d4_2de1_d5f3;

/// 身份有没有登记上；没登记时 `Show()` 也返回成功却什么都不弹，所以这个开关挡在 `publish` 前面
static REGISTERED: AtomicBool = AtomicBool::new(false);
/// 冷却截止时间（本进程毫秒数，见 `tick_ms`）：回报来自通知平台的线程，故不用 `thread_local`
static COOLED_UNTIL: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static COM_OWNED: Cell<bool> = const { Cell::new(false) };
}

/// 安装器建的开始菜单快捷方式；没勾那项、或直接跑解包出来的 exe 时就没有
fn start_menu_link() -> Option<PathBuf> {
    let root = std::env::var_os("APPDATA")?;
    let lnk = Path::new(&root)
        .join(r"Microsoft\Windows\Start Menu\Programs\LinkX")
        .join("LinkX.lnk");
    lnk.is_file().then_some(lnk)
}

/// 把 AUMID 写进那条快捷方式。本该由安装器做，但 WiX v5 的 `<Shortcut>` 不收 `<Properties>`
/// （实测 WIX0005），所以每次启动补一次：升级、重建快捷方式都能自己好回来
fn claim_identity(lnk: &Path) -> bool {
    let wide: Vec<u16> = lnk.as_os_str().encode_wide().chain([0]).collect();
    let key = PROPERTYKEY {
        fmtid: GUID::from_u128(AUMID_FMTID),
        pid: 5,
    };
    let value = PROPVARIANT::from(AUMID);
    ensure_com();
    let store: windows::core::Result<IPropertyStore> = unsafe {
        SHGetPropertyStoreFromParsingName(
            PCWSTR(wide.as_ptr()),
            None,
            GETPROPERTYSTOREFLAGS(2), // GPS_READWRITE；1 是只读，写不进去
        )
    };
    match store {
        Ok(store) => {
            let written = unsafe { store.SetValue(&key, &value) }.is_ok();
            written && unsafe { store.Commit() }.is_ok()
        }
        Err(_) => false,
    }
}

/// 由 `main` 在启动时调用一次：给我们的开始菜单快捷方式补上身份。
/// 判"能不能弹"不许用 `ToastNotifier.Setting()`：实测它会在确实弹得出卡的身份上报 `Disabled`(0)，
/// 拿它降级等于亲手关掉能用的卡，所以只进日志当线索；真没弹出来由 [`watch_failed`] 回报
pub(crate) fn register_identity() {
    let ok = start_menu_link().is_some_and(|lnk| claim_identity(&lnk));
    REGISTERED.store(ok, Ordering::Release);
    let setting = ok.then(ask_setting).unwrap_or(-1);
    debuglog::log!(
        debuglog::Level::Info,
        "ui",
        "toast.identity",
        &[("ok", &ok.to_string()), ("setting", &setting.to_string())]
    );
}

/// 系统给这个身份的通知设置（0=Disabled…3=Default），读不到算 -1；只当线索，不当判据
fn ask_setting() -> i32 {
    ensure_com();
    let id = HSTRING::from(AUMID);
    ToastNotificationManager::CreateToastNotifierWithId(&id)
        .and_then(|n| n.Setting())
        .map(|s| s.0)
        .unwrap_or(-1)
}

/// 与 `dialog.rs`/`wic.rs` 同一口径：只初始化、不卸载——重复初始化返回的 `S_FALSE` 也算 `is_ok()`，
/// 照它配对 Uninitialize 会把同线程别的模块（OLE、WIC）那层一起减掉
fn ensure_com() {
    let already = COM_OWNED.with(|c| c.get());
    if !already && unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok() {
        COM_OWNED.with(|c| c.set(true));
    }
}

/// XML 转义：`&` `<` `>` `"` 换实体，控制字符直接丢（正文里出现 `<` `&` 是常态，不转义就是整张卡
/// 静默不显示）。NUL 之类只能丢而不是转义：XML 1.0 连 `&#0;` 都不认，一条怪通知不该让后面
/// 五分钟所有卡片都退成气泡。
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let raw = c as u32;
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // XML 里唯一合法的三个控制字符，留着才能保住通知正文的换行
            '\t' | '\n' | '\r' => out.push(c),
            _ if raw < 0x20 || raw == 0xFFFE || raw == 0xFFFF => {}
            _ => out.push(c),
        }
    }
    out
}

/// 拼 toast XML。卡上没有输入框（见文件头），所以两颗按钮都只带令牌、不带用户文本
fn build_xml(card: &ToastCard, copy_token: u64, reply_token: u64) -> String {
    let mut actions = String::new();
    if let Some(code) = &card.copy {
        // `content` 是 XML **属性**，抽出来的验证码在这里过一遍
        actions.push_str(&format!(
            "<action content=\"复制验证码 {}\" activationType=\"protocol\" \
             arguments=\"{SCHEME}copy/{copy_token:016x}\"/>",
            esc(code)
        ));
    }
    if card.reply.is_some() {
        actions.push_str(&format!(
            "<action content=\"回复这条\" activationType=\"protocol\" \
             arguments=\"{SCHEME}reply/{reply_token:016x}\"/>"
        ));
    }
    // 带按钮的卡要多给十几秒：看清两颗按钮各是干什么的不能卡在 7 秒自动收起上
    let duration = if actions.is_empty() { "short" } else { "long" };
    format!(
        "<toast duration=\"{duration}\"><visual><binding template=\"ToastGeneric\">\
         <text>{}</text><text>{}</text></binding></visual><actions>{actions}</actions>\
         <audio silent=\"yes\"/></toast>",
        esc(&card.title),
        esc(&card.body)
    )
}

/// 本进程启动以来的毫秒（单调、跨线程可见）。冷却用它：回报来自通知平台的线程，
/// 本线程的 `thread_local` 时钟对方看不见
fn tick_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn in_cooldown() -> bool {
    COOLED_UNTIL.load(Ordering::Acquire) > tick_ms()
}

/// 5 分钟内不再白试 WinRT，直接走托盘气泡
fn cool_now() {
    COOLED_UNTIL.store(tick_ms() + COOLDOWN_MS, Ordering::Release);
}

/// 挂上系统的"这张卡没显示出来"回报。判"弹没弹"不能只看 `Show()` 的返回值：实测过
/// "`Show()` 成功 + 屏幕上什么都没有"。回调只写冷却开关与错误码，不含正文也不含口令。
fn watch_failed(toast: &ToastNotification) {
    let handler = TypedEventHandler::new(
        |_card: &Option<ToastNotification>, args: &Option<ToastFailedEventArgs>| {
            let code = args
                .as_ref()
                .and_then(|a| a.ErrorCode().ok())
                .map_or(0u32, |e| e.0 as u32);
            cool_now();
            debuglog::log!(
                debuglog::Level::Warn,
                "ui",
                "toast.failed",
                &[("code", &format!("0x{code:08X}"))]
            );
            Ok(())
        },
    );
    let _ = toast.Failed(&handler);
}

/// 弹一张卡。`false` = 当场被系统没收（没身份、XML 没读进去、`Show()` 报错），调用方回落气泡；
/// `true` 只代表"系统收下了"，没显示出来由 `Failed` 回报
fn show(card: &ToastCard, copy_token: u64, reply_token: u64) -> bool {
    let xml = build_xml(card, copy_token, reply_token);
    ensure_com();
    let Ok(doc) = XmlDocument::new() else {
        return false;
    };
    if doc.LoadXml(&HSTRING::from(&xml)).is_err() {
        debuglog::log!(debuglog::Level::Warn, "ui", "toast.xml.rejected", &[]);
        return false;
    }
    let Ok(toast) = ToastNotification::CreateToastNotification(&doc) else {
        return false;
    };
    watch_failed(&toast);
    let id = HSTRING::from(AUMID);
    let Ok(notifier) = ToastNotificationManager::CreateToastNotifierWithId(&id) else {
        return false;
    };
    notifier.Show(&toast).is_ok()
}

/// 一张卡的正门：能用系统 toast 就用 toast（带按钮），否则回落托盘气泡
pub(crate) fn publish(state: &SharedState, card: &ToastCard) -> bool {
    if REGISTERED.load(Ordering::Acquire) && !in_cooldown() {
        if show_now(state, card) {
            return true;
        }
        cool_now();
    }
    unsafe { crate::tray::show_balloon(&card.title, &card.body) }
}

/// 有对应动作才发令牌：`None` = 这颗按钮不存在，XML 里也就不会画它
fn issue<T>(
    state: &SharedState,
    src: &Option<T>,
    make: impl FnOnce(&T) -> ToastTarget,
) -> Option<u64> {
    let target = make(src.as_ref()?);
    let token = crate::state::random_token();
    state
        .lock()
        .unwrap()
        .issue_toast_token(token, target, Instant::now());
    Some(token)
}

/// 令牌在真要弹出去这一刻才发放：排队时被丢掉的卡片不该留下可用口令，弹不出去当场收回
fn show_now(state: &SharedState, card: &ToastCard) -> bool {
    let copy = issue(state, &card.copy, |code| {
        ToastTarget::CopyCode(code.clone())
    });
    let reply = issue(state, &card.reply, |target| {
        ToastTarget::Reply(target.clone())
    });
    let shown = show(card, copy.unwrap_or(0), reply.unwrap_or(0));
    if !shown {
        let tokens: Vec<u64> = [copy, reply].into_iter().flatten().collect();
        state.lock().unwrap().revoke_toast_tokens(tokens);
    }
    shown
}

/// 一条 `linkx://` 激活参数 →（动词，一次性令牌）。纯函数，好测；参数里没有用户文本（见文件头）
pub(crate) fn parse(uri: &str) -> Option<(&str, u64)> {
    let rest = uri.strip_prefix(SCHEME)?;
    let (verb, hex) = rest.rsplit_once('/')?;
    let token = u64::from_str_radix(hex, 16).ok()?;
    matches!(verb, "copy" | "reply").then_some((verb, token))
}

/// 处理第二实例转交来的按钮动作（在管道线程上，与 `ipc::deliver` 同一口径：改状态、再敲重绘）
pub(crate) fn handle_activation(state: &SharedState, uri: &str) {
    let Some((verb, token)) = parse(uri) else {
        reject(state, "parse", "收到一条格式不对的通知按钮请求，已忽略");
        return;
    };
    let target = state
        .lock()
        .unwrap()
        .take_toast_token(token, Instant::now());
    // 动词与口令必须成对命中：拿「复制」那条口令去走「回复」这条路，等于绕过令牌表
    match (verb, target) {
        ("copy", Some(ToastTarget::CopyCode(code))) => copy_back(state, code),
        ("reply", Some(ToastTarget::Reply(target))) => open_reply(state, target),
        (_, Some(_)) => reject(state, "verb", "这条通知的按钮和参数对不上，已忽略"),
        (_, None) => reject(state, "stale", "这条通知的操作已经过期或已经用过了"),
    }
}

fn copy_back(state: &SharedState, code: String) {
    let mut st = state.lock().unwrap();
    // 走与界面内「复制验证码」同一份防回声登记，否则这条会被当成用户新复制的内容再推回手机
    let Some(code) = crate::window::copy_locally(&mut st, Some(code)) else {
        return;
    };
    drop(st); // 跨进程剪贴板写不占着锁做
    crate::clipboard::set_text(&code);
    crate::say("[LinkX] 通知卡上按了「复制验证码」");
}

/// 卡上按了「回复这条」：选中这条通知、光标落到我们自己界面的回复框、把窗口摆回来。
/// 打字与发送都在回复条里完成——这颗按钮不替用户发任何东西
fn open_reply(state: &SharedState, target: ReplyTarget) {
    let mut st = state.lock().unwrap();
    let hwnd = st.hwnd_raw;
    st.reply_target = Some(target);
    st.input_focus = crate::state::FOCUS_REPLY;
    st.ui_rev += 1;
    crate::window::post_state_changed(hwnd);
    drop(st);
    crate::say("[LinkX] 通知卡上按了「回复这条」");
    crate::window::request_show_from_raw(hwnd);
}

fn reject(state: &SharedState, tag: &str, why: &str) {
    let mut st = state.lock().unwrap();
    st.push_error(why.to_string());
    st.ui_rev += 1;
    drop(st);
    // 只记一个短标签：URI 里带着一次性口令，写进日志就等于把它抄一份留在盘上
    debuglog::log!(debuglog::Level::Warn, "ui", "toast.reject", &[("why", tag)]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> ReplyTarget {
        ReplyTarget {
            package: "com.android.mms".into(),
            tag: "sms-code".into(),
            notification_id: 91,
            action_index: 0,
            result_key: "reply".into(),
        }
    }

    #[test]
    fn parse_reads_both_button_shapes() {
        assert_eq!(parse("linkx://copy/000000000000002a"), Some(("copy", 42)));
        assert_eq!(parse("linkx://reply/000000000000002a"), Some(("reply", 42)));
        // 认不出的都作废：动词不对、令牌不是十六进制、后面拖了别的东西、协议前缀是别人的
        assert!(parse("linkx://open/000000000000002a").is_none());
        assert!(parse("linkx://copy/not-hex").is_none());
        assert!(parse("linkx://reply/000000000000002a?t=马上到").is_none());
        assert!(parse("http://linkx/copy/2a").is_none());
    }

    #[test]
    fn build_xml_draws_only_the_buttons_a_card_has() {
        let plain = ToastCard::plain("文件已送达", "a.pdf：对端校验通过");
        let xml = build_xml(&plain, 1, 2);
        assert!(xml.contains("<actions></actions>"));
        assert!(xml.contains("duration=\"short\""));

        let full = ToastCard {
            title: "短信".into(),
            body: "验证码 1234".into(),
            copy: Some("1234".into()),
            reply: Some(target()),
        };
        let xml = build_xml(&full, 0xaa, 0xbb);
        assert!(xml.contains("复制验证码 1234"));
        assert!(xml.contains("linkx://copy/00000000000000aa"));
        assert!(xml.contains("回复这条"));
        assert!(xml.contains("linkx://reply/00000000000000bb"));
        // 卡上没有输入框：免安装应用拿不到它的内容，摆上去就是骗人点一下
        assert!(!xml.contains("<input"));
        assert!(xml.contains("duration=\"long\""));
        // 手机已经响过一遍了，电脑上再"叮"一次是双重打扰
        assert!(xml.ends_with("<audio silent=\"yes\"/></toast>"));
    }

    #[test]
    fn a_button_only_fires_for_the_token_that_was_issued_for_it() {
        let state: SharedState =
            std::sync::Arc::new(std::sync::Mutex::new(crate::state::UiState::default()));
        let target = target();
        let now = Instant::now();
        state
            .lock()
            .unwrap()
            .issue_toast_token(0x2a, ToastTarget::Reply(target.clone()), now);

        // 口令对不上：什么都不做，并且要告诉用户为什么没动静
        handle_activation(&state, "linkx://reply/00000000000000ff");
        assert_eq!(
            state.lock().unwrap().errors[0].msg,
            "这条通知的操作已经过期或已经用过了"
        );
        assert!(state.lock().unwrap().reply_target.is_none());

        // 命中：选中这条 + 光标落到回复框。发送那一步留给人在回复条里按
        handle_activation(&state, "linkx://reply/000000000000002a");
        let st = state.lock().unwrap();
        assert_eq!(st.reply_target.as_ref(), Some(&target));
        assert_eq!(st.input_focus, crate::state::FOCUS_REPLY);
        assert!(st.reply_req.is_none(), "卡上那颗按钮不许替用户按下发送");
        assert!(st.reply_input.is_empty());
        drop(st);

        // 用过即废：连点两次只有第一次有用
        handle_activation(&state, "linkx://reply/000000000000002a");
        assert_eq!(
            state.lock().unwrap().errors[0].msg,
            "这条通知的操作已经过期或已经用过了"
        );

        // 动词与口令不配对：拿「复制」那条口令去走「回复」这条路，换不到任何动作
        state
            .lock()
            .unwrap()
            .issue_toast_token(0x2b, ToastTarget::CopyCode("1234".into()), now);
        handle_activation(&state, "linkx://reply/000000000000002b");
        assert_eq!(
            state.lock().unwrap().errors[0].msg,
            "这条通知的按钮和参数对不上，已忽略"
        );
        assert_eq!(
            state.lock().unwrap().reply_target.as_ref(),
            Some(&target),
            "被拒的那次尝试不该动已经选中的那条"
        );
        // 这次失败的尝试已经把口令吃掉：换个动词再来一次也不给（不许留着口令等重试）
        handle_activation(&state, "linkx://copy/000000000000002b");
        assert_eq!(
            state.lock().unwrap().errors[0].msg,
            "这条通知的操作已经过期或已经用过了"
        );

        // 前缀或动词不对：连令牌都读不出来
        handle_activation(&state, "https://example.com/copy/2a");
        assert_eq!(
            state.lock().unwrap().errors[0].msg,
            "收到一条格式不对的通知按钮请求，已忽略"
        );
    }

    #[test]
    fn control_characters_never_reach_the_card_markup() {
        // 正文来自手机上的任意一条通知。NUL 这类字符 XML 1.0 根本不收，留着就是整张卡
        // 静默不显示；一条怪通知不该让后面五分钟的所有卡片都退成气泡
        let card = ToastCard::plain("验证码\u{0}1234", "带\u{7}响铃与\u{1}控制符");
        let xml = build_xml(&card, 0, 0);
        assert!(!xml.contains('\u{0}') && !xml.contains('\u{7}') && !xml.contains('\u{1}'));
        assert!(xml.contains("验证码1234"));
        // 换行合法、也是通知正文常见的排版，不能顺手吃掉
        let multiline = ToastCard::plain("标题", "第一行\n第二行");
        assert!(build_xml(&multiline, 0, 0).contains("第一行\n第二行"));
    }

    #[test]
    fn a_code_with_markup_stays_inside_its_attribute() {
        // 「复制验证码」那颗按钮的文字是 XML **属性**，验证码在这里也过一遍转义
        let card = ToastCard {
            title: "短信".into(),
            body: "验证码 1\"2<&3".into(),
            copy: Some("1\"2<&3".into()),
            reply: None,
        };
        let xml = build_xml(&card, 7, 8);
        assert!(xml.contains("content=\"复制验证码 1&quot;2&lt;&amp;3\""));
        assert!(!xml.contains("复制验证码 1\"2"));
    }

    #[test]
    fn escaping_keeps_markup_out_of_the_card() {
        let card = ToastCard::plain("<b>银行</b>", "点击 https://x?a=1&b=2 领取");
        let xml = build_xml(&card, 0, 0);
        assert!(xml.contains("&lt;b&gt;银行&lt;/b&gt;"));
        assert!(xml.contains("a=1&amp;b=2"));
        assert!(!xml.contains("<b>"));
    }
}
