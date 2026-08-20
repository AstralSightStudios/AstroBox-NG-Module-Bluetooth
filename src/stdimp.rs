// TODO: 等待蓝牙重构以替换掉这个庞大的转接文件

use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

#[cfg(not(target_os = "android"))]
use bluest::{
    error::ErrorKind as BluestErrorKind, Adapter, AdvertisementData, Characteristic,
    Device as BluestDevice, Service as BluestService,
};

use btclassic_spp::BtclassicSppExt;
use futures_util::{
    future::{AbortHandle, Abortable},
    StreamExt,
};
use tauri::{AppHandle, Wry};
use tokio::sync::{oneshot, Mutex as AsyncMutex};

use crate::btinterface::{
    BluetoothDevice, BluetoothInterface, ConnectError, ConnectType, DisconnectError, ScanError,
    SendError, SubscribeError, Uuid,
};

const BLE_UUID_KEYWORD_XIAOMI_SERVICE: &str = "0050";
const BLE_UUID_KEYWORD_XIAOMI_SENT: &str = "005f";
const BLE_UUID_KEYWORD_XIAOMI_RECV: &str = "005e";
const BLE_UUID_VIVO_SERVICE: &str = "0000276008c211e190730e8ac72e1011";
const BLE_UUID_VIVO_SENT: &str = "0000276008c211e190730e8ac72e0011";
const BLE_UUID_VIVO_RECV: &str = "0000276008c211e190730e8ac72e0012";
const VIVO_MANUFACTURER_ID: u16 = 2103;

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
    // Kept for scan selection; connection operations are address-scoped.
    scan_connect_type: ConnectType,

    // The transport for subsequent address-scoped operations is recorded when
    // connect is called. Fallback channels are supplied to connect directly.
    addressed_connect_types: Mutex<HashMap<String, ConnectType>>,
    #[cfg(not(target_os = "android"))]
    addressed_on_connected: Mutex<HashMap<String, Arc<dyn Fn() + Send + Sync + 'static>>>,
    #[cfg(not(target_os = "android"))]
    addressed_ble_sessions: Mutex<HashMap<String, Arc<AddressedBleSession>>>,
    #[cfg(not(target_os = "android"))]
    ble_scanned_devices: Arc<Mutex<HashMap<String, BluestDevice>>>,
    scan_state: Arc<Mutex<Option<ScanSession>>>,
    scan_seq: AtomicUsize,
    scan_results: Arc<Mutex<Vec<BluetoothDevice>>>,
}

impl fmt::Debug for StdImp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StdImp")
            .field("scan_connect_type", &self.scan_connect_type)
            .finish()
    }
}

#[derive(Debug, Clone, Default)]
struct BleCharacteristicBundle {
    service: Option<Uuid>,
    recv: Option<Uuid>,
    sent: Option<Uuid>,
}

#[cfg(not(target_os = "android"))]
struct AddressedBleSession {
    device: BluestDevice,
    services: Vec<BluestService>,
    chara_cache: Arc<Mutex<HashMap<Uuid, Characteristic>>>,
    char_bundle: BleCharacteristicBundle,
    send_lock: Arc<AsyncMutex<()>>,
}

#[derive(Debug, Clone)]
struct VivoAdvertisementInfo {
    protocol_version: u8,
    mask: u16,
    product_id: u16,
    mac: String,
}

#[derive(Debug)]
enum ScanSession {
    Spp {
        id: usize,
        abort_handle: AbortHandle,
    },
    #[cfg(target_os = "android")]
    AndroidBle {
        id: usize,
        abort_handle: AbortHandle,
    },
    Ble {
        id: usize,
        cancel_tx: Option<oneshot::Sender<()>>,
        finished_rx: Option<oneshot::Receiver<()>>,
    },
}

impl StdImp {
    pub fn new(connect_type: ConnectType) -> Self {
        Self {
            scan_connect_type: connect_type,
            addressed_connect_types: Mutex::new(HashMap::new()),
            #[cfg(not(target_os = "android"))]
            addressed_on_connected: Mutex::new(HashMap::new()),
            #[cfg(not(target_os = "android"))]
            addressed_ble_sessions: Mutex::new(HashMap::new()),
            #[cfg(not(target_os = "android"))]
            ble_scanned_devices: Arc::new(Mutex::new(HashMap::new())),
            scan_state: Arc::new(Mutex::new(None)),
            scan_seq: AtomicUsize::new(0),
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
        BLE_ADAPTER.get().ok_or(ConnectError::DeviceNotFound)
    }

    #[cfg(not(target_os = "android"))]
    async fn shutdown_ble_scan(
        cancel_tx: Option<oneshot::Sender<()>>,
        finished_rx: Option<oneshot::Receiver<()>>,
    ) {
        if let Some(tx) = cancel_tx {
            let _ = tx.send(());
        }
        if let Some(rx) = finished_rx {
            let _ = tokio::time::timeout(Duration::from_secs(2), rx).await;
        }
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

    fn uuid_compact_eq(uuid: &Uuid, expected: &str) -> bool {
        let haystack: String = uuid
            .to_string()
            .chars()
            .filter(|c| *c != '-')
            .map(|c| c.to_ascii_lowercase())
            .collect();
        haystack == expected.to_ascii_lowercase()
    }

    fn parse_vivo_manufacturer_data(data: &[u8]) -> Option<VivoAdvertisementInfo> {
        if data.len() < 12 || data.first().copied() != Some(0) {
            return None;
        }

        let protocol_version = data[1];
        let mask = u16::from_le_bytes([data[2], data[3]]);
        let product_id = u16::from_le_bytes([data[4], data[5]]);

        let mac_bytes = &data[6..12];
        if mac_bytes.iter().all(|b| *b == 0) {
            return None;
        }

        let mac = mac_bytes
            .iter()
            .map(|b| format!("{:02X}", b))
            .collect::<Vec<_>>()
            .join(":");

        Some(VivoAdvertisementInfo {
            protocol_version,
            mask,
            product_id,
            mac,
        })
    }

    #[cfg(not(target_os = "android"))]
    fn parse_vivo_advertisement(adv_data: &AdvertisementData) -> Option<VivoAdvertisementInfo> {
        let manufacturer = adv_data.manufacturer_data.as_ref()?;
        if manufacturer.company_id != VIVO_MANUFACTURER_ID {
            return None;
        }
        Self::parse_vivo_manufacturer_data(&manufacturer.data)
    }

    fn is_vivo_watch_name(name: &str) -> bool {
        let normalized = name.trim().to_ascii_lowercase();
        (normalized.starts_with("vivo watch") || normalized.starts_with("iqoo watch"))
            && !normalized.is_empty()
    }

    #[cfg(not(target_os = "android"))]
    fn describe_manufacturer_data(adv_data: &AdvertisementData) -> String {
        match adv_data.manufacturer_data.as_ref() {
            Some(manufacturer) => format!(
                "company_id={} data_len={} data={}",
                manufacturer.company_id,
                manufacturer.data.len(),
                Self::hex_preview(&manufacturer.data, 24)
            ),
            None => "manufacturer_data=none".to_string(),
        }
    }

    fn hex_preview(data: &[u8], limit: usize) -> String {
        let mut out = data
            .iter()
            .take(limit)
            .map(|b| format!("{:02X}", b))
            .collect::<Vec<_>>()
            .join(" ");
        if data.len() > limit {
            out.push_str(" ...");
        }
        out
    }

    #[cfg(target_os = "ios")]
    async fn ble_probe_connected_device_by_addr(
        &self,
        adapter: &Adapter,
        addr: &str,
    ) -> Option<BluestDevice> {
        let normalized_target = normalize_addr_for_dedup(addr);
        match adapter.connected_devices().await {
            Ok(devices) => {
                for dev in devices {
                    let id = dev.id().to_string();
                    let normalized_id = normalize_addr_for_dedup(&id);
                    if id.eq_ignore_ascii_case(addr)
                        || (!normalized_target.is_empty()
                            && !normalized_id.is_empty()
                            && normalized_id.eq_ignore_ascii_case(&normalized_target))
                    {
                        log::info!(
                            "StdImp::ble_probe_connected_device_by_addr reusing already-connected peripheral: addr={} (matched id={})",
                            addr,
                            id
                        );
                        return Some(dev);
                    }
                }
                None
            }
            Err(err) => {
                log::warn!(
                    "StdImp::ble_probe_connected_device_by_addr failed to query connected devices: {}",
                    err
                );
                None
            }
        }
    }

    #[cfg(target_os = "ios")]
    async fn ble_seed_scan_with_connected(
        &self,
        adapter: &Adapter,
        channel: &tauri::ipc::Channel<BluetoothDevice>,
    ) {
        match adapter.connected_devices().await {
            Ok(devices) => {
                for dev in devices {
                    let addr = dev.id().to_string();
                    let key = normalize_addr_for_dedup(&addr);
                    if key.is_empty() {
                        continue;
                    }

                    let mut cache = self.ble_scanned_devices.lock().unwrap();
                    if cache.contains_key(&addr) {
                        continue;
                    }

                    let name = dev.name().unwrap_or_else(|err| {
                        log::debug!(
                            "StdImp::ble_seed_scan_with_connected failed to read device name for {}: {}",
                            addr,
                            err
                        );
                        String::new()
                    });

                    cache.insert(addr.clone(), dev.clone());
                    drop(cache);

                    self.scan_results.lock().unwrap().push(BluetoothDevice {
                        name: name.clone(),
                        addr: addr.clone(),
                        connect_type: Some(ConnectType::BLE),
                    });

                    if let Err(err) = channel.send(BluetoothDevice {
                        name,
                        addr: addr.clone(),
                        connect_type: Some(ConnectType::BLE),
                    }) {
                        log::debug!(
                            "StdImp::ble_seed_scan_with_connected failed to push connected device {}: {:?}",
                            addr,
                            err
                        );
                    }
                }
            }
            Err(err) => {
                log::warn!(
                    "StdImp::ble_seed_scan_with_connected failed to query connected devices: {}",
                    err
                );
            }
        }
    }
}

impl StdImp {
    fn addressed_key(addr: &str) -> String {
        addr.trim().to_string()
    }

    fn addressed_connect_type(&self, addr: &str) -> Option<ConnectType> {
        self.addressed_connect_types
            .lock()
            .unwrap()
            .get(&Self::addressed_key(addr))
            .copied()
    }

    #[cfg(not(target_os = "android"))]
    async fn build_addressed_ble_session(
        device: BluestDevice,
        services: Vec<BluestService>,
    ) -> Result<Arc<AddressedBleSession>, ConnectError> {
        let mut bundle = BleCharacteristicBundle::default();
        let mut chara_cache = HashMap::new();

        for service in &services {
            let service_uuid = service.uuid();
            let is_xiaomi = Self::uuid_contains(&service_uuid, "fe95");
            let is_vivo = Self::uuid_compact_eq(&service_uuid, BLE_UUID_VIVO_SERVICE);
            if !is_xiaomi && !is_vivo {
                continue;
            }

            let chars = service
                .discover_characteristics()
                .await
                .map_err(|_| ConnectError::TargetRejected)?;
            for characteristic in chars {
                let uuid = characteristic.uuid();
                if is_xiaomi && Self::uuid_contains(&uuid, BLE_UUID_KEYWORD_XIAOMI_RECV) {
                    bundle.recv = Some(uuid);
                } else if is_xiaomi && Self::uuid_contains(&uuid, BLE_UUID_KEYWORD_XIAOMI_SENT) {
                    bundle.sent = Some(uuid);
                } else if is_xiaomi && Self::uuid_contains(&uuid, BLE_UUID_KEYWORD_XIAOMI_SERVICE) {
                    bundle.service = Some(uuid);
                } else if is_vivo && Self::uuid_compact_eq(&uuid, BLE_UUID_VIVO_RECV) {
                    bundle.recv = Some(uuid);
                } else if is_vivo && Self::uuid_compact_eq(&uuid, BLE_UUID_VIVO_SENT) {
                    bundle.sent = Some(uuid);
                }
                chara_cache.insert(uuid, characteristic);
            }
        }

        if bundle.recv.is_none() || bundle.sent.is_none() {
            return Err(ConnectError::TargetRejected);
        }

        Ok(Arc::new(AddressedBleSession {
            device,
            services,
            chara_cache: Arc::new(Mutex::new(chara_cache)),
            char_bundle: bundle,
            send_lock: Arc::new(AsyncMutex::new(())),
        }))
    }

    #[cfg(not(target_os = "android"))]
    async fn addressed_ble_get_or_discover_chara(
        session: &AddressedBleSession,
        uuid: Uuid,
    ) -> Result<Characteristic, SubscribeError> {
        if let Some(chara) = session.chara_cache.lock().unwrap().get(&uuid).cloned() {
            return Ok(chara);
        }

        for service in &session.services {
            let chars = service
                .discover_characteristics()
                .await
                .map_err(|_| SubscribeError::Disconnected)?;
            for chara in chars {
                if chara.uuid() == uuid {
                    session
                        .chara_cache
                        .lock()
                        .unwrap()
                        .insert(uuid, chara.clone());
                    return Ok(chara);
                }
            }
        }
        Err(SubscribeError::BleCharaNotFound)
    }

    #[cfg(not(target_os = "android"))]
    async fn connect_addressed_ble(&self, addr: String) -> Result<(), ConnectError> {
        let key = Self::addressed_key(&addr);
        let adapter = Self::ble_adapter().await?;

        if let Some(old_session) = self.addressed_ble_sessions.lock().unwrap().remove(&key) {
            let _ = adapter.disconnect_device(&old_session.device).await;
        }

        let mut device_opt = self.ble_scanned_devices.lock().unwrap().get(&addr).cloned();

        #[cfg(target_os = "ios")]
        if device_opt.is_none() {
            device_opt = self
                .ble_probe_connected_device_by_addr(adapter, &addr)
                .await;
            if let Some(device) = &device_opt {
                self.ble_scanned_devices
                    .lock()
                    .unwrap()
                    .insert(addr.clone(), device.clone());
            }
        }

        if device_opt.is_none() {
            let mut scan = adapter
                .scan(&[])
                .await
                .map_err(|_| ConnectError::DeviceNotFound)?;
            let deadline = Instant::now() + Duration::from_secs(12);
            while Instant::now() < deadline {
                if let Some(item) = scan.next().await {
                    let raw_id = item.device.id().to_string();
                    let vivo_mac =
                        Self::parse_vivo_advertisement(&item.adv_data).map(|info| info.mac);
                    if raw_id.eq_ignore_ascii_case(&addr)
                        || vivo_mac
                            .as_ref()
                            .is_some_and(|mac| mac.eq_ignore_ascii_case(&addr))
                    {
                        let device = item.device.clone();
                        let mut cache = self.ble_scanned_devices.lock().unwrap();
                        cache.insert(raw_id, device.clone());
                        cache.insert(addr.clone(), device.clone());
                        if let Some(mac) = vivo_mac {
                            cache.insert(mac, device.clone());
                        }
                        device_opt = Some(device);
                        break;
                    }
                } else {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                }
            }
        }

        let device = device_opt.ok_or(ConnectError::DeviceNotFound)?;
        #[cfg(target_os = "ios")]
        let already_connected = device.is_connected().await;
        #[cfg(not(target_os = "ios"))]
        let already_connected = false;

        if !already_connected {
            adapter
                .connect_device(&device)
                .await
                .map_err(|_| ConnectError::TargetRejected)?;
        }

        let mut attempts = 0;
        while !device.is_connected().await {
            if attempts >= 20 {
                break;
            }
            attempts += 1;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !device.is_connected().await {
            return Err(ConnectError::TargetRejected);
        }

        if let Err(err) = device.pair().await {
            log::warn!("BLE pair failed for {}: {}", addr, err);
        }
        let services = device
            .discover_services()
            .await
            .map_err(|_| ConnectError::TargetRejected)?;
        let session = Self::build_addressed_ble_session(device, services).await?;
        self.addressed_ble_sessions
            .lock()
            .unwrap()
            .insert(key.clone(), session);

        if let Some(cb) = self
            .addressed_on_connected
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
        {
            cb();
        }
        Ok(())
    }

    #[cfg(not(target_os = "android"))]
    async fn addressed_ble_send_many(
        &self,
        addr: String,
        data: Vec<Vec<u8>>,
        characteristic: Option<Uuid>,
    ) -> Result<(), SendError> {
        let key = Self::addressed_key(&addr);
        let session = self
            .addressed_ble_sessions
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .ok_or(SendError::Disconnected)?;
        let _send_guard = session.send_lock.lock().await;
        let uuid = characteristic
            .or(session.char_bundle.sent)
            .ok_or(SendError::BleCharaNotFound)?;
        if !session.device.is_connected().await {
            return Err(SendError::Disconnected);
        }
        let chara = Self::addressed_ble_get_or_discover_chara(&session, uuid)
            .await
            .map_err(|err| match err {
                SubscribeError::BleCharaNotFound => SendError::BleCharaNotFound,
                SubscribeError::Disconnected => SendError::Disconnected,
            })?;
        for item in data {
            chara
                .write_without_response(&item)
                .await
                .map_err(|_| SendError::Disconnected)?;
        }
        Ok(())
    }

    #[cfg(not(target_os = "android"))]
    fn addressed_ble_max_send_len(
        &self,
        addr: &str,
        characteristic: Option<Uuid>,
    ) -> Option<usize> {
        let key = Self::addressed_key(addr);
        let session = self
            .addressed_ble_sessions
            .lock()
            .unwrap()
            .get(&key)
            .cloned()?;
        let uuid = characteristic.or(session.char_bundle.sent)?;
        tauri::async_runtime::block_on(async {
            Self::addressed_ble_get_or_discover_chara(&session, uuid)
                .await
                .ok()?
                .max_write_len_async()
                .await
                .ok()
        })
        .filter(|len| *len > 0)
    }

    #[cfg(not(target_os = "android"))]
    fn addressed_ble_subscribe(
        &self,
        addr: &str,
        cb: Arc<dyn Fn(Result<Vec<u8>, String>) + Send + Sync>,
        characteristic: Option<Uuid>,
    ) -> Result<(), SubscribeError> {
        let key = Self::addressed_key(addr);
        let session = self
            .addressed_ble_sessions
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .ok_or(SubscribeError::Disconnected)?;
        let uuid = characteristic
            .or(session.char_bundle.recv)
            .ok_or(SubscribeError::BleCharaNotFound)?;
        tauri::async_runtime::block_on(async move {
            if !session.device.is_connected().await {
                return Err(SubscribeError::Disconnected);
            }
            let chara = Self::addressed_ble_get_or_discover_chara(&session, uuid).await?;
            tauri::async_runtime::spawn(async move {
                match chara.notify().await {
                    Ok(mut notify) => {
                        while let Some(item) = notify.next().await {
                            match item {
                                Ok(bytes) => cb(Ok(bytes)),
                                Err(err) => cb(Err(err.to_string())),
                            }
                        }
                        cb(Err("BLE notification stream ended".to_string()));
                    }
                    Err(err) => cb(Err(err.to_string())),
                }
            });
            if let Some(service_uuid) = session.char_bundle.service {
                if let Ok(service_chara) =
                    Self::addressed_ble_get_or_discover_chara(&session, service_uuid).await
                {
                    if let Err(err) = service_chara.read().await {
                        log::debug!("Failed to read Xiaomi service characteristic: {}", err);
                    }
                }
            }
            Ok(())
        })
    }

    #[cfg(not(target_os = "android"))]
    fn disconnect_addressed_ble(&self, addr: &str) -> Result<(), DisconnectError> {
        let key = Self::addressed_key(addr);
        let session = self.addressed_ble_sessions.lock().unwrap().remove(&key);
        self.addressed_on_connected.lock().unwrap().remove(&key);
        let Some(session) = session else {
            return Ok(());
        };
        tauri::async_runtime::block_on(async {
            let adapter = Self::ble_adapter()
                .await
                .map_err(|_| DisconnectError::DeviceNotFound)?;
            adapter
                .disconnect_device(&session.device)
                .await
                .map_err(|_| DisconnectError::DeviceNotFound)
        })
    }
}

impl BluetoothInterface for StdImp {
    fn start_scan(
        &self,
        channel: tauri::ipc::Channel<BluetoothDevice>,
        connect_type: Option<ConnectType>,
    ) -> Result<(), ScanError> {
        let scan_connect_type = connect_type.unwrap_or(self.scan_connect_type);
        log::info!(
            "StdImp::start_scan invoked for {:?}; default transport={:?}; current scan_state active={} (linux={})",
            scan_connect_type,
            self.scan_connect_type,
            self.scan_state.lock().unwrap().is_some(),
            cfg!(target_os = "linux")
        );
        match scan_connect_type {
            ConnectType::SPP => {
                let app = Self::app().ok_or(ScanError::AdapterNotFound)?;

                if let Some(session) = {
                    let mut guard = self.scan_state.lock().unwrap();
                    guard.take()
                } {
                    match session {
                        ScanSession::Spp { id, abort_handle } => {
                            let mut guard = self.scan_state.lock().unwrap();
                            *guard = Some(ScanSession::Spp { id, abort_handle });
                            return Ok(());
                        }
                        #[cfg(target_os = "android")]
                        ScanSession::AndroidBle { abort_handle, .. } => {
                            abort_handle.abort();
                            let _ = app.btclassic_spp().stop_ble_scan();
                        }
                        ScanSession::Ble {
                            cancel_tx,
                            finished_rx,
                            ..
                        } => {
                            #[cfg(not(target_os = "android"))]
                            let _ = tauri::async_runtime::block_on(Self::shutdown_ble_scan(
                                cancel_tx,
                                finished_rx,
                            ));
                            #[cfg(target_os = "android")]
                            let _ = (cancel_tx, finished_rx);
                        }
                    }
                }

                let (abort_reg, session_id) = {
                    let mut guard = self.scan_state.lock().unwrap();
                    let session_id = self
                        .scan_seq
                        .fetch_add(1, Ordering::Relaxed)
                        .wrapping_add(1);
                    let (abort_handle, abort_reg) = AbortHandle::new_pair();
                    *guard = Some(ScanSession::Spp {
                        id: session_id,
                        abort_handle,
                    });
                    (abort_reg, session_id)
                };

                let app_handle = app.clone();
                let scan_state = Arc::clone(&self.scan_state);
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
                                            connect_type: Some(ConnectType::SPP),
                                        });
                                    }
                                }
                            }
                            tokio::time::sleep(Duration::from_millis(800)).await;
                        }
                    };
                    let _ = Abortable::new(fut, abort_reg).await;
                    let mut guard = scan_state.lock().unwrap();
                    if matches!(
                        guard.as_ref(),
                        Some(ScanSession::Spp { id, .. }) if *id == session_id
                    ) {
                        guard.take();
                    }
                });

                app.btclassic_spp()
                    .start_scan()
                    .map_err(|_| ScanError::AdapterNotFound)?;
                Ok(())
            }

            #[cfg(not(target_os = "android"))]
            ConnectType::BLE => tauri::async_runtime::block_on(async {
                log::info!(
                    "StdImp::start_scan (BLE) preparing new scan session; previous state cleared={}",
                    self.scan_state.lock().unwrap().is_none()
                );
                if let Some(session) = {
                    let mut guard = self.scan_state.lock().unwrap();
                    guard.take()
                } {
                    match session {
                        ScanSession::Spp { .. } => {}
                        ScanSession::Ble {
                            cancel_tx,
                            finished_rx,
                            ..
                        } => {
                            #[cfg(not(target_os = "android"))]
                            Self::shutdown_ble_scan(cancel_tx, finished_rx).await;
                            #[cfg(target_os = "android")]
                            let _ = (cancel_tx, finished_rx);
                        }
                    }
                }

                let adapter = Self::ble_adapter()
                    .await
                    .map_err(|_| ScanError::AdapterNotFound)?;

                #[cfg(target_os = "linux")]
                if let Some(app) = Self::app() {
                    log::info!(
                        "StdImp::start_scan (BLE, linux) ensuring btclassic-spp scan stopped before BLE discovery"
                    );
                    if let Err(err) = app.btclassic_spp().stop_scan() {
                        log::info!(
                            "StdImp::start_scan (BLE, linux) pre-emptively stopped SPP scan: {}",
                            err
                        );
                    } else {
                        log::info!(
                            "StdImp::start_scan (BLE, linux) btclassic-spp stop_scan returned success"
                        );
                    }
                }

                self.scan_results.lock().unwrap().clear();
                self.ble_scanned_devices.lock().unwrap().clear();

                #[cfg(target_os = "ios")]
                {
                    self.ble_seed_scan_with_connected(adapter, &channel).await;
                }

                let session_id = self
                    .scan_seq
                    .fetch_add(1, Ordering::Relaxed)
                    .wrapping_add(1);
                let (cancel_tx, cancel_rx) = oneshot::channel();
                let (finished_tx, finished_rx) = oneshot::channel();

                {
                    let mut guard = self.scan_state.lock().unwrap();
                    *guard = Some(ScanSession::Ble {
                        id: session_id,
                        cancel_tx: Some(cancel_tx),
                        finished_rx: Some(finished_rx),
                    });
                }

                let results = Arc::clone(&self.scan_results);
                let scan_state = Arc::clone(&self.scan_state);
                let channel_clone = channel.clone();
                let scanned_devices = Arc::clone(&self.ble_scanned_devices);
                let adapter_clone = adapter.clone();
                let preknown_addrs: HashSet<String> = self
                    .ble_scanned_devices
                    .lock()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect();

                tauri::async_runtime::spawn(async move {
                    let mut cancel_rx = cancel_rx;
                    let mut finish_tx = Some(finished_tx);
                    let mut known_addrs = preknown_addrs;

                    #[cfg(not(target_os = "android"))]
                    const BLE_SCAN_RETRY_LIMIT: usize = 6;
                    #[cfg(not(target_os = "android"))]
                    const BLE_SCAN_RETRY_BASE_DELAY_MS: u64 = 180;

                    let mut retry_index = 0usize;

                    let mut stream = match adapter_clone.scan(&[]).await {
                        Ok(s) => s,
                        Err(mut err) => loop {
                            #[cfg(not(target_os = "android"))]
                            {
                                let err_kind = err.kind();
                                let err_msg = err.message().to_string();
                                log::info!(
                                    "StdImp::start_scan (BLE) scan attempt {} returned error kind={:?} message='{}'",
                                    retry_index + 1,
                                    err_kind,
                                    err_msg
                                );
                                if err.kind() == BluestErrorKind::AlreadyScanning
                                    && retry_index < BLE_SCAN_RETRY_LIMIT
                                {
                                    retry_index += 1;
                                    let delay = Duration::from_millis(
                                        BLE_SCAN_RETRY_BASE_DELAY_MS * retry_index as u64,
                                    );
                                    log::info!(
                                        "StdImp::start_scan (BLE) discovery already active, retry {} after {:?}",
                                        retry_index,
                                        delay
                                    );
                                    tokio::time::sleep(delay).await;
                                    match adapter_clone.scan(&[]).await {
                                        Ok(stream) => break stream,
                                        Err(next_err) => {
                                            err = next_err;
                                            continue;
                                        }
                                    }
                                }
                            }
                            let attempts = retry_index + 1;
                            log::warn!(
                                "StdImp::start_scan (BLE) failed to start discovery after {} attempt(s): kind={:?}, message='{}', display={}",
                                attempts,
                                err.kind(),
                                err.message(),
                                err
                            );
                            if let Some(tx) = finish_tx.take() {
                                let _ = tx.send(());
                            }
                            let mut guard = scan_state.lock().unwrap();
                            if matches!(
                                guard.as_ref(),
                                Some(ScanSession::Ble { id, .. }) if *id == session_id
                            ) {
                                guard.take();
                            }
                            return;
                        },
                    };

                    loop {
                        tokio::select! {
                            _ = &mut cancel_rx => {
                                break;
                            }
                            maybe_dev = stream.next() => {
                                match maybe_dev {
                                    Some(dev) => {
                                        let name = dev.adv_data.local_name.clone().unwrap_or_else(|| {
                                            dev.device.name().unwrap_or_default().to_string()
                                        });
                                        let raw_addr = dev.device.id().to_string();
                                        let vivo_adv = StdImp::parse_vivo_advertisement(&dev.adv_data);

                                        #[cfg(target_os = "ios")]
                                        let (emit_addr, emit_connect_type) = {
                                            if let Some(info) = &vivo_adv {
                                                log::info!(
                                                    "StdImp::start_scan (BLE) parsed vivo advertisement id={} mac={} product_id={} mask=0x{:04x} protocol={}",
                                                    raw_addr,
                                                    info.mac,
                                                    info.product_id,
                                                    info.mask,
                                                    info.protocol_version
                                                );
                                            }
                                            (raw_addr.clone(), ConnectType::BLE)
                                        };

                                        #[cfg(not(target_os = "ios"))]
                                        let (emit_addr, emit_connect_type) = {
                                            if let Some(info) = vivo_adv {
                                                log::info!(
                                                    "StdImp::start_scan (BLE) parsed vivo advertisement id={} mac={} product_id={} mask=0x{:04x} protocol={}, emitting SPP target",
                                                    raw_addr,
                                                    info.mac,
                                                    info.product_id,
                                                    info.mask,
                                                    info.protocol_version
                                                );
                                                (info.mac, ConnectType::SPP)
                                            } else if Self::is_vivo_watch_name(&name) {
                                                log::warn!(
                                                    "StdImp::start_scan (BLE) saw vivo-like device '{}' id={} but could not parse real MAC from advertisement ({}); emitting BLE fallback target",
                                                    name,
                                                    raw_addr,
                                                    Self::describe_manufacturer_data(&dev.adv_data)
                                                );
                                                (raw_addr.clone(), ConnectType::BLE)
                                            } else {
                                                continue;
                                            }
                                        };

                                        let key = normalize_addr_for_dedup(&emit_addr);
                                        if key.is_empty() {
                                            continue;
                                        }
                                        if known_addrs.insert(key) {
                                            let mut cache = scanned_devices.lock().unwrap();
                                            cache.insert(raw_addr.clone(), dev.device.clone());
                                            if emit_addr != raw_addr {
                                                cache.insert(emit_addr.clone(), dev.device.clone());
                                            }
                                            drop(cache);
                                            results.lock().unwrap().push(BluetoothDevice {
                                                name: name.clone(),
                                                addr: emit_addr.clone(),
                                                connect_type: Some(emit_connect_type),
                                            });
                                            let _ = channel_clone.send(BluetoothDevice {
                                                name,
                                                addr: emit_addr,
                                                connect_type: Some(emit_connect_type),
                                            });
                                        }
                                    }
                                    None => break,
                                }
                            }
                        }
                    }

                    if let Some(tx) = finish_tx.take() {
                        let _ = tx.send(());
                    }

                    let mut guard = scan_state.lock().unwrap();
                    if matches!(
                        guard.as_ref(),
                        Some(ScanSession::Ble { id, .. }) if *id == session_id
                    ) {
                        guard.take();
                    }
                });
                Ok(())
            }),

            #[cfg(target_os = "android")]
            ConnectType::BLE => {
                let app = Self::app().ok_or(ScanError::AdapterNotFound)?;

                if let Some(session) = {
                    let mut guard = self.scan_state.lock().unwrap();
                    guard.take()
                } {
                    match session {
                        ScanSession::Spp { abort_handle, .. } => {
                            abort_handle.abort();
                            let _ = app.btclassic_spp().stop_scan();
                        }
                        ScanSession::AndroidBle { abort_handle, .. } => {
                            abort_handle.abort();
                            let _ = app.btclassic_spp().stop_ble_scan();
                        }
                        ScanSession::Ble { .. } => {}
                    }
                }

                self.scan_results.lock().unwrap().clear();

                let session_id = self
                    .scan_seq
                    .fetch_add(1, Ordering::Relaxed)
                    .wrapping_add(1);
                let (abort_handle, abort_reg) = AbortHandle::new_pair();
                {
                    let mut guard = self.scan_state.lock().unwrap();
                    *guard = Some(ScanSession::AndroidBle {
                        id: session_id,
                        abort_handle,
                    });
                }

                let app_handle = app.clone();
                let scan_state = Arc::clone(&self.scan_state);
                let results = Arc::clone(&self.scan_results);
                tauri::async_runtime::spawn(async move {
                    let fut = async move {
                        let mut known_addrs = HashSet::<String>::new();
                        loop {
                            if let Ok(res) = app_handle.btclassic_spp().get_ble_scanned_devices() {
                                for d in res.ret {
                                    let name = d.name.unwrap_or_default();
                                    let raw_addr = d.address;
                                    let key = normalize_addr_for_dedup(&raw_addr);
                                    if key.is_empty() {
                                        continue;
                                    }
                                    if known_addrs.insert(key) {
                                        let item = BluetoothDevice {
                                            name,
                                            addr: raw_addr,
                                            connect_type: Some(ConnectType::BLE),
                                        };
                                        results.lock().unwrap().push(item.clone());
                                        let _ = channel.send(item);
                                    }
                                }
                            }
                            tokio::time::sleep(Duration::from_millis(400)).await;
                        }
                    };
                    let _ = Abortable::new(fut, abort_reg).await;
                    let mut guard = scan_state.lock().unwrap();
                    if matches!(
                        guard.as_ref(),
                        Some(ScanSession::AndroidBle { id, .. }) if *id == session_id
                    ) {
                        guard.take();
                    }
                });

                app.btclassic_spp()
                    .start_ble_scan()
                    .map_err(|_| ScanError::AdapterNotFound)?;
                Ok(())
            }
        }
    }

    fn stop_scan(&self) -> Result<Vec<BluetoothDevice>, ScanError> {
        let session = {
            let mut guard = self.scan_state.lock().unwrap();
            guard.take()
        };

        let stop_spp_and_collect = |was_scanning: bool| -> Result<Vec<BluetoothDevice>, ScanError> {
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
                        connect_type: Some(ConnectType::SPP),
                    });
                }
            }
            Ok(out)
        };

        #[cfg(target_os = "android")]
        let stop_android_ble_and_collect =
            |was_scanning: bool| -> Result<Vec<BluetoothDevice>, ScanError> {
                let app = Self::app().ok_or(ScanError::AdapterNotFound)?;
                let spp = app.btclassic_spp();

                if was_scanning {
                    let _ = spp
                        .stop_ble_scan()
                        .map_err(|_| ScanError::AdapterNotFound)?;
                }

                let result = spp
                    .get_ble_scanned_devices()
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
                            connect_type: Some(ConnectType::BLE),
                        });
                    }
                }
                Ok(out)
            };

        match session {
            Some(ScanSession::Spp { abort_handle, .. }) => {
                abort_handle.abort();
                stop_spp_and_collect(true)
            }
            #[cfg(target_os = "android")]
            Some(ScanSession::AndroidBle { abort_handle, .. }) => {
                abort_handle.abort();
                stop_android_ble_and_collect(true)
            }
            Some(ScanSession::Ble {
                cancel_tx,
                finished_rx,
                ..
            }) => {
                #[cfg(not(target_os = "android"))]
                let _ =
                    tauri::async_runtime::block_on(Self::shutdown_ble_scan(cancel_tx, finished_rx));
                #[cfg(target_os = "android")]
                let _ = (cancel_tx, finished_rx);
                Ok(self.scan_results.lock().unwrap().drain(..).collect())
            }
            None => match self.scan_connect_type {
                ConnectType::SPP => stop_spp_and_collect(false),
                #[cfg(target_os = "android")]
                ConnectType::BLE => stop_android_ble_and_collect(false),
                #[cfg(not(target_os = "android"))]
                ConnectType::BLE => Ok(self.scan_results.lock().unwrap().drain(..).collect()),
            },
        }
    }

    fn connect(
        &self,
        addr: String,
        connect_type: ConnectType,
        spp_fallback_channels: Vec<u8>,
    ) -> Result<(), ConnectError> {
        let key = Self::addressed_key(&addr);
        self.addressed_connect_types
            .lock()
            .unwrap()
            .insert(key, connect_type);
        let fallback_channels = if spp_fallback_channels.is_empty() {
            vec![5, 1]
        } else {
            spp_fallback_channels
                .into_iter()
                .filter(|channel| *channel != 0)
                .collect()
        };
        let fallback_channels = if fallback_channels.is_empty() {
            vec![5, 1]
        } else {
            fallback_channels
        };

        match connect_type {
            ConnectType::SPP => {
                let app = Self::app().ok_or(ConnectError::DeviceNotFound)?;
                app.btclassic_spp()
                    .connect(&addr, true, &fallback_channels)
                    .map_err(|_| ConnectError::DeviceNotFound)
                    .and_then(|result| {
                        if result.ret {
                            Ok(())
                        } else {
                            Err(ConnectError::TargetRejected)
                        }
                    })
            }
            #[cfg(not(target_os = "android"))]
            ConnectType::BLE => tauri::async_runtime::block_on(self.connect_addressed_ble(addr)),
            #[cfg(target_os = "android")]
            ConnectType::BLE => {
                let app = Self::app().ok_or(ConnectError::DeviceNotFound)?;
                app.btclassic_spp()
                    .connect_ble(&addr)
                    .map_err(|_| ConnectError::DeviceNotFound)
                    .and_then(|result| {
                        if result.ret {
                            Ok(())
                        } else {
                            Err(ConnectError::TargetRejected)
                        }
                    })
            }
        }
    }

    fn set_on_connected_listener(
        &self,
        addr: &str,
        connect_type: ConnectType,
        cb: Arc<dyn Fn() + Send + Sync + 'static>,
    ) {
        let key = Self::addressed_key(addr);
        self.addressed_connect_types
            .lock()
            .unwrap()
            .insert(key.clone(), connect_type);
        match connect_type {
            ConnectType::SPP => {
                if let Some(app) = Self::app() {
                    let cb2 = cb.clone();
                    let _ = app.btclassic_spp().on_connected(addr, move || {
                        (cb2)();
                    });
                }
            }
            #[cfg(not(target_os = "android"))]
            ConnectType::BLE => {
                self.addressed_on_connected
                    .lock()
                    .unwrap()
                    .insert(key.clone(), cb.clone());
                if self
                    .addressed_ble_sessions
                    .lock()
                    .unwrap()
                    .contains_key(&key)
                {
                    cb();
                }
            }
            #[cfg(target_os = "android")]
            ConnectType::BLE => {
                if let Some(app) = Self::app() {
                    let cb2 = cb.clone();
                    let _ = app.btclassic_spp().on_connected(addr, move || {
                        (cb2)();
                    });
                }
            }
        }
    }

    fn max_send_len(&self, addr: &str, characteristic: Option<Uuid>) -> Option<usize> {
        match self.addressed_connect_type(addr)? {
            ConnectType::SPP => Self::app()?
                .btclassic_spp()
                .get_max_send_len(addr)
                .ok()
                .flatten()
                .filter(|len| *len > 0),
            #[cfg(not(target_os = "android"))]
            ConnectType::BLE => self.addressed_ble_max_send_len(addr, characteristic),
            #[cfg(target_os = "android")]
            ConnectType::BLE => Self::app()?
                .btclassic_spp()
                .get_ble_max_send_len(addr)
                .ok()
                .flatten()
                .filter(|len| *len > 0),
        }
    }

    fn send(
        &self,
        addr: &str,
        data: Vec<u8>,
        characteristic: Option<Uuid>,
    ) -> Result<(), SendError> {
        match self
            .addressed_connect_type(addr)
            .ok_or(SendError::Disconnected)?
        {
            ConnectType::SPP => Self::app()
                .ok_or(SendError::Disconnected)?
                .btclassic_spp()
                .send(addr, &data)
                .map_err(|_| SendError::Disconnected),
            #[cfg(not(target_os = "android"))]
            ConnectType::BLE => tauri::async_runtime::block_on(self.addressed_ble_send_many(
                addr.to_string(),
                vec![data],
                characteristic,
            )),
            #[cfg(target_os = "android")]
            ConnectType::BLE => Self::app()
                .ok_or(SendError::Disconnected)?
                .btclassic_spp()
                .send(addr, &data)
                .map_err(|_| SendError::Disconnected),
        }
    }

    fn send_async(
        &self,
        addr: &str,
        data: Vec<u8>,
        characteristic: Option<Uuid>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SendError>> + Send + '_>>
    {
        #[cfg(not(target_os = "android"))]
        if self.addressed_connect_type(addr) == Some(ConnectType::BLE) {
            return Box::pin(self.addressed_ble_send_many(
                addr.to_string(),
                vec![data],
                characteristic,
            ));
        }
        let addr = addr.to_owned();
        Box::pin(async move { self.send(&addr, data, characteristic) })
    }

    fn send_many_async(
        &self,
        addr: &str,
        data: Vec<Vec<u8>>,
        characteristic: Option<Uuid>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SendError>> + Send + '_>>
    {
        #[cfg(not(target_os = "android"))]
        if self.addressed_connect_type(addr) == Some(ConnectType::BLE) {
            return Box::pin(self.addressed_ble_send_many(addr.to_string(), data, characteristic));
        }
        let addr = addr.to_owned();
        Box::pin(async move {
            for item in data {
                self.send(&addr, item, characteristic)?;
            }
            Ok(())
        })
    }

    fn subscribe(
        &self,
        addr: &str,
        cb: Arc<dyn Fn(Result<Vec<u8>, String>) + Send + Sync>,
        characteristic: Option<Uuid>,
    ) -> Result<(), SubscribeError> {
        match self
            .addressed_connect_type(addr)
            .ok_or(SubscribeError::Disconnected)?
        {
            ConnectType::SPP => {
                let app = Self::app().ok_or(SubscribeError::Disconnected)?;
                app.btclassic_spp()
                    .set_data_listener(addr, move |result| cb(result))
                    .map_err(|_| SubscribeError::Disconnected)?;
                app.btclassic_spp()
                    .start_subscription(addr)
                    .map_err(|_| SubscribeError::Disconnected)
            }
            #[cfg(not(target_os = "android"))]
            ConnectType::BLE => self.addressed_ble_subscribe(addr, cb, characteristic),
            #[cfg(target_os = "android")]
            ConnectType::BLE => {
                let app = Self::app().ok_or(SubscribeError::Disconnected)?;
                app.btclassic_spp()
                    .set_data_listener(addr, move |result| cb(result))
                    .map_err(|_| SubscribeError::Disconnected)?;
                app.btclassic_spp()
                    .start_ble_subscription(addr)
                    .map_err(|_| SubscribeError::Disconnected)
            }
        }
    }

    fn disconnect(&self, addr: &str) -> Result<(), DisconnectError> {
        let connect_type = self.addressed_connect_type(addr);
        let result = match connect_type {
            Some(ConnectType::SPP) => Self::app()
                .ok_or(DisconnectError::DeviceNotFound)?
                .btclassic_spp()
                .disconnect(addr)
                .map_err(|_| DisconnectError::DeviceNotFound),
            #[cfg(not(target_os = "android"))]
            Some(ConnectType::BLE) => self.disconnect_addressed_ble(addr),
            #[cfg(target_os = "android")]
            Some(ConnectType::BLE) => Self::app()
                .ok_or(DisconnectError::DeviceNotFound)?
                .btclassic_spp()
                .disconnect_ble(addr)
                .map_err(|_| DisconnectError::DeviceNotFound),
            None => Ok(()),
        };
        self.addressed_connect_types
            .lock()
            .unwrap()
            .remove(&Self::addressed_key(addr));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::StdImp;

    #[test]
    fn vivo_manufacturer_data_matches_java_scan_record_layout() {
        let data = [
            0x00, 0x03, 0x11, 0x00, 0x34, 0x12, 0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6,
        ];

        let info = StdImp::parse_vivo_manufacturer_data(&data).unwrap();

        assert_eq!(info.protocol_version, 3);
        assert_eq!(info.mask, 0x0011);
        assert_eq!(info.product_id, 0x1234);
        assert_eq!(info.mac, "A1:B2:C3:D4:E5:F6");
    }

    #[test]
    fn vivo_manufacturer_data_rejects_non_vivo_payloads() {
        assert!(StdImp::parse_vivo_manufacturer_data(&[1, 2, 3]).is_none());
        assert!(StdImp::parse_vivo_manufacturer_data(&[0; 12]).is_none());
    }

    #[test]
    fn vivo_manufacturer_data_accepts_zero_product_id_like_official_app() {
        let data = [
            0x00, 0x03, 0x11, 0x00, 0x00, 0x00, 0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6,
        ];

        let info = StdImp::parse_vivo_manufacturer_data(&data).unwrap();

        assert_eq!(info.product_id, 0);
        assert_eq!(info.mac, "A1:B2:C3:D4:E5:F6");
    }
}
