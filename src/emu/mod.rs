//! Bluetooth bridge to the band emulator (qemu-miwear).
//!
//! The emulator's virtual radio joins the band's Bluetooth controller to a
//! second, standard HCI controller that it serves over TCP. This module is
//! the host stack on that controller: devices it discovers there are routed
//! here by [`crate::router::Router`], so the regular connect / bind flows
//! run unchanged against the emulated band over SPP.

mod codec;
mod session;

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;
use tauri::ipc::Channel;

pub use session::{FoundDevice, SessionStatus};
use session::Session;

use crate::btinterface::{
    BluetoothDevice, ConnectError, ConnectType, DisconnectError, ScanError, SendError,
    SubscribeError,
};

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStatus {
    pub attached: bool,
    pub endpoint: Option<String>,
    #[serde(flatten)]
    pub session: SessionStatus,
}

#[derive(Default)]
struct Bridge {
    session: Option<Arc<Session>>,
    endpoint: Option<String>,
}

pub struct EmuBridge {
    inner: Mutex<Bridge>,
}

impl std::fmt::Debug for EmuBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmuBridge").finish_non_exhaustive()
    }
}

/// The process-wide bridge (one emulator at a time).
pub fn bridge() -> &'static EmuBridge {
    static BRIDGE: OnceLock<EmuBridge> = OnceLock::new();
    BRIDGE.get_or_init(|| EmuBridge { inner: Mutex::new(Bridge::default()) })
}

impl EmuBridge {
    fn session(&self) -> Option<Arc<Session>> {
        let mut inner = self.inner.lock().unwrap();
        if inner.session.as_ref().is_some_and(|s| !s.alive()) {
            inner.session = None;
        }
        inner.session.clone()
    }

    /// Attach to the emulator's peer HCI socket and look for the band.
    /// Link keys are kept in `key_store` so a band bonds only once.
    pub fn attach(&self, host: &str, port: u16, key_store: Option<PathBuf>) -> Result<BridgeStatus, String> {
        self.detach();
        let session = Session::open(host, port, key_store)?;
        {
            let mut inner = self.inner.lock().unwrap();
            inner.session = Some(session.clone());
            inner.endpoint = Some(format!("{host}:{port}"));
        }
        // a short inquiry so the band is known before anyone scans
        session.inquiry(2, None)?;
        Ok(self.status())
    }

    pub fn detach(&self) {
        let session = {
            let mut inner = self.inner.lock().unwrap();
            inner.endpoint = None;
            inner.session.take()
        };
        if let Some(s) = session {
            s.disconnect();
            s.close();
        }
    }

    pub fn status(&self) -> BridgeStatus {
        let session = self.session();
        BridgeStatus {
            attached: session.is_some(),
            endpoint: self.inner.lock().unwrap().endpoint.clone(),
            session: session.map(|s| s.status()).unwrap_or_default(),
        }
    }

    /// Look for the band again (it may have booted after the attach).
    pub fn rescan(&self) -> Result<(), String> {
        let session = self.session().ok_or("the emulator bridge is not attached")?;
        session.inquiry(4, None)
    }

    /// Bridge log lines from index `since`; returns the next index.
    pub fn log(&self, since: usize) -> (usize, Vec<String>) {
        self.session().map(|s| s.log_lines(since)).unwrap_or((0, Vec::new()))
    }

    /// Whether `addr` is a device on the emulator's radio.
    pub fn owns(&self, addr: &str) -> bool {
        match (self.session(), codec::parse_bdaddr(&crate::stdimp::normalize_addr_for_dedup(addr))) {
            (Some(s), Some(a)) => s.knows(&a),
            _ => false,
        }
    }

    pub fn attached(&self) -> bool {
        self.session().is_some()
    }

    pub fn start_scan(&self, channel: Channel<BluetoothDevice>) -> Result<(), ScanError> {
        let session = self.session().ok_or(ScanError::AdapterNotFound)?;
        let on_found = Arc::new(move |addr: String, name: String| {
            let _ = channel.send(BluetoothDevice { name, addr, connect_type: Some(ConnectType::SPP) });
        });
        session.inquiry(8, Some(on_found)).map_err(|err| {
            log::warn!(target: "emubt", "inquiry failed: {err}");
            ScanError::AdapterNotFound
        })
    }

    pub fn stop_scan(&self) -> Vec<BluetoothDevice> {
        let Some(session) = self.session() else { return Vec::new() };
        session
            .stop_inquiry()
            .into_iter()
            .map(|d| BluetoothDevice { name: d.name, addr: d.addr, connect_type: Some(ConnectType::SPP) })
            .collect()
    }

    pub fn connect(&self, addr: &str, fallback: &[u8], unpair: bool) -> Result<(), ConnectError> {
        let session = self.session().ok_or(ConnectError::DeviceNotFound)?;
        let a = codec::parse_bdaddr(&crate::stdimp::normalize_addr_for_dedup(addr))
            .ok_or(ConnectError::DeviceNotFound)?;
        session.connect(a, fallback, unpair).map_err(|err| {
            log::warn!(target: "emubt", "connect {addr} failed: {err}");
            ConnectError::TargetRejected
        })
    }

    pub fn set_on_connected(&self, cb: Arc<dyn Fn() + Send + Sync + 'static>) {
        if let Some(s) = self.session() {
            s.set_connected_callback(Some(cb));
        }
    }

    pub fn max_send_len(&self) -> Option<usize> {
        self.session()?.max_send_len()
    }

    pub fn send(&self, data: &[u8]) -> Result<(), SendError> {
        let session = self.session().ok_or(SendError::Disconnected)?;
        session.send(data).map_err(|err| {
            log::warn!(target: "emubt", "send failed: {err}");
            SendError::Disconnected
        })
    }

    pub fn send_async(&self, data: Vec<Vec<u8>>) -> Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + 'static>> {
        let session = self.session();
        Box::pin(async move {
            let session = session.ok_or(SendError::Disconnected)?;
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let r = data.iter().try_for_each(|d| session.send(d));
                let _ = tx.send(r);
            });
            match rx.await {
                Ok(Ok(())) => Ok(()),
                _ => Err(SendError::Disconnected),
            }
        })
    }

    pub fn subscribe(&self, cb: Arc<dyn Fn(Result<Vec<u8>, String>) + Send + Sync>) -> Result<(), SubscribeError> {
        let session = self.session().ok_or(SubscribeError::Disconnected)?;
        session.set_data_callback(Some(cb));
        Ok(())
    }

    pub fn disconnect(&self) -> Result<(), DisconnectError> {
        let session = self.session().ok_or(DisconnectError::DeviceNotFound)?;
        session.disconnect();
        session.set_data_callback(None);
        Ok(())
    }
}
