use bluest::Uuid;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Debug, Display};
use std::sync::Arc;
use tauri::ipc::Channel;

/// Helper macro to implement Display and Error for simple enums
macro_rules! impl_error {
    ($name:ident, $($variant:ident => $msg:expr),+ $(,)?) => {
        impl Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    $(Self::$variant => write!(f, $msg),)+
                }
            }
        }

        impl std::error::Error for $name {}
    };
}

// Android 下不引入 bluest，提供桩类型
#[cfg(target_os = "android")]
pub mod ble_stub {
    #[derive(Debug, Clone)]
    pub struct Adapter;
    #[derive(Debug, Clone)]
    pub struct Characteristic;
    #[derive(Debug, Clone)]
    pub struct BluestDevice;
    #[derive(Debug, Clone)]
    pub struct BluestService;
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Uuid([u8; 16]);
}

#[derive(Debug)]
pub enum ScanError {
    MissingPermission,
    AdapterNotFound,
}

impl_error!(
    ScanError,
    MissingPermission => "missing permission",
    AdapterNotFound => "adapter not found",
);

#[derive(Debug)]
pub enum ConnectError {
    DeviceNotFound,
    TargetRejected,
}

impl_error!(
    ConnectError,
    DeviceNotFound => "device not found",
    TargetRejected => "target rejected",
);

#[derive(Debug)]
pub enum SendError {
    Disconnected,
    BleCharaNotFound,
    TooLong,
}

impl_error!(
    SendError,
    Disconnected => "disconnected",
    BleCharaNotFound => "ble characteristic not found",
    TooLong => "data too long",
);

#[derive(Debug)]
pub enum SubscribeError {
    Disconnected,
    BleCharaNotFound,
}

impl_error!(
    SubscribeError,
    Disconnected => "disconnected",
    BleCharaNotFound => "ble characteristic not found",
);

#[derive(Debug)]
pub enum DisconnectError {
    DeviceNotFound,
}

impl_error!(
    DisconnectError,
    DeviceNotFound => "device not found",
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ConnectType {
    SPP = 0,
    BLE = 1,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BluetoothDevice {
    pub name: String,
    pub addr: String,
}

pub trait BluetoothInterface: Send + Sync + Debug {
    fn start_scan(&self, channel: Channel<BluetoothDevice>) -> Result<(), ScanError>;
    fn stop_scan(&self) -> Result<Vec<BluetoothDevice>, ScanError>;
    fn connect(&self, addr: String) -> Result<(), ConnectError>;
    fn set_on_connected_listener(&self, cb: Arc<dyn Fn() + Send + Sync + 'static>);
    fn send(&self, data: Vec<u8>, characteristic: Option<Uuid>) -> Result<(), SendError>;
    fn subscribe(
        &self,
        cb: Arc<dyn Fn(Result<Vec<u8>, String>) + Send + Sync>,
        characteristic: Option<Uuid>,
    ) -> Result<(), SubscribeError>;
    fn disconnect(&self) -> Result<(), DisconnectError>;
}
