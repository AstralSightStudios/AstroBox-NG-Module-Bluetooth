use std::sync::Arc;

use crate::btinterface::{BluetoothInterface, Uuid};
use tauri::ipc::Channel;

// TODO: Implement possible ESP32 support.
#[derive(Debug)]
pub struct Esp32Imp;

impl BluetoothInterface for Esp32Imp {
    fn start_scan(
        &self,
        _channel: Channel<crate::btinterface::BluetoothDevice>,
        _connect_type: Option<crate::btinterface::ConnectType>,
    ) -> Result<(), crate::btinterface::ScanError> {
        todo!()
    }

    fn stop_scan(
        &self,
    ) -> Result<Vec<crate::btinterface::BluetoothDevice>, crate::btinterface::ScanError> {
        todo!()
    }

    fn connect(
        &self,
        _addr: String,
        _connect_type: crate::btinterface::ConnectType,
        _fallback_channels: Vec<u8>,
    ) -> Result<(), crate::btinterface::ConnectError> {
        todo!()
    }

    fn set_on_connected_listener(
        &self,
        _addr: &str,
        _connect_type: crate::btinterface::ConnectType,
        _cb: Arc<dyn Fn() + Send + Sync + 'static>,
    ) {
        todo!()
    }

    fn max_send_len(&self, _addr: &str, _characteristic: Option<Uuid>) -> Option<usize> {
        None
    }

    fn send(
        &self,
        _addr: &str,
        _data: Vec<u8>,
        _characteristic: Option<Uuid>,
    ) -> Result<(), crate::btinterface::SendError> {
        todo!()
    }

    fn send_async(
        &self,
        addr: &str,
        data: Vec<u8>,
        characteristic: Option<Uuid>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), crate::btinterface::SendError>> + Send + '_,
        >,
    > {
        Box::pin(async move { self.send(addr, data, characteristic) })
    }

    fn send_many_async(
        &self,
        addr: &str,
        data: Vec<Vec<u8>>,
        characteristic: Option<Uuid>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), crate::btinterface::SendError>> + Send + '_,
        >,
    > {
        Box::pin(async move {
            for item in data {
                self.send(addr, item, characteristic)?;
            }
            Ok(())
        })
    }

    fn subscribe(
        &self,
        _addr: &str,
        _cb: Arc<dyn Fn(Result<Vec<u8>, String>) + Send + Sync>,
        _characteristic: Option<Uuid>,
    ) -> Result<(), crate::btinterface::SubscribeError> {
        todo!()
    }

    fn disconnect(&self, _addr: &str) -> Result<(), crate::btinterface::DisconnectError> {
        todo!()
    }
}
