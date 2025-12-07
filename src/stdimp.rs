// TODO: 等待蓝牙重构以替换掉这个庞大的转接文件

use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

#[cfg(target_os = "android")]
use crate::btinterface::ble_stub::{Adapter, BluestDevice, BluestService, Characteristic};

#[cfg(not(target_os = "android"))]
use bluest::{
    Adapter, Characteristic, Device as BluestDevice, Service as BluestService,
    error::ErrorKind as BluestErrorKind,
};

use bluest::Uuid;

use btclassic_spp::BtclassicSppExt;
use futures_util::{
    StreamExt,
    future::{AbortHandle, Abortable},
};
use tauri::{AppHandle, Wry};
use tokio::sync::oneshot;

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
    scan_state: Arc<Mutex<Option<ScanSession>>>,
    scan_seq: AtomicUsize,
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

#[derive(Debug)]
enum ScanSession {
    Spp {
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
            connect_type,
            ble_device: Mutex::new(None),
            ble_services: Mutex::new(None),
            ble_chara_cache: Mutex::new(HashMap::new()),
            ble_char_bundle: Mutex::new(None),
            #[cfg(not(target_os = "android"))]
            ble_scanned_devices: Arc::new(Mutex::new(HashMap::new())),
            ble_on_connected: Mutex::new(None),
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
                self.ble_chara_cache
                    .lock()
                    .unwrap()
                    .insert(cuuid, c.clone());
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
                    });

                    if let Err(err) = channel.send(BluetoothDevice {
                        name,
                        addr: addr.clone(),
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

impl BluetoothInterface for StdImp {
    fn start_scan(&self, channel: tauri::ipc::Channel<BluetoothDevice>) -> Result<(), ScanError> {
        log::info!(
            "StdImp::start_scan invoked for {:?}; current scan_state active={} (linux={})",
            self.connect_type,
            self.scan_state.lock().unwrap().is_some(),
            cfg!(target_os = "linux")
        );
        match self.connect_type {
            ConnectType::SPP => {
                let app = Self::app().ok_or(ScanError::AdapterNotFound)?;

                let (abort_reg, session_id) = {
                    let mut guard = self.scan_state.lock().unwrap();
                    if guard.is_some() {
                        return Ok(());
                    }
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
                                        let name = dev.device.name().unwrap_or_default().to_string();
                                        let raw_addr = dev.device.id().to_string();
                                        let key = normalize_addr_for_dedup(&raw_addr);
                                        if key.is_empty() {
                                            continue;
                                        }
                                        if known_addrs.insert(key) {
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
            ConnectType::BLE => Err(ScanError::AdapterNotFound),
        }
    }

    fn stop_scan(&self) -> Result<Vec<BluetoothDevice>, ScanError> {
        match self.connect_type {
            ConnectType::SPP => {
                let session = {
                    let mut guard = self.scan_state.lock().unwrap();
                    guard.take()
                };
                let mut was_scanning = false;
                if let Some(session) = session {
                    match session {
                        ScanSession::Spp { abort_handle, .. } => {
                            was_scanning = true;
                            abort_handle.abort();
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
                if let Some(session) = {
                    let mut guard = self.scan_state.lock().unwrap();
                    guard.take()
                } {
                    match session {
                        ScanSession::Spp { abort_handle, .. } => {
                            abort_handle.abort();
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

                #[cfg(target_os = "ios")]
                if device_opt.is_none() {
                    device_opt = self
                        .ble_probe_connected_device_by_addr(adapter, &addr)
                        .await;
                    if let Some(dev) = &device_opt {
                        self.ble_scanned_devices
                            .lock()
                            .unwrap()
                            .insert(addr.clone(), dev.clone());
                    }
                }

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

                #[cfg(target_os = "ios")]
                let already_connected = device.is_connected().await;
                #[cfg(not(target_os = "ios"))]
                let already_connected = false;

                if !already_connected {
                    adapter
                        .connect_device(&device)
                        .await
                        .map_err(|_| ConnectError::TargetRejected)?;
                    log::info!(
                        "StdImp::connect (BLE) controller connection established addr={}",
                        addr
                    );
                } else {
                    log::info!(
                        "StdImp::connect (BLE) controller already connected at system level addr={}",
                        addr
                    );
                }

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
                    log::debug!(
                        "StdImp::send (BLE) writing {} bytes to char {}",
                        data.len(),
                        uuid
                    );
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
                    log::info!(
                        "StdImp::subscribe (BLE) attempting to subscribe to {}",
                        uuid
                    );
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
                        if let Ok(service_chara) =
                            self.ble_get_or_discover_chara(service_uuid).await
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
