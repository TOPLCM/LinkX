//! 用户设置持久化（`%APPDATA%\LinkX\settings.ini`）
//!
//! 极简 `key=value` 文本，不引第三方配置库（依赖最小化）。读/写全程容错：
//! 文件缺失或损坏一律回落默认值，**绝不让设置文件影响主流程**（与身份私钥同目录）。

use std::fs;

/// 界面主题偏好（默认跟随系统）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Theme {
    /// 跟随系统「应用模式」（Win10 1809+ / Win11 的浅色/深色）
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    fn as_str(self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }

    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "light" => Theme::Light,
            "dark" => Theme::Dark,
            _ => Theme::System,
        }
    }
}

/// 点标题栏关闭按钮时做什么（默认先问一句）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CloseBehavior {
    #[default]
    Ask,
    Minimize,
    Exit,
}

impl CloseBehavior {
    pub(crate) fn label(self) -> &'static str {
        match self {
            CloseBehavior::Ask => "询问",
            CloseBehavior::Minimize => "最小化到托盘",
            CloseBehavior::Exit => "退出程序",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            CloseBehavior::Ask => "ask",
            CloseBehavior::Minimize => "minimize",
            CloseBehavior::Exit => "exit",
        }
    }

    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "minimize" => CloseBehavior::Minimize,
            "exit" => CloseBehavior::Exit,
            _ => CloseBehavior::Ask,
        }
    }

    /// 设置页那一行点击后往哪个值走：显示与切换必须同源于这张表
    pub(crate) fn next(self) -> Self {
        match self {
            CloseBehavior::Ask => CloseBehavior::Minimize,
            CloseBehavior::Minimize => CloseBehavior::Exit,
            CloseBehavior::Exit => CloseBehavior::Ask,
        }
    }
}

/// 可持久化的用户设置
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Settings {
    /// 手机通知到达时弹系统弹窗
    pub toast_enabled: bool,
    /// 弹窗是否显示通知正文（关 = 只提示"收到一条通知"，保护隐私）
    pub toast_show_content: bool,
    /// 剪贴板自动同步开关
    pub clip_sync: bool,
    /// 主题偏好
    pub theme: Theme,
    /// Debug 模式（全栈日志落盘 + 可导出；默认关）
    pub debug_enabled: bool,
    /// 运行期功能开关：**用户想要的状态**。实际是否加载由 `crate::features` 在启动时固化，
    /// 两者不一致即"待重启生效"
    pub feat_notifications: bool,
    pub feat_clipboard: bool,
    pub feat_file_transfer: bool,
    /// 媒体控制
    pub feat_media: bool,
    /// 相册（图片互传）
    pub feat_album: bool,
    /// 收件目录（手机发来的文件落哪）。空串 = 用默认 Downloads\LinkX。
    pub inbox: String,
    /// 自动连接已绑定设备（默认开；关 = 必须人点一次设备才连）
    pub auto_connect: bool,
    /// 开机自启动：`settings.ini` 里这份只是记忆，真值在注册表，进设置页时回读并同步到这里
    pub autostart: bool,
    /// 关闭按钮行为
    pub close_behavior: CloseBehavior,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            toast_enabled: true,
            toast_show_content: true,
            clip_sync: true,
            theme: Theme::System,
            debug_enabled: false,
            // 默认全开：功能开关是"用户主动关才省内存"，不能反过来默认砍人功能
            feat_notifications: true,
            feat_clipboard: true,
            feat_file_transfer: true,
            feat_media: true,
            feat_album: true,
            inbox: String::new(),
            auto_connect: true,
            // 开机自启动默认关：往注册表 Run 键写东西必须是用户明确要的
            autostart: false,
            close_behavior: CloseBehavior::default(),
        }
    }
}

impl Settings {
    /// 从共享状态读出当前设置（用于保存）
    pub(crate) fn from_state(st: &crate::state::UiState) -> Self {
        Self {
            toast_enabled: st.toast_enabled,
            toast_show_content: st.toast_show_content,
            clip_sync: st.clip_sync,
            theme: st.theme,
            debug_enabled: st.debug_enabled,
            feat_notifications: st.feat_notifications,
            feat_clipboard: st.feat_clipboard,
            feat_file_transfer: st.feat_file_transfer,
            feat_media: st.feat_media,
            feat_album: st.feat_album,
            inbox: st.inbox_dir.clone(),
            auto_connect: st.auto_connect,
            autostart: st.autostart,
            close_behavior: st.close_behavior,
        }
    }

    /// 覆盖式写入文件（失败静默：设置丢了也不该影响使用）
    pub(crate) fn save(&self) {
        // data_dir 返回 Result（拒绝 CWD 回退）；设置写入失败一律静默
        let Ok(dir) = crate::identity::data_dir() else {
            return;
        };
        let _ = fs::create_dir_all(&dir);
        let body = format!(
            "toast_enabled={}
toast_show_content={}
clip_sync={}
theme={}
debug_enabled={}
feat_notifications={}
feat_clipboard={}
feat_file_transfer={}
feat_media={}
feat_album={}
inbox={}
auto_connect={}
autostart={}
close_behavior={}
",
            self.toast_enabled as u8,
            self.toast_show_content as u8,
            self.clip_sync as u8,
            self.theme.as_str(),
            self.debug_enabled as u8,
            self.feat_notifications as u8,
            self.feat_clipboard as u8,
            self.feat_file_transfer as u8,
            self.feat_media as u8,
            self.feat_album as u8,
            // 这份文件是「一行一键」的格式，值里带换行就会凭空多出一行配置，而读取端照单全收
            // 收件目录是唯一自由文本，所以在写的时候剥掉 CR/LF
            self.inbox.replace(['\r', '\n'], ""),
            self.auto_connect as u8,
            self.autostart as u8,
            self.close_behavior.as_str(),
        );
        // 先写临时文件、再改名覆盖：`fs::write` 是"截断后重写"，进程在中间被杀就留下一份半截
        // ini，而读取端把"缺键"一律回落默认值 —— 于是隐私相关的开关（剪贴板同步、通知里显示
        // 正文、各功能模块）会在用户完全不知情的时候自己打开回去。改名是原子的，写坏不了在用的那份。
        let tmp = dir.join("settings.ini.tmp");
        if fs::write(&tmp, &body).is_ok() {
            let _ = fs::rename(&tmp, dir.join("settings.ini"));
        }
    }
}

/// 读取设置（缺失/损坏的键各自回落默认值）
pub(crate) fn load() -> Settings {
    let mut s = Settings::default();
    let Ok(dir) = crate::identity::data_dir() else {
        return s;
    };
    let Ok(raw) = fs::read_to_string(dir.join("settings.ini")) else {
        return s;
    };
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        // 只认 "1" 为开：`v != "0"` 会把垃圾值（半行、手改、编码坏掉）读成"开"，
        // 而 `auto_connect` 一旦"被开"就是冷启动静默连上某台设备 —— 那种默认值必须偏保守。
        let on = |v: &str| v == "1";
        match k.trim() {
            "toast_enabled" => s.toast_enabled = on(v),
            "toast_show_content" => s.toast_show_content = on(v),
            "clip_sync" => s.clip_sync = on(v),
            "theme" => s.theme = Theme::parse(v),
            "debug_enabled" => s.debug_enabled = on(v),
            "feat_notifications" => s.feat_notifications = on(v),
            "feat_clipboard" => s.feat_clipboard = on(v),
            "feat_file_transfer" => s.feat_file_transfer = on(v),
            "feat_media" => s.feat_media = on(v),
            "feat_album" => s.feat_album = on(v),
            "inbox" => s.inbox = v.to_string(),
            "auto_connect" => s.auto_connect = on(v),
            "autostart" => s.autostart = on(v),
            "close_behavior" => s.close_behavior = CloseBehavior::parse(v),
            _ => {}
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_behavior_values_round_trip_and_garbage_falls_back_to_ask() {
        for b in [
            CloseBehavior::Ask,
            CloseBehavior::Minimize,
            CloseBehavior::Exit,
        ] {
            assert_eq!(CloseBehavior::parse(b.as_str()), b);
        }
        assert_eq!(CloseBehavior::parse("MINIMIZE"), CloseBehavior::Minimize);
        for junk in ["", " ", "tray", "0", "最小化"] {
            assert_eq!(
                CloseBehavior::parse(junk),
                CloseBehavior::Ask,
                "{junk:?} 不该被认成别的值"
            );
        }
        assert_eq!(CloseBehavior::default(), CloseBehavior::Ask);
    }

    #[test]
    fn close_behavior_cycle_covers_all_three_values_once() {
        let mut seen = vec![CloseBehavior::Ask];
        let mut cur = CloseBehavior::Ask;
        for _ in 0..3 {
            cur = cur.next();
            seen.push(cur);
        }
        assert_eq!(
            seen,
            vec![
                CloseBehavior::Ask,
                CloseBehavior::Minimize,
                CloseBehavior::Exit,
                CloseBehavior::Ask
            ],
            "点一下只该走一格，且三格一圈回到询问"
        );
    }

    /// 界面上那颗循环按钮显示的就是 `label()`：文案与取值表同源，改一个字不会只改到一半
    #[test]
    fn close_behavior_labels_are_the_three_named_choices() {
        assert_eq!(CloseBehavior::Ask.label(), "询问");
        assert_eq!(CloseBehavior::Minimize.label(), "最小化到托盘");
        assert_eq!(CloseBehavior::Exit.label(), "退出程序");
    }
}
