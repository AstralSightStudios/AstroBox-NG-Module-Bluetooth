//! Routes each device to the transport it lives on: devices on the band
//! emulator's radio go to the emulator bridge, everything else to the
//! platform implementation.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tauri::ipc::Channel;

use crate::btinterface::{
    BluetoothDevice, BluetoothInterface, ConnectError, ConnectType, DisconnectError, ScanError,
    SendError, SubscribeError, Uuid,
};
use crate::emu;

#[derive(Debug)]
pub struct Router {
    platform: Box<dyn BluetoothInterface>,
}

impl Router {
    pub fn new(platform: Box<dyn BluetoothInterface>) -> Self {
        Self { platform }
    }

    fn emulated(addr: &str) -> bool {
        emu::bridge().owns(addr)
    }
}

impl BluetoothInterface for Router {
    fn start_scan(
        &self,
        channel: Channel<BluetoothDevice>,
        connect_type: Option<ConnectType>,
    ) -> Result<(), ScanError> {
        let emulator = if emu::bridge().attached() && connect_type != Some(ConnectType::BLE) {
            emu::bridge().start_scan(channel.clone())
        } else {
            Err(ScanError::AdapterNotFound)
        };
        match self.platform.start_scan(channel, connect_type) {
            Err(_) if emulator.is_ok() => Ok(()),
            other => other,
        }
    }

    fn stop_scan(&self) -> Result<Vec<BluetoothDevice>, ScanError> {
        let mut found = emu::bridge().stop_scan();
        match self.platform.stop_scan() {
            Ok(list) => found.extend(list),
            Err(err) if found.is_empty() => return Err(err),
            Err(_) => {}
        }
        Ok(found)
    }

    fn connect(
        &self,
        addr: String,
        connect_type: ConnectType,
        spp_fallback_channels: Vec<u8>,
        unpair_before_connect: Option<bool>,
    ) -> Result<(), ConnectError> {
        if Self::emulated(&addr) {
            // unpair_before_connect is the Android workaround the frontend
            // passes on every platform; the bridge re-pairs by itself when the
            // band has lost our key, so dropping the key would only make the
            // band ask for confirmation on every connect.
            return emu::bridge().connect(&addr, &spp_fallback_channels, false);
        }
        self.platform
            .connect(addr, connect_type, spp_fallback_channels, unpair_before_connect)
    }

    fn set_on_connected_listener(
        &self,
        addr: &str,
        connect_type: ConnectType,
        cb: Arc<dyn Fn() + Send + Sync + 'static>,
    ) {
        if Self::emulated(addr) {
            emu::bridge().set_on_connected(cb);
        } else {
            self.platform.set_on_connected_listener(addr, connect_type, cb);
        }
    }

    fn max_send_len(&self, addr: &str, characteristic: Option<Uuid>) -> Option<usize> {
        if Self::emulated(addr) {
            return emu::bridge().max_send_len();
        }
        self.platform.max_send_len(addr, characteristic)
    }

    fn send(&self, addr: &str, data: Vec<u8>, characteristic: Option<Uuid>) -> Result<(), SendError> {
        if Self::emulated(addr) {
            return emu::bridge().send(&data);
        }
        self.platform.send(addr, data, characteristic)
    }

    fn send_async(
        &self,
        addr: &str,
        data: Vec<u8>,
        characteristic: Option<Uuid>,
    ) -> Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + '_>> {
        if Self::emulated(addr) {
            return emu::bridge().send_async(vec![data]);
        }
        self.platform.send_async(addr, data, characteristic)
    }

    fn send_many_async(
        &self,
        addr: &str,
        data: Vec<Vec<u8>>,
        characteristic: Option<Uuid>,
    ) -> Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + '_>> {
        if Self::emulated(addr) {
            return emu::bridge().send_async(data);
        }
        self.platform.send_many_async(addr, data, characteristic)
    }

    fn subscribe(
        &self,
        addr: &str,
        cb: Arc<dyn Fn(Result<Vec<u8>, String>) + Send + Sync>,
        characteristic: Option<Uuid>,
    ) -> Result<(), SubscribeError> {
        if Self::emulated(addr) {
            return emu::bridge().subscribe(cb);
        }
        self.platform.subscribe(addr, cb, characteristic)
    }

    fn disconnect(&self, addr: &str) -> Result<(), DisconnectError> {
        if Self::emulated(addr) {
            return emu::bridge().disconnect();
        }
        self.platform.disconnect(addr)
    }
}
