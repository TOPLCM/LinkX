//! Windows BLE Central（`windows` crate 直调 WinRT）：扫描 Peripheral 广播 → 按 LinkX SERVICE UUID 连接
//! Android GATT Server → 订阅 CHAR_EVT 读分片流、写 CHAR_TX 发命令流。Windows 是主动端，UART 形状与
//! `linkx_lan::BleUuid` 单源一致，由 `crate::app` 的 worker 线程驱动。两条不能改的约束：
//! - **写必须非阻塞**（`begin_write` + `poll_inflight`）：`IAsyncOperation::get()` 在链路已死时会一路等到 OS 超时，
//!   worker 是唯一线程，一停就把收包和 tick 全饿死；
//! - **通知处理器成对存、成对摘**（见 `subscription` 字段）：把上一轮的 token 交给本轮的特征对象去摘，
//!   在 WinRT 里是未定义行为，实测对应 `ntdll` 堆损坏。

use std::sync::Mutex;
use std::time::Duration;

use linkx_lan::BleUuid;
use windows::core::{Result, GUID};
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisement, BluetoothLEAdvertisementReceivedEventArgs,
    BluetoothLEAdvertisementWatcher, BluetoothLEScanningMode,
};
use windows::Devices::Bluetooth::BluetoothCacheMode;
use windows::Devices::Bluetooth::BluetoothLEDevice;
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristic, GattClientCharacteristicConfigurationDescriptorValue,
    GattCommunicationStatus, GattDeviceService, GattValueChangedEventArgs,
};
use windows::Foundation::{
    AsyncStatus, EventRegistrationToken, IAsyncOperation, TypedEventHandler,
};
use windows::Storage::Streams::{DataReader, DataWriter};

/// 连接尝试次数（真机联调：**首次** GATT 服务发现常因对端 RPA 尚未解析而返回 `Unreachable`，官方建议稍候重试）
const CONNECT_ATTEMPTS: usize = 6;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(700);

#[derive(Debug)]
pub enum WritePoll {
    Idle,
    Pending,
    /// 上一片已完成：`Ok` 送达 / `Err(原因, 原始包)` 需回塞重发。用 `std::result::Result` 而非本文件顶部的
    /// `Result` 别名 —— 后者是 `windows::core::Result<T>`（只带一个泛型参数，HRESULT 语义也不适合回传文本原因）
    Done(std::result::Result<(), String>, Vec<u8>),
}

/// `GattCommunicationStatus` 可读名（windows-rs 把它生成成常量结构体，不能 `match`）
fn status_name(s: GattCommunicationStatus) -> &'static str {
    if s == GattCommunicationStatus::Success {
        "Success"
    } else if s == GattCommunicationStatus::Unreachable {
        "Unreachable"
    } else if s == GattCommunicationStatus::ProtocolError {
        "ProtocolError"
    } else if s == GattCommunicationStatus::AccessDenied {
        "AccessDenied"
    } else {
        "Unknown"
    }
}

fn guid_of(uuid: &str) -> Result<GUID> {
    // windows 0.58 GUID 无 FromStr，手工按 8-4-4-4-12 十六进制解析
    let clean: String = uuid.chars().filter(|c| *c != '-').collect();
    if clean.len() != 32 || !clean.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(windows::core::Error::from_hresult(
            windows::core::HRESULT(0x8000_000Du32 as i32), // E_INVALIDARG
        ));
    }
    let b = |s: &str| u32::from_str_radix(s, 16).unwrap_or(0);
    let d = |s: &str| u16::from_str_radix(s, 16).unwrap_or(0);
    Ok(GUID {
        data1: b(&clean[0..8]),
        data2: d(&clean[8..12]),
        data3: d(&clean[12..16]),
        data4: [
            u8::from_str_radix(&clean[16..18], 16).unwrap_or(0),
            u8::from_str_radix(&clean[18..20], 16).unwrap_or(0),
            u8::from_str_radix(&clean[20..22], 16).unwrap_or(0),
            u8::from_str_radix(&clean[22..24], 16).unwrap_or(0),
            u8::from_str_radix(&clean[24..26], 16).unwrap_or(0),
            u8::from_str_radix(&clean[26..28], 16).unwrap_or(0),
            u8::from_str_radix(&clean[28..30], 16).unwrap_or(0),
            u8::from_str_radix(&clean[30..32], 16).unwrap_or(0),
        ],
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BleAdvert {
    pub address: u64,
    pub name: String,
    /// 是否命中 LinkX 特征（含 LinkX GATT SERVICE UUID，或本地名含 "LinkX"）
    pub linkx: bool,
}

pub struct BleCentral {
    watcher: BluetoothLEAdvertisementWatcher,
    device: Mutex<Option<BluetoothLEDevice>>,
    tx_char: Mutex<Option<GattCharacteristic>>,
    evt_char: Mutex<Option<GattCharacteristic>>,
    /// 在途的那一次 GATT 写：`(待发原始包, 异步句柄)`。存在意义是**取代阻塞**：原实现每片都
    /// `WriteValueAsync()?.get()`，而 `.get()` 在链路已死（对端重启、RPA 失效）时会一路等到 OS 超时 ——
    /// 真机实测单轮 `step_flush_ble_out_us` 高达 **30 秒**、整轮 96 秒，而 worker 是唯一线程。
    /// 现在改成"发起即返回、下一轮轮询状态"，最坏只占用一个时间预算
    inflight: Mutex<Option<(Vec<u8>, IAsyncOperation<GattCommunicationStatus>)>>,
    /// 上一轮注册的 `(所挂的特征对象, token)`。**必须成对存**：`RemoveValueChanged` 只能在注册时那个对象上调用。
    /// 服务发现走 `BluetoothCacheMode::Uncached`，每轮重新配对都会拿到一个**新的** `GattCharacteristic`；把上一轮的
    /// token 交给这一轮的对象去摘在 WinRT 里是未定义行为（轻则返回错误被吞、旧闭包永久挂在 OS 缓存的对象上，重则 ntdll 堆损坏）
    subscription: Mutex<Option<(GattCharacteristic, EventRegistrationToken)>>,
}

impl BleCentral {
    /// 创建并启动扫描（active scan：Android 广播 LinkX GATT 服务时上报广告）
    pub fn new() -> Result<Self> {
        let watcher = BluetoothLEAdvertisementWatcher::new()?;
        watcher.SetScanningMode(BluetoothLEScanningMode::Active)?;
        Ok(Self {
            watcher,
            device: Mutex::new(None),
            tx_char: Mutex::new(None),
            evt_char: Mutex::new(None),
            subscription: Mutex::new(None),
            inflight: Mutex::new(None),
        })
    }

    /// 注册广告回调后开始扫描。回调抛错只记日志（扫描不可中断）。
    pub fn start_scan(&self, mut on_advert: impl FnMut(BleAdvert) + Send + 'static) -> Result<()> {
        let handler = TypedEventHandler::<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementReceivedEventArgs,
        >::new(
            move |_: &Option<BluetoothLEAdvertisementWatcher>,
                  args: &Option<BluetoothLEAdvertisementReceivedEventArgs>| {
                if let Some(args) = args {
                    if let Some(adv) = advert_of(args) {
                        on_advert(adv);
                    }
                }
                Ok(())
            },
        );
        let _ = self.watcher.Received(&handler)?;
        self.watcher.Start()
    }

    /// 按地址连接 Android Peripheral，并按 LinkX SERVICE UUID 解析出 CHAR_TX（写）/ CHAR_EVT（通知）两个特征。
    /// **带重试**（真机关键）：`FromBluetoothAddressAsync` 只取设备引用、**不发起连接**，连接由随后的服务发现触发，
    /// 而首次发现常返回 `Unreachable`（Windows 尚未解析对端 RPA）——表现为「BLE 连接失败」。
    /// 故按官方建议重试并重新取设备，且一律走 `Uncached` 避免命中过期的 GATT 缓存（重装/升级手机端后缓存必失效）
    pub fn connect(&self, address: u64) -> std::result::Result<(), String> {
        let mut last = String::from("未知错误");
        for attempt in 1..=CONNECT_ATTEMPTS {
            match self.try_connect(address) {
                Ok(()) => {
                    if attempt > 1 {
                        crate::say(format!("[LinkX] BLE 连接在第 {attempt} 次尝试成功"));
                    }
                    return Ok(());
                }
                Err(e) => {
                    last = e;
                    // 丢弃设备/特征引用：下一次重新取设备才会重新触发连接
                    *self.device.lock().unwrap() = None;
                    *self.tx_char.lock().unwrap() = None;
                    *self.evt_char.lock().unwrap() = None;
                    if attempt < CONNECT_ATTEMPTS {
                        std::thread::sleep(CONNECT_RETRY_DELAY);
                    }
                }
            }
        }
        Err(format!("{last}（已重试 {CONNECT_ATTEMPTS} 次）"))
    }

    /// 单次连接尝试（取设备 → 服务发现 → 解析两个特征）
    fn try_connect(&self, address: u64) -> std::result::Result<(), String> {
        let device = BluetoothLEDevice::FromBluetoothAddressAsync(address)
            .map_err(|e| format!("取设备句柄失败: {e}"))?
            .get()
            .map_err(|e| format!("取设备句柄失败: {e}"))?;
        let service = find_service(&device)?;
        let tx = first_characteristic(&service, BleUuid::CHAR_TX)?;
        let evt = first_characteristic(&service, BleUuid::CHAR_EVT)?;

        *self.device.lock().unwrap() = Some(device);
        *self.tx_char.lock().unwrap() = Some(tx);
        *self.evt_char.lock().unwrap() = Some(evt);
        Ok(())
    }

    /// 写特征句柄还在（仅供调试面 `ble_connected`；句柄在 ≠ 链路活着，那只能从写失败看出来）
    #[cfg(feature = "agent-debug")]
    pub fn is_connected(&self) -> bool {
        self.tx_char
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    /// 订阅 CHAR_EVT 通知（Peripheral→Central 方向字节流）。
    /// ⚠ 挂载顺序不能挪到写 CCCD **之前**：那个顺序确实堵住了"CCCD 已开、处理器未挂"的丢片窗口（真机 10/9 → 7/7），
    /// 但对应构建出现了 `ntdll` 堆损坏 —— 提前挂载的通知会在 worker 仍阻塞于 CCCD 写时从 WinRT 线程进来
    pub fn subscribe_notify(&self, mut rx: impl FnMut(&[u8]) + Send + 'static) -> Result<()> {
        let evt = self.evt_char.lock().unwrap().clone().expect("先 connect()");
        let status = evt
            .WriteClientCharacteristicConfigurationDescriptorAsync(
                GattClientCharacteristicConfigurationDescriptorValue::Notify,
            )?
            .get()?;
        if status != GattCommunicationStatus::Success {
            return Err(windows::core::Error::from_hresult(
                windows::core::HRESULT(0x8000_4005u32 as i32), // E_FAIL：CCCD 订阅未成功
            ));
        }
        let handler = TypedEventHandler::<GattCharacteristic, GattValueChangedEventArgs>::new(
            move |_: &Option<GattCharacteristic>, args: &Option<GattValueChangedEventArgs>| {
                if let Some(args) = args {
                    if let Some(bytes) = read_value(args) {
                        rx(&bytes);
                    }
                }
                Ok(())
            },
        );
        // 先摘掉上一轮的处理器，**并且是在它当初注册的那个对象上摘**：上一版写成 `evt.RemoveValueChanged(old_token)`，
        // 而 `evt` 是本轮新拿到的特征对象（Uncached 每轮都是新对象）——把外来 token 交给别的对象摘是未定义行为，堆损坏的头号嫌疑
        self.release_subscription();
        let token = evt.ValueChanged(&handler)?;
        *self.subscription.lock().unwrap_or_else(|p| p.into_inner()) = Some((evt.clone(), token));
        Ok(())
    }

    /// 轮询在途写状态。非阻塞 —— 这是修"单轮阻塞 30 秒"的关键：`IAsyncOperation::get()` 在链路已死时会等到 OS 超时，而本函数只查一次状态
    pub fn poll_inflight(&self) -> WritePoll {
        let mut slot = self
            .inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(op) = slot.as_ref().map(|(_, op)| op) else {
            return WritePoll::Idle;
        };
        let status = match op.Status() {
            Ok(s) => s,
            // 连状态都取不到：视为失败，把包交回调用方处置
            Err(e) => {
                let pkt = slot.take().map(|(p, _)| p).unwrap_or_default();
                return WritePoll::Done(Err(format!("查询 BLE 写状态失败: {e}")), pkt);
            }
        };
        if status == AsyncStatus::Started {
            return WritePoll::Pending;
        }
        let (pkt, op) = slot.take().expect("上面确认过在途");
        if status == AsyncStatus::Canceled {
            return WritePoll::Done(Err("BLE 写被取消".to_string()), pkt);
        }
        if status == AsyncStatus::Error {
            return WritePoll::Done(Err("BLE 写出错（链路可能已断开）".to_string()), pkt);
        }
        match op.GetResults() {
            Ok(s) if s == GattCommunicationStatus::Success => WritePoll::Done(Ok(()), pkt),
            Ok(s) => WritePoll::Done(Err(format!("BLE 写未完成: {}", status_name(s))), pkt),
            Err(e) => WritePoll::Done(Err(format!("BLE 取结果失败: {e}")), pkt),
        }
    }

    /// 发起一次非阻塞写。仅在 [`WritePoll::Idle`] 时调用；返回 `Err` 表示没能排入。
    /// 原始包存进在途槽是为了在**完成时**发现失败还能交回上层回塞 —— 分片少一片接收侧整组作废，那是静默丢包路径
    pub fn begin_write(&self, data: Vec<u8>) -> std::result::Result<(), String> {
        let tx = self
            .tx_char
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(tx) = tx else {
            #[cfg(feature = "agent-debug")]
            linkx_debugd::bump("ble_write_no_char", 1);
            return Err("BLE 未连接（无写特征）".to_string());
        };
        let writer = DataWriter::new().map_err(|e| format!("{e}"))?;
        writer.WriteBytes(&data).map_err(|e| format!("{e}"))?;
        let buffer = writer.DetachBuffer().map_err(|e| format!("{e}"))?;
        let op = tx.WriteValueAsync(&buffer).map_err(|e| format!("{e}"))?;
        *self
            .inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((data, op));
        Ok(())
    }

    /// 链路被判定已死时作废所有 GATT 句柄，强制下一次 `connect()` 重新解析。必须有这个：手机重启后 Windows 仍抱着
    /// 上一轮的 `GattCharacteristic`，每次写都返回 `AsyncStatus::Error`，而失败片会被回塞队首重试 —— 旧链路的死对象
    /// 把出站队列钉死（真机：第 2 轮起配对从 5 s 劣化到 35 s）。作废后至少不拿死对象空转，重连也会拿到全新句柄
    pub fn invalidate_link(&self) {
        self.abort_inflight();
        self.release_subscription();
        *self.tx_char.lock().unwrap_or_else(|p| p.into_inner()) = None;
        *self.evt_char.lock().unwrap_or_else(|p| p.into_inner()) = None;
        *self.device.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// 换设备 / 断开 / 重连前丢弃在途写槽。
    pub fn abort_inflight(&self) {
        self.inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }

    /// 摘除上一轮注册的 `ValueChanged` 处理器 —— **只在该处理器当初挂载的那个对象上摘**。摘除失败必须上报、
    /// 不能 `let _ =` 吞掉：吞掉意味着旧闭包永久挂在 OS 缓存的特征对象上，之后每片通知都往没人消费的队列里灌字节（无界增长）
    fn release_subscription(&self) {
        let prev = self
            .subscription
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some((evt, token)) = prev {
            if let Err(e) = evt.RemoveValueChanged(token) {
                crate::say(format!(
                    "[LinkX] BLE 旧通知处理器摘除失败（旧闭包可能仍在灌数据）: {e}"
                ));
                #[cfg(feature = "agent-debug")]
                linkx_debugd::bump("ble_handler_remove_failed", 1);
            }
        }
    }
}

fn advert_of(args: &BluetoothLEAdvertisementReceivedEventArgs) -> Option<BleAdvert> {
    let address = args.BluetoothAddress().ok()?;
    let adv = args.Advertisement().ok();
    let name = adv
        .as_ref()
        .and_then(|a| a.LocalName().ok())
        .map(|h| h.to_string())
        .unwrap_or_default();
    // 命中判据：名字含 "LinkX"，或广告声明了 LinkX GATT SERVICE UUID
    let linkx = name.contains("LinkX") || adv.as_ref().is_some_and(advert_has_linkx_service);
    Some(BleAdvert {
        address,
        name,
        linkx,
    })
}

fn advert_has_linkx_service(adv: &BluetoothLEAdvertisement) -> bool {
    let Ok(service) = guid_of(BleUuid::SERVICE) else {
        return false;
    };
    let Ok(list) = adv.ServiceUuids() else {
        return false;
    };
    let n = list.Size().unwrap_or(0);
    (0..n).any(|i| list.GetAt(i).map(|g| g == service).unwrap_or(false))
}

/// 按 UUID 找到 LinkX 服务（`Uncached` 强制重新发现；按 UUID 直查失败则回退为全量枚举）
fn find_service(device: &BluetoothLEDevice) -> std::result::Result<GattDeviceService, String> {
    let want = guid_of(BleUuid::SERVICE).map_err(|e| format!("服务 UUID 非法: {e}"))?;

    // 1) 按 UUID 直查。该调用同时承担「建立连接 + 服务发现」两件事。
    let direct = device
        .GetGattServicesForUuidWithCacheModeAsync(want, BluetoothCacheMode::Uncached)
        .map_err(|e| format!("服务发现调用失败: {e}"))?
        .get()
        .map_err(|e| format!("服务发现失败: {e}"))?;
    let direct_status = direct
        .Status()
        .map_err(|e| format!("服务发现状态读取失败: {e}"))?;
    if direct_status == GattCommunicationStatus::Success {
        let services = direct
            .Services()
            .map_err(|e| format!("服务列表读取失败: {e}"))?;
        if services.Size().unwrap_or(0) > 0 {
            return services
                .GetAt(0)
                .map_err(|e| format!("服务句柄读取失败: {e}"));
        }
    }

    // 2) 回退：枚举全部服务再按 UUID 匹配（部分蓝牙栈按 UUID 过滤会直接返回空列表）
    let all = device
        .GetGattServicesWithCacheModeAsync(BluetoothCacheMode::Uncached)
        .map_err(|e| format!("服务枚举调用失败: {e}"))?
        .get()
        .map_err(|e| format!("服务枚举失败: {e}"))?;
    let all_status = all
        .Status()
        .map_err(|e| format!("服务枚举状态读取失败: {e}"))?;
    if all_status != GattCommunicationStatus::Success {
        return Err(format!(
            "对端不可达/服务发现失败（按 UUID 查询: {}，全量枚举: {}）",
            status_name(direct_status),
            status_name(all_status)
        ));
    }
    let list = all
        .Services()
        .map_err(|e| format!("服务列表读取失败: {e}"))?;
    let n = list.Size().unwrap_or(0);
    for i in 0..n {
        let s = list
            .GetAt(i)
            .map_err(|e| format!("服务句柄读取失败: {e}"))?;
        if s.Uuid().map(|u| u == want).unwrap_or(false) {
            return Ok(s);
        }
    }
    Err(format!(
        "对端未提供 LinkX 服务（已枚举 {n} 个服务；请确认手机端 LinkX 已启动且蓝牙已开启）"
    ))
}

fn first_characteristic(
    service: &GattDeviceService,
    uuid: &str,
) -> std::result::Result<GattCharacteristic, String> {
    let want = guid_of(uuid).map_err(|e| format!("特征 UUID 非法: {e}"))?;
    let result = service
        .GetCharacteristicsForUuidWithCacheModeAsync(want, BluetoothCacheMode::Uncached)
        .map_err(|e| format!("取特征 {uuid} 调用失败: {e}"))?
        .get()
        .map_err(|e| format!("取特征 {uuid} 失败: {e}"))?;
    let status = result
        .Status()
        .map_err(|e| format!("特征状态读取失败: {e}"))?;
    if status != GattCommunicationStatus::Success {
        return Err(format!("取特征 {uuid} 失败: {}", status_name(status)));
    }
    let chars = result
        .Characteristics()
        .map_err(|e| format!("特征列表读取失败: {e}"))?;
    if chars.Size().unwrap_or(0) == 0 {
        return Err(format!("对端缺少特征 {uuid}（GATT 服务不完整）"));
    }
    chars.GetAt(0).map_err(|e| format!("特征句柄读取失败: {e}"))
}

fn read_value(args: &GattValueChangedEventArgs) -> Option<Vec<u8>> {
    let buffer = args.CharacteristicValue().ok()?;
    let reader = DataReader::FromBuffer(&buffer).ok()?;
    let len = reader.UnconsumedBufferLength().ok()? as usize;
    let mut bytes = vec![0u8; len];
    reader.ReadBytes(&mut bytes).ok()?;
    Some(bytes)
}
