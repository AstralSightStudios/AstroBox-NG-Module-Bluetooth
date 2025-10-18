// TODO: 等待蓝牙重构以替换掉这个庞大的转接文件

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

#[cfg(target_os = "android")]
use crate::btinterface::ble_stub::{Adapter, BluestDevice, BluestService, Characteristic};

#[cfg(not(target_os = "android"))]
use bluest::{Adapter, Characteristic, Device as BluestDevice, Service as BluestService};

use bluest::Uuid;

use btclassic_spp::BtclassicSppExt;
use futures_util::{
    StreamExt,
    future::{AbortHandle, Abortable},
};
use tauri::{AppHandle, Wry};

use crate::btinterface::{
    BluetoothDevice, BluetoothInterface, ConnectError, ConnectType, DisconnectError, ScanError,
    SendError, SubscribeError,
};

const BLE_UUID_KEYWORD_XIAOMI_SERVICE: &str = "0050";
const BLE_UUID_KEYWORD_XIAOMI_SENT: &str = "005f";
const BLE_UUID_KEYWORD_XIAOMI_RECV: &str = "005e";

static APP_HANDLE: OnceLock<AppHandle<Wry>> = OnceLock::new();

#[cfg(not(target_os = "android"))]
static BLE_ADAPTER: OnceLock<Adapter> = OnceLock::new();

pub fn init(app: AppHandle<Wry>) -> tauri::Result<()> {
    app.plugin(btclassic_spp::init())?;
    let _ = APP_HANDLE.set(app);
    Ok(())
}

pub fn normalize_addr_for_dedup(raw: &str) -> String {
    fn is_hex(c: char) -> bool {
        matches!(c, '0'..='9' | 'a'..='f' | 'A'..='F')
    }

    let s = raw.trim();
    if s.is_empty() {
        return String::new();
    }

    let mut hex: String = s.chars().filter(|c: &char| is_hex(*c)).collect();

    if hex.len() >= 12 {
        if hex.len() > 12 {
            hex = hex[hex.len() - 12..].to_string();
        }
        let pairs: Vec<String> = hex
            .as_bytes()
            .chunks(2)
            .map(|ch| std::str::from_utf8(ch).unwrap_or(""))
            .map(|p| p.to_ascii_uppercase())
            .collect();
        return pairs.join(":");
    }

    s.to_ascii_uppercase()
}

pub struct StdImp {
    connect_type: ConnectType,

    ble_device: Mutex<Option<BluestDevice>>,
    ble_services: Mutex<Option<Vec<BluestService>>>,
    ble_chara_cache: Mutex<HashMap<Uuid, Characteristic>>,
    ble_char_bundle: Mutex<Option<BleCharacteristicBundle>>,
    #[cfg(not(target_os = "android"))]
    ble_scanned_devices: Arc<Mutex<HashMap<String, BluestDevice>>>,
    ble_on_connected: Mutex<Option<Arc<dyn Fn() + Send + Sync + 'static>>>,
    scan_abort: Mutex<Option<AbortHandle>>,
    scan_results: Arc<Mutex<Vec<BluetoothDevice>>>,
}

impl fmt::Debug for StdImp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StdImp")
            .field("connect_type", &self.connect_type)
            .finish()
    }
}

#[derive(Debug, Default)]
struct BleCharacteristicBundle {
    service: Option<Uuid>,
    recv: Option<Uuid>,
    sent: Option<Uuid>,
}

impl StdImp {
    pub fn new(connect_type: ConnectType) -> Self {
        Self {
            connect_type,
            ble_device: Mutex::new(None),
            ble_services: Mutex::new(None),
            ble_chara_cache: Mutex::new(HashMap::new()),
            ble_char_bundle: Mutex::new(None),
            #[cfg(not(target_os = "android"))]
            ble_scanned_devices: Arc::new(Mutex::new(HashMap::new())),
            ble_on_connected: Mutex::new(None),
            scan_abort: Mutex::new(None),
            scan_results: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[inline]
    fn app() -> Option<&'static AppHandle<Wry>> {
        APP_HANDLE.get()
    }

    #[cfg(not(target_os = "android"))]
    async fn ble_adapter() -> Result<&'static Adapter, ConnectError> {
        if let Some(adapter) = BLE_ADAPTER.get() {
            return Ok(adapter);
        }

        let adapter = Adapter::default()
            .await
            .ok_or(ConnectError::DeviceNotFound)?;
        adapter
            .wait_available()
            .await
            .map_err(|_| ConnectError::DeviceNotFound)?;

        if BLE_ADAPTER.set(adapter).is_err() {
            // another concurrent initializer won the race; fall through
        }
        BLE_ADAPTER
            .get()
            .ok_or(ConnectError::DeviceNotFound)
    }

    fn uuid_contains(uuid: &Uuid, needle: &str) -> bool {
        let lowered_needle = needle.to_ascii_lowercase();
        let haystack: String = uuid
            .to_string()
            .chars()
            .filter(|c| *c != '-')
            .map(|c| c.to_ascii_lowercase())
            .collect();
        haystack.contains(&lowered_needle)
    }

    fn ble_default_recv_uuid(&self) -> Option<Uuid> {
        self.ble_char_bundle
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|bundle| bundle.recv)
    }

    fn ble_default_sent_uuid(&self) -> Option<Uuid> {
        self.ble_char_bundle
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|bundle| bundle.sent)
    }

    fn ble_default_service_uuid(&self) -> Option<Uuid> {
        self.ble_char_bundle
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|bundle| bundle.service)
    }

    #[cfg(not(target_os = "android"))]
    async fn ble_get_or_discover_chara(
        &self,
        uuid: Uuid,
    ) -> Result<Characteristic, SubscribeError> {
        if let Some(ch) = self.ble_chara_cache.lock().unwrap().get(&uuid).cloned() {
            return Ok(ch);
        }

        let services = self
            .ble_services
            .lock()
            .unwrap()
            .as_ref()
            .cloned()
            .ok_or(SubscribeError::Disconnected)?;

        for svc in services {
            let chars = svc
                .discover_characteristics()
                .await
                .map_err(|_| SubscribeError::Disconnected)?;
            for c in chars {
                if c.uuid() == uuid {
                    self.ble_chara_cache.lock().unwrap().insert(uuid, c.clone());
                    return Ok(c);
                }
            }
        }
        Err(SubscribeError::BleCharaNotFound)
    }

    #[cfg(target_os = "android")]
    async fn ble_get_or_discover_chara(
        &self,
        _uuid: Uuid,
    ) -> Result<Characteristic, SubscribeError> {
        Err(SubscribeError::BleCharaNotFound)
    }

    #[cfg(not(target_os = "android"))]
    async fn ble_extract_xiaomi_characteristics(
        &self,
        services: &[BluestService],
    ) -> Result<(), ConnectError> {
        let mut bundle = BleCharacteristicBundle::default();

        for svc in services {
            let svc_uuid = svc.uuid();
            if !Self::uuid_contains(&svc_uuid, "fe95") {
                continue;
            }

            let chars = svc
                .discover_characteristics()
                .await
                .map_err(|_| ConnectError::TargetRejected)?;

            for c in chars {
                let cuuid = c.uuid();
                if Self::uuid_contains(&cuuid, BLE_UUID_KEYWORD_XIAOMI_RECV) {
                    log::debug!(
                        "StdImp::ble_extract_xiaomi_characteristics detected recv characteristic {}",
                        cuuid
                    );
                    bundle.recv = Some(cuuid);
                } else if Self::uuid_contains(&cuuid, BLE_UUID_KEYWORD_XIAOMI_SENT) {
                    log::debug!(
                        "StdImp::ble_extract_xiaomi_characteristics detected sent characteristic {}",
                        cuuid
                    );
                    bundle.sent = Some(cuuid);
                } else if Self::uuid_contains(&cuuid, BLE_UUID_KEYWORD_XIAOMI_SERVICE) {
                    log::debug!(
                        "StdImp::ble_extract_xiaomi_characteristics detected service characteristic {}",
                        cuuid
                    );
                    bundle.service = Some(cuuid);
                }
                self.ble_chara_cache.lock().unwrap().insert(cuuid, c.clone());
            }
        }

        if bundle.recv.is_none() || bundle.sent.is_none() {
            return Err(ConnectError::TargetRejected);
        }

        *self.ble_char_bundle.lock().unwrap() = Some(bundle);
        Ok(())
    }

    fn ble_clear_cache(&self) {
        *self.ble_device.lock().unwrap() = None;
        *self.ble_services.lock().unwrap() = None;
        self.ble_chara_cache.lock().unwrap().clear();
        *self.ble_char_bundle.lock().unwrap() = None;
    }
}

impl BluetoothInterface for StdImp {
    fn start_scan(&self, channel: tauri::ipc::Channel<BluetoothDevice>) -> Result<(), ScanError> {
        match self.connect_type {
            ConnectType::SPP => {
                let app = Self::app().ok_or(ScanError::AdapterNotFound)?;

                let abort_reg = {
                    let mut guard = self.scan_abort.lock().unwrap();
                    if guard.is_some() {
                        return Ok(());
                    }
                    let (abort_handle, abort_reg) = AbortHandle::new_pair();
                    *guard = Some(abort_handle);
                    abort_reg
                };

                let app_handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    let fut = async move {
                        let mut known_addrs = HashSet::<String>::new();
                        loop {
                            if let Ok(res) = app_handle.btclassic_spp().get_scanned_devices() {
                                for d in res.ret {
                                    let name = d.name.unwrap_or_default();
                                    let raw_addr = d.address;
                                    let key = normalize_addr_for_dedup(&raw_addr);
                                    if key.is_empty() {
                                        continue;
                                    }
                                    if known_addrs.insert(key) {
                                        let _ = channel.send(BluetoothDevice {
                                            name,
                                            addr: raw_addr,
                                        });
                                    }
                                }
                            }
                            tokio::time::sleep(Duration::from_millis(800)).await;
                        }
                    };
                    let _ = Abortable::new(fut, abort_reg).await;
                });

                app.btclassic_spp()
                    .start_scan()
                    .map_err(|_| ScanError::AdapterNotFound)?;
                Ok(())
            }

            #[cfg(not(target_os = "android"))]
            ConnectType::BLE => tauri::async_runtime::block_on(async {
                {
                    let abort_guard = self.scan_abort.lock().unwrap();
                    if abort_guard.is_some() {
                        return Ok(());
                    }
                }

                let adapter = Self::ble_adapter()
                    .await
                    .map_err(|_| ScanError::AdapterNotFound)?;

                self.scan_results.lock().unwrap().clear();
                #[cfg(not(target_os = "android"))]
                self.ble_scanned_devices.lock().unwrap().clear();

                let results = self.scan_results.clone();
                let (abort_handle, abort_reg) = AbortHandle::new_pair();
                *self.scan_abort.lock().unwrap() = Some(abort_handle);

                let channel_clone = channel.clone();
                #[cfg(not(target_os = "android"))]
                let scanned_devices = Arc::clone(&self.ble_scanned_devices);
                tauri::async_runtime::spawn(async move {
                    let mut stream = match adapter.scan(&[]).await {
                        Ok(s) => s,
                        Err(_) => return,
                    };
                    let fut = async move {
                        let mut known_addrs = HashSet::<String>::new();

                        while let Some(dev) = stream.next().await {
                            let name = dev.device.name().unwrap_or_default().to_string();
                            let raw_addr = dev.device.id().to_string();
                            let key = normalize_addr_for_dedup(&raw_addr);
                            if key.is_empty() {
                                continue;
                            }
                            if known_addrs.insert(key) {
                                #[cfg(not(target_os = "android"))]
                                scanned_devices
                                    .lock()
                                    .unwrap()
                                    .insert(raw_addr.clone(), dev.device.clone());
                                results.lock().unwrap().push(BluetoothDevice {
                                    name: name.clone(),
                                    addr: raw_addr.clone(),
                                });
                                let _ = channel_clone.send(BluetoothDevice {
                                    name,
                                    addr: raw_addr,
                                });
                            }
                        }
                    };
                    let _ = Abortable::new(fut, abort_reg).await;
                });
                Ok(())
            }),

            #[cfg(target_os = "android")]
            ConnectType::BLE => Err(ScanError::AdapterNotFound),
        }
    }

    fn stop_scan(&self) -> Result<Vec<BluetoothDevice>, ScanError> {
        match self.connect_type {
            ConnectType::SPP => {
                let was_scanning = self.scan_abort.lock().unwrap().take().is_some();
                let app = Self::app().ok_or(ScanError::AdapterNotFound)?;
                let spp = app.btclassic_spp();

                if was_scanning {
                    let _ = spp.stop_scan().map_err(|_| ScanError::AdapterNotFound)?;
                }

                let result = spp
                    .get_scanned_devices()
                    .map_err(|_| ScanError::AdapterNotFound)?;

                let mut seen = HashSet::<String>::new();
                let mut out = Vec::new();
                for d in result.ret {
                    let name = d.name.unwrap_or_default();
                    let raw_addr = d.address;
                    let key = normalize_addr_for_dedup(&raw_addr);
                    if key.is_empty() {
                        continue;
                    }
                    if seen.insert(key) {
                        out.push(BluetoothDevice {
                            name,
                            addr: raw_addr,
                        });
                    }
                }
                Ok(out)
            }
            ConnectType::BLE => {
                if let Some(handle) = self.scan_abort.lock().unwrap().take() {
                    handle.abort();
                }
                Ok(self.scan_results.lock().unwrap().drain(..).collect())
            }
        }
    }

    fn connect(&self, addr: String) -> Result<(), ConnectError> {
        match self.connect_type {
            ConnectType::SPP => {
                let app = Self::app().ok_or(ConnectError::DeviceNotFound)?;
                let spp = app.btclassic_spp();
                match spp.connect(&addr, false) {
                    Ok(res) if res.ret => Ok(()),
                    Ok(_) => Err(ConnectError::TargetRejected),
                    Err(_) => Err(ConnectError::DeviceNotFound),
                }
            }

            #[cfg(not(target_os = "android"))]
            ConnectType::BLE => tauri::async_runtime::block_on(async {
                log::info!("StdImp::connect (BLE) starting for addr={}", addr);
                let adapter = Self::ble_adapter().await?;

                let mut device_opt = {
                    let map = self.ble_scanned_devices.lock().unwrap();
                    map.get(&addr).cloned()
                };

                if device_opt.is_none() {
                    log::debug!(
                        "StdImp::connect (BLE) cache miss for addr={}, starting on-demand scan",
                        addr
                    );
                    let mut scan = adapter
                        .scan(&[])
                        .await
                        .map_err(|_| ConnectError::DeviceNotFound)?;
                    let deadline = Instant::now() + Duration::from_secs(12);
                    while Instant::now() < deadline {
                        if let Some(dev) = scan.next().await {
                            if dev.device.id().to_string().eq_ignore_ascii_case(&addr) {
                                device_opt = Some(dev.device.clone());
                                #[cfg(not(target_os = "android"))]
                                self.ble_scanned_devices
                                    .lock()
                                    .unwrap()
                                    .insert(addr.clone(), dev.device.clone());
                                break;
                            }
                        } else {
                            tokio::time::sleep(Duration::from_millis(60)).await;
                        }
                    }
                }

                let device = device_opt.ok_or(ConnectError::DeviceNotFound)?;
                log::debug!(
                    "StdImp::connect (BLE) using cached device handle for addr={}",
                    addr
                );

                adapter
                    .connect_device(&device)
                    .await
                    .map_err(|_| ConnectError::TargetRejected)?;
                log::info!(
                    "StdImp::connect (BLE) controller connection established addr={}",
                    addr
                );

                let mut attempts = 0;
                while !device.is_connected().await {
                    if attempts >= 20 {
                        log::warn!(
                            "StdImp::connect (BLE) device still reports disconnected after {} checks",
                            attempts
                        );
                        break;
                    }
                    attempts += 1;
                    log::debug!(
                        "StdImp::connect (BLE) waiting for device to report connected (attempt {})",
                        attempts
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                log::info!(
                    "StdImp::connect (BLE) device is_connected={} after wait",
                    device.is_connected().await
                );

                if let Err(e) = device.pair().await {
                    log::warn!("BLE pair failed: {}", e);
                }

                let services = device
                    .discover_services()
                    .await
                    .map_err(|_| ConnectError::TargetRejected)?;
                log::info!(
                    "StdImp::connect (BLE) discovered {} services for addr={}",
                    services.len(),
                    addr
                );

                self.ble_chara_cache.lock().unwrap().clear();
                self.ble_char_bundle.lock().unwrap().take();

                self.ble_extract_xiaomi_characteristics(&services).await?;

                *self.ble_device.lock().unwrap() = Some(device);
                *self.ble_services.lock().unwrap() = Some(services);

                let cb_opt = self.ble_on_connected.lock().unwrap().clone();
                if let Some(cb) = cb_opt {
                    log::info!(
                        "StdImp::connect (BLE) invoking on_connected callback for addr={}",
                        addr
                    );
                    cb();
                } else {
                    log::warn!(
                        "StdImp::connect (BLE) no on_connected callback registered; upper layers may hang"
                    );
                }

                Ok(())
            }),

            #[cfg(target_os = "android")]
            ConnectType::BLE => Err(ConnectError::DeviceNotFound),
        }
    }

    fn set_on_connected_listener(&self, cb: Arc<dyn Fn() + Send + Sync + 'static>) {
        match self.connect_type {
            ConnectType::SPP => {
                if let Some(app) = Self::app() {
                    let cb2 = cb.clone();
                    let _ = app.btclassic_spp().on_connected(move || {
                        (cb2)();
                    });
                }
            }
            ConnectType::BLE => {
                log::debug!("StdImp::set_on_connected_listener registered BLE callback");
                *self.ble_on_connected.lock().unwrap() = Some(cb);
            }
        }
    }

    fn send(&self, data: Vec<u8>, characteristic: Option<Uuid>) -> Result<(), SendError> {
        match self.connect_type {
            ConnectType::SPP => {
                let app = Self::app().ok_or(SendError::Disconnected)?;
                app.btclassic_spp()
                    .send(&data)
                    .map_err(|_| SendError::Disconnected)
            }

            #[cfg(not(target_os = "android"))]
            ConnectType::BLE => {
                let uuid = match characteristic {
                    Some(uuid) => uuid,
                    None => self
                        .ble_default_sent_uuid()
                        .ok_or(SendError::BleCharaNotFound)?,
                };
                tauri::async_runtime::block_on(async {
                    log::debug!("StdImp::send (BLE) writing {} bytes to char {}", data.len(), uuid);
                    if self.ble_device.lock().unwrap().is_none() {
                        return Err(SendError::Disconnected);
                    }

                    let chara =
                        self.ble_get_or_discover_chara(uuid)
                            .await
                            .map_err(|e| match e {
                                SubscribeError::BleCharaNotFound => SendError::BleCharaNotFound,
                                _ => SendError::Disconnected,
                            })?;

                    chara
                        .write_without_response(&data)
                        .await
                        .map_err(|_| SendError::Disconnected)
                })
            }

            #[cfg(target_os = "android")]
            ConnectType::BLE => Err(SendError::Disconnected),
        }
    }

    fn subscribe(
        &self,
        cb: Arc<dyn Fn(Result<Vec<u8>, String>) + Send + Sync>,
        characteristic: Option<Uuid>,
    ) -> Result<(), SubscribeError> {
        match self.connect_type {
            ConnectType::SPP => {
                let app = Self::app().ok_or(SubscribeError::Disconnected)?;
                let spp = app.btclassic_spp();
                let cb_clone = cb.clone();
                spp.set_data_listener(move |res| (cb_clone)(res))
                    .map_err(|_| SubscribeError::Disconnected)?;
                spp.start_subscription()
                    .map_err(|_| SubscribeError::Disconnected)
            }

            #[cfg(not(target_os = "android"))]
            ConnectType::BLE => {
                let uuid = match characteristic {
                    Some(uuid) => uuid,
                    None => self
                        .ble_default_recv_uuid()
                        .ok_or(SubscribeError::BleCharaNotFound)?,
                };
                tauri::async_runtime::block_on(async {
                    if self.ble_device.lock().unwrap().is_none() {
                        return Err(SubscribeError::Disconnected);
                    }
                    log::info!("StdImp::subscribe (BLE) attempting to subscribe to {}", uuid);
                    let chara = self.ble_get_or_discover_chara(uuid).await?;
                    let cb_clone = cb.clone();
                    tauri::async_runtime::spawn(async move {
                        match chara.notify().await {
                            Ok(mut notify) => {
                                while let Some(item) = notify.next().await {
                                    match item {
                                        Ok(bytes) => (cb_clone)(Ok(bytes)),
                                        Err(e) => (cb_clone)(Err(e.to_string())),
                                    }
                                }
                            }
                            Err(e) => {
                                log::error!(
                                    "StdImp::subscribe (BLE) failed to start notify on {}: {}",
                                    uuid,
                                    e
                                );
                                (cb_clone)(Err(e.to_string()))
                            }
                        }
                    });

                    if let Some(service_uuid) = self.ble_default_service_uuid() {
                        if let Ok(service_chara) = self.ble_get_or_discover_chara(service_uuid).await
                        {
                            if let Err(e) = service_chara.read().await {
                                log::debug!("Failed to read Xiaomi service characteristic: {}", e);
                            }
                        }
                    }
                    Ok(())
                })
            }

            #[cfg(target_os = "android")]
            ConnectType::BLE => Err(SubscribeError::Disconnected),
        }
    }

    fn disconnect(&self) -> Result<(), DisconnectError> {
        match self.connect_type {
            ConnectType::SPP => {
                let app = Self::app().ok_or(DisconnectError::DeviceNotFound)?;
                app.btclassic_spp()
                    .disconnect()
                    .map_err(|_| DisconnectError::DeviceNotFound)
            }

            #[cfg(not(target_os = "android"))]
            ConnectType::BLE => {
                let dev_opt = self.ble_device.lock().unwrap().clone();
                if let Some(dev) = dev_opt {
                    let res = tauri::async_runtime::block_on(async {
                        if let Ok(adapter) = Self::ble_adapter().await {
                            adapter
                                .disconnect_device(&dev)
                                .await
                                .map_err(|_| DisconnectError::DeviceNotFound)
                        } else {
                            Err(DisconnectError::DeviceNotFound)
                        }
                    });
                    self.ble_clear_cache();
                    res
                } else {
                    self.ble_clear_cache();
                    Ok(())
                }
            }

            #[cfg(target_os = "android")]
            ConnectType::BLE => {
                self.ble_clear_cache();
                Ok(())
            }
        }
    }
}
