use std::{
    collections::{HashMap, HashSet},
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

static APP_HANDLE: OnceLock<AppHandle<Wry>> = OnceLock::new();

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

#[derive(Debug)]
pub struct StdImp {
    connect_type: ConnectType,

    ble_device: Mutex<Option<BluestDevice>>,
    ble_services: Mutex<Option<Vec<BluestService>>>,
    ble_chara_cache: Mutex<HashMap<Uuid, Characteristic>>,
    scan_abort: Mutex<Option<AbortHandle>>,
    scan_results: Arc<Mutex<Vec<BluetoothDevice>>>,
}

impl StdImp {
    pub fn new(connect_type: ConnectType) -> Self {
        Self {
            connect_type,
            ble_device: Mutex::new(None),
            ble_services: Mutex::new(None),
            ble_chara_cache: Mutex::new(HashMap::new()),
            scan_abort: Mutex::new(None),
            scan_results: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[inline]
    fn app() -> Option<&'static AppHandle<Wry>> {
        APP_HANDLE.get()
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

    fn ble_clear_cache(&self) {
        *self.ble_device.lock().unwrap() = None;
        *self.ble_services.lock().unwrap() = None;
        self.ble_chara_cache.lock().unwrap().clear();
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

                let adapter = Adapter::default().await.ok_or(ScanError::AdapterNotFound)?;
                adapter
                    .wait_available()
                    .await
                    .map_err(|_| ScanError::MissingPermission)?;

                self.scan_results.lock().unwrap().clear();

                let results = self.scan_results.clone();
                let (abort_handle, abort_reg) = AbortHandle::new_pair();
                *self.scan_abort.lock().unwrap() = Some(abort_handle);

                let channel_clone = channel.clone();
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
                let adapter = Adapter::default()
                    .await
                    .ok_or(ConnectError::DeviceNotFound)?;
                adapter
                    .wait_available()
                    .await
                    .map_err(|_| ConnectError::DeviceNotFound)?;

                let mut scan = adapter
                    .scan(&[])
                    .await
                    .map_err(|_| ConnectError::DeviceNotFound)?;
                let deadline = Instant::now() + Duration::from_secs(12);
                let mut found: Option<BluestDevice> = None;
                while Instant::now() < deadline {
                    if let Some(dev) = scan.next().await {
                        if dev.device.id().to_string().eq_ignore_ascii_case(&addr) {
                            found = Some(dev.device);
                            break;
                        }
                    } else {
                        tokio::time::sleep(Duration::from_millis(60)).await;
                    }
                }
                let device = found.ok_or(ConnectError::DeviceNotFound)?;

                adapter
                    .connect_device(&device)
                    .await
                    .map_err(|_| ConnectError::TargetRejected)?;
                let services = device
                    .discover_services()
                    .await
                    .map_err(|_| ConnectError::TargetRejected)?;

                *self.ble_device.lock().unwrap() = Some(device);
                *self.ble_services.lock().unwrap() = Some(services);
                self.ble_chara_cache.lock().unwrap().clear();

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
                let _ = &cb;
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
                let uuid = characteristic.ok_or(SendError::BleCharaNotFound)?;
                tauri::async_runtime::block_on(async {
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
                let uuid = characteristic.ok_or(SubscribeError::BleCharaNotFound)?;
                tauri::async_runtime::block_on(async {
                    if self.ble_device.lock().unwrap().is_none() {
                        return Err(SubscribeError::Disconnected);
                    }
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
                            Err(e) => (cb_clone)(Err(e.to_string())),
                        }
                    });
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
                        if let Some(adapter) = Adapter::default().await {
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
