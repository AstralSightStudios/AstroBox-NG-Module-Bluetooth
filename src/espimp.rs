use std::sync::Arc;

use crate::btinterface::BluetoothInterface;
use tauri::ipc::Channel;

// TODO: 实现可能的ESP32单片机支持
#[derive(Debug)]
pub struct Esp32Imp;
impl BluetoothInterface for Esp32Imp {
    fn start_scan(&self, _channel: Channel<crate::btinterface::BluetoothDevice>) -> Result<(), crate::btinterface::ScanError> {
        todo!()
    }

    fn stop_scan(&self) -> Result<Vec<crate::btinterface::BluetoothDevice>, crate::btinterface::ScanError> {
        todo!()
    }

    fn connect(&self, _addr: String) -> Result<(), crate::btinterface::ConnectError> {
        todo!()
    }

    fn set_on_connected_listener(&self, _cb: Arc<dyn Fn() + Send + Sync + 'static>){
        todo!()
    }

    fn send(&self, _data: Vec<u8>, _characteristic: Option<bluest::Uuid>) -> Result<(), crate::btinterface::SendError> {
        todo!()
    }

    fn subscribe(
        &self,
        _cb: std::sync::Arc<dyn Fn(Result<Vec<u8>, String>) + Send + Sync>,
        _characteristic: Option<bluest::Uuid>,
    ) -> Result<(), crate::btinterface::SubscribeError> {
        todo!()
    }

    fn disconnect(&self) -> Result<(), crate::btinterface::DisconnectError> {
        todo!()
    }
}