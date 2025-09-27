pub mod btinterface;
pub mod stdimp;

use std::sync::OnceLock;

use btinterface::BluetoothInterface;

static GLOBAL_BT: OnceLock<Box<dyn BluetoothInterface>> = OnceLock::new();

pub fn set_interface(
    interface: Box<dyn BluetoothInterface>,
) -> Result<(), Box<dyn BluetoothInterface>> {
    GLOBAL_BT.set(interface)
}

pub fn get_interface() -> Option<&'static dyn BluetoothInterface> {
    GLOBAL_BT.get().map(|b| b.as_ref())
}
