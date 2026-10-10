//! cargo run -p bluetooth --example emulator -- <port> <state-directory> [bind]
//! Confirm pairing/binding on the emulator's screen. No firmware patches.
use std::{path::PathBuf, sync::Arc, time::Duration};
use bluetooth::emu::bridge;
use corelib::device::{DeviceKind, xiaomi::{components::{auth::AuthComponent, bind::{LocalBindConfig, XiaomiConnectOptions}}, packet::dispatcher, r#type::ConnectType}};

struct Logger;
impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool { m.level() <= log::Level::Info }
    fn log(&self, r: &log::Record) { if self.enabled(r.metadata()) { eprintln!("{} {}", r.level(), r.args()); } }
    fn flush(&self) {}
}
static LOGGER: Logger = Logger;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Info);
    let args: Vec<_> = std::env::args().collect();
    let port: u16 = args.get(1).ok_or("missing port")?.parse()?;
    let dir = PathBuf::from(args.get(2).ok_or("missing state directory")?);
    std::fs::create_dir_all(&dir)?;
    let keys = dir.join("link-keys");
    tokio::task::spawn_blocking(move || bridge().attach("127.0.0.1", port, Some(keys))).await??;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let status = bridge().status();
    let found = status.session.devices.first().ok_or("band not found")?.clone();
    let addr = found.addr.clone();
    tokio::task::spawn_blocking(move || bridge().connect(&addr, &[5, 1], false).map_err(|e| format!("{e:?}"))).await??;
    corelib::ecs::init_runtime_default();
    let handle = tokio::runtime::Handle::current();
    let addr = found.addr.clone();
    let rx_handle = handle.clone();
    bridge().subscribe(Arc::new(move |data| {
        match data {
            Ok(data) => dispatcher::on_packet(rx_handle.clone(), addr.clone(), data),
            Err(err) => log::error!("Transport: {err}"),
        }
    })).map_err(|e| format!("{e:?}"))?;
    let bind = args.get(3).is_some_and(|s| s == "bind");
    let (authkey, options) = if bind {
        (String::new(), XiaomiConnectOptions {
            local_bind: Some(LocalBindConfig { user_id: "astrobox-emulator".into(), app_device_id: "astrobox-emulator-host".into() }),
            app_device_id: None,
        })
    } else {
        let credentials: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("credentials.json"))?)?;
        (credentials["authkey"].as_str().ok_or("missing authkey")?.to_owned(), XiaomiConnectOptions {
            local_bind: None, app_device_id: credentials["appDeviceId"].as_str().map(str::to_owned),
        })
    };
    let info = tokio::time::timeout(Duration::from_secs(120), corelib::device::create_device_with_options(
        handle, DeviceKind::Xiaomi, found.name, found.addr.clone(), authkey, 2,
        ConnectType::SPP, None, bridge().max_send_len(), None, true, options,
        |data| async move {
            bridge().send_async(data).await.map_err(|e| corelib::device::xiaomi::SendError::Io(format!("{e:?}")))
        },
    )).await??;
    let addr = info.addr.clone();
    let addr_for_cred = addr.clone();
    let credentials = corelib::ecs::with_rt_mut(move |rt| {
        let auth = rt.component_ref::<AuthComponent>(&addr_for_cred).unwrap();
        serde_json::json!({ "authkey": auth.authkey, "appDeviceId": auth.app_device_id })
    }).await;
    std::fs::write(dir.join("credentials.json"), serde_json::to_vec_pretty(&credentials)?)?;
    println!("Connected and authenticated: {} ({})", info.name, info.addr);

    let addr_for_info = addr.clone();
    let rx_info = corelib::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr_for_info, |world, entity| {
            let mut info_sys = world.get_mut::<corelib::device::xiaomi::components::info::InfoSystem>(entity).unwrap();
            info_sys.request_device_info()
        }).unwrap()
    }).await;
    match tokio::time::timeout(Duration::from_secs(5), rx_info).await {
        Ok(Ok(Ok(dev_info))) => {
            println!("--- Device Info ---");
            println!("  Serial Number:    {}", dev_info.serial_number);
            println!("  Firmware Version: {}", dev_info.firmware_version);
            println!("  Model:            {}", dev_info.model);
            println!("  Product Device:   {}", dev_info.product_device);
        }
        res => println!("Failed to get device info: {res:?}"),
    }

    let addr_for_status = addr.clone();
    let rx_status = corelib::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr_for_status, |world, entity| {
            let mut info_sys = world.get_mut::<corelib::device::xiaomi::components::info::InfoSystem>(entity).unwrap();
            info_sys.request_device_status()
        }).unwrap()
    }).await;
    match tokio::time::timeout(Duration::from_secs(5), rx_status).await {
        Ok(Ok(Ok(dev_status))) => {
            println!("--- Device Status ---");
            let bat = &dev_status.battery;
            println!("  Battery:          {}%", bat.capacity);
            println!("  Charge Status:    {:?}", bat.charge_status);
        }
        res => println!("Failed to get device status: {res:?}"),
    }

    let duration_secs = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(10);
    println!("Connection active. Running for {duration_secs}s...");
    tokio::time::sleep(Duration::from_secs(duration_secs)).await;
    bridge().detach();
    Ok(())
}
