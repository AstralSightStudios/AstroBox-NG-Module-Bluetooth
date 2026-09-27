use serde::{Deserialize, Serialize};
use std::fmt::{self, Debug, Display};
use std::future::Future;
use std::pin::Pin;
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
    use std::fmt;

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

    impl fmt::Display for Uuid {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let b = self.0;
            write!(
                f,
                "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                b[0],
                b[1],
                b[2],
                b[3],
                b[4],
                b[5],
                b[6],
                b[7],
                b[8],
                b[9],
                b[10],
                b[11],
                b[12],
                b[13],
                b[14],
                b[15]
            )
        }
    }
}

#[cfg(target_os = "android")]
pub use ble_stub::Uuid;

#[cfg(not(target_os = "android"))]
pub use bluest::Uuid;

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
    Disconnected => "SendError: disconnected",
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
    Disconnected => "SubscribeError: disconnected",
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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BluetoothDevice {
    pub name: String,
    pub addr: String,
    #[serde(
        default,
        rename = "connectType",
        skip_serializing_if = "Option::is_none"
    )]
    pub connect_type: Option<ConnectType>,
}

pub trait BluetoothInterface: Send + Sync + Debug {
    fn start_scan(
        &self,
        channel: Channel<BluetoothDevice>,
        connect_type: Option<ConnectType>,
    ) -> Result<(), ScanError>;
    fn stop_scan(&self) -> Result<Vec<BluetoothDevice>, ScanError>;

    /// Connect one physical device. The address is part of every connection
    /// operation; implementations must never replace another address here.
    fn connect(
        &self,
        addr: String,
        connect_type: ConnectType,
        spp_fallback_channels: Vec<u8>,
        unpair_before_connect: Option<bool>,
    ) -> Result<(), ConnectError>;
    fn set_on_connected_listener(
        &self,
        addr: &str,
        connect_type: ConnectType,
        cb: Arc<dyn Fn() + Send + Sync + 'static>,
    );
    fn max_send_len(&self, addr: &str, characteristic: Option<Uuid>) -> Option<usize>;
    fn send(
        &self,
        addr: &str,
        data: Vec<u8>,
        characteristic: Option<Uuid>,
    ) -> Result<(), SendError>;
    fn send_async(
        &self,
        addr: &str,
        data: Vec<u8>,
        characteristic: Option<Uuid>,
    ) -> Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + '_>>;
    fn send_many_async(
        &self,
        addr: &str,
        data: Vec<Vec<u8>>,
        characteristic: Option<Uuid>,
    ) -> Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + '_>>;
    fn subscribe(
        &self,
        addr: &str,
        cb: Arc<dyn Fn(Result<Vec<u8>, String>) + Send + Sync>,
        characteristic: Option<Uuid>,
    ) -> Result<(), SubscribeError>;
    fn disconnect(&self, addr: &str) -> Result<(), DisconnectError>;
}
