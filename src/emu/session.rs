//! A minimal BR/EDR host for the emulator bridge.
//!
//! The emulator exposes the peer side of its virtual radio as an H4 HCI
//! controller on a socket. This host drives it like a phone would: it
//! discovers the band, pages it, bonds with Secure Simple Pairing (as a
//! NoInputNoOutput device, so the band asks its user to confirm), turns on
//! encryption, finds the SPP server channel over SDP and opens it over
//! RFCOMM with credit based flow control. It also answers what the band
//! asks of a phone: L2CAP information/echo requests and SDP queries (with
//! no records), refusing every other channel.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::codec::*;

pub type DataCallback = Arc<dyn Fn(Result<Vec<u8>, String>) + Send + Sync>;
pub type ConnectedCallback = Arc<dyn Fn() + Send + Sync>;
pub type FoundCallback = Arc<dyn Fn(String, String) + Send + Sync>;

const LOCAL_NAME: &str = "AstroBox";
/// Class of Device: Phone / Smartphone, with networking/audio/telephony.
const LOCAL_COD: [u8; 3] = [0x0C, 0x02, 0x5A];
const L2CAP_MTU: u16 = 1013;
const RFCOMM_CREDITS: u8 = 7;
const IO_NO_INPUT_NO_OUTPUT: u8 = 0x03;
/// Authentication_Requirements: MITM not required, general bonding.
const AUTH_GENERAL_BONDING: u8 = 0x04;
const LOG_LINES: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChanState {
    WaitConnRsp,
    Config,
    Open,
    Closed,
}

#[derive(Debug)]
struct L2Chan {
    psm: u16,
    local: u16,
    remote: u16,
    initiator: bool,
    state: ChanState,
    remote_mtu: u16,
    conf_in: bool,
    conf_out: bool,
    refused: Option<u16>,
}

#[derive(Default)]
struct SdpClient {
    cid: u16,
    tid: u16,
    request: Vec<u8>,
    lists: Vec<u8>,
    result: Option<Result<Vec<u8>, String>>,
}

#[derive(Default)]
struct Rfcomm {
    cid: u16,
    mux_up: Option<bool>,
    dlci: u8,
    pn: Option<(usize, u8, bool)>,
    ua: Option<bool>,
    open: bool,
    cfc: bool,
    n1: usize,
    tx_credits: u32,
    rx_given: u32,
    closed: bool,
}

struct Link {
    addr: [u8; 6],
    handle: u16,
    connected: Option<u8>,
    disconnected: Option<u8>,
    auth: Option<u8>,
    encrypt: Option<(u8, u8)>,
    rx: Vec<u8>,
    ident: u8,
    next_cid: u16,
    chans: Vec<L2Chan>,
    sdp: SdpClient,
    rfcomm: Rfcomm,
}

impl Link {
    fn new(addr: [u8; 6]) -> Self {
        Self {
            addr,
            handle: 0,
            connected: None,
            disconnected: None,
            auth: None,
            encrypt: None,
            rx: Vec::new(),
            ident: 0,
            next_cid: 0x0040,
            chans: Vec::new(),
            sdp: SdpClient::default(),
            rfcomm: Rfcomm::default(),
        }
    }

    fn up(&self) -> bool {
        self.connected == Some(0) && self.disconnected.is_none()
    }

    fn ident(&mut self) -> u8 {
        self.ident = self.ident.wrapping_add(1).max(1);
        self.ident
    }

    fn alloc_cid(&mut self) -> u16 {
        let cid = self.next_cid;
        self.next_cid += 1;
        cid
    }

    fn chan(&mut self, local: u16) -> Option<&mut L2Chan> {
        self.chans.iter_mut().find(|c| c.local == local)
    }
}

/// What the bridge shows about itself.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatus {
    pub local_addr: String,
    pub devices: Vec<FoundDevice>,
    pub link: Option<String>,
    pub encrypted: bool,
    pub rfcomm_channel: Option<u8>,
    pub spp_open: bool,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FoundDevice {
    pub addr: String,
    pub name: String,
}

#[derive(Default)]
struct Stack {
    local: [u8; 6],
    acl_mtu: usize,
    cmd_done: Option<(u16, Vec<u8>)>,
    inquiring: bool,
    on_found: Option<FoundCallback>,
    known: Vec<([u8; 6], String)>,
    link: Option<Link>,
    keys: HashMap<[u8; 6], [u8; 16]>,
    key_store: Option<PathBuf>,
    data_cb: Option<DataCallback>,
    connected_cb: Option<ConnectedCallback>,
    log: Vec<String>,
    /// lines dropped from the front of `log`, so indices stay absolute
    log_dropped: usize,
    tx_bytes: u64,
    rx_bytes: u64,
    /// work for the reader thread once the state lock is released
    deliveries: Vec<Vec<u8>>,
    found: Vec<(String, String)>,
    link_lost: bool,
    out: Vec<Vec<u8>>,
}

impl Stack {
    fn note(&mut self, msg: String) {
        log::info!(target: "emubt", "{msg}");
        if self.log.len() >= LOG_LINES {
            self.log.remove(0);
            self.log_dropped += 1;
        }
        self.log.push(msg);
    }

    fn command(&mut self, opcode: u16, params: &[u8]) {
        self.out.push(hci_command(opcode, params));
    }

    /// One L2CAP frame to the band, fragmented to the controller's ACL size.
    fn l2cap(&mut self, frame: Vec<u8>) {
        let Some(link) = self.link.as_ref() else { return };
        let handle = link.handle;
        let mtu = self.acl_mtu.max(27);
        for (i, chunk) in frame.chunks(mtu).enumerate() {
            self.out.push(hci_acl(handle, i == 0, chunk));
        }
    }

    fn signal(&mut self, code: u8, ident: u8, data: &[u8]) {
        self.l2cap(l2cap_signal(code, ident, data));
    }

    fn remember(&mut self, addr: [u8; 6], name: Option<String>) {
        let name = name.unwrap_or_default();
        match self.known.iter_mut().find(|(a, _)| *a == addr) {
            Some(entry) => {
                if !name.is_empty() {
                    entry.1 = name;
                }
            }
            None => self.known.push((addr, name)),
        }
    }

    fn save_keys(&self) {
        let Some(path) = self.key_store.as_ref() else { return };
        let text: String = self
            .keys
            .iter()
            .map(|(a, k)| {
                let key: String = k.iter().map(|b| format!("{b:02x}")).collect();
                format!("{}={key}\n", format_bdaddr(a))
            })
            .collect();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(err) = std::fs::write(path, text) {
            log::warn!(target: "emubt", "failed to store link keys: {err}");
        }
    }

    fn load_keys(&mut self) {
        let Some(path) = self.key_store.as_ref() else { return };
        let Ok(text) = std::fs::read_to_string(path) else { return };
        for line in text.lines() {
            let Some((addr, key)) = line.split_once('=') else { continue };
            let (Some(addr), true) = (parse_bdaddr(addr), key.len() == 32) else { continue };
            let mut k = [0u8; 16];
            if (0..16).all(|i| {
                u8::from_str_radix(&key[2 * i..2 * i + 2], 16)
                    .map(|b| k[i] = b)
                    .is_ok()
            }) {
                self.keys.insert(addr, k);
                self.remember(addr, None);
            }
        }
    }

    /* ---- HCI events ---- */

    fn on_event(&mut self, code: u8, p: &[u8]) {
        match code {
            0x0E if p.len() >= 3 => self.cmd_done = Some((le16(p, 1), p[3..].to_vec())),
            0x0F if p.len() >= 4 => self.cmd_done = Some((le16(p, 2), vec![p[0]])),
            0x01 => self.inquiring = false,
            0x02 | 0x22 | 0x2F if p.len() >= 7 => {
                let mut addr = [0u8; 6];
                addr.copy_from_slice(&p[1..7]);
                let name = if code == 0x2F && p.len() >= 15 { eir_name(&p[15..]) } else { None };
                let shown = name.clone().unwrap_or_else(|| format_bdaddr(&addr));
                self.note(format!("inquiry: found {} ({shown})", format_bdaddr(&addr)));
                self.remember(addr, name);
                self.found.push((format_bdaddr(&addr), shown));
            }
            0x07 if p.len() >= 7 => {
                let mut addr = [0u8; 6];
                addr.copy_from_slice(&p[1..7]);
                if p[0] == 0 {
                    self.remember(addr, Some(cstr(&p[7..])));
                }
            }
            0x03 if p.len() >= 11 => {
                let status = p[0];
                let handle = le16(p, 1) & 0x0FFF;
                if let Some(link) = self.link.as_mut().filter(|l| l.addr == p[3..9]) {
                    link.connected = Some(status);
                    link.handle = handle;
                }
                self.note(format!(
                    "connection complete: status 0x{status:02x} handle 0x{handle:03x}"
                ));
            }
            0x04 if p.len() >= 6 => {
                // the band paging us: this host only connects out
                let mut rp = p[0..6].to_vec();
                rp.push(0x0F);
                self.command(0x040A, &rp);
                self.note(format!("refused connection from {}", format_bdaddr(&p[0..6])));
            }
            0x05 if p.len() >= 4 => {
                let reason = p[3];
                if let Some(link) = self.link.as_mut() {
                    if link.handle == le16(p, 1) & 0x0FFF {
                        link.disconnected = Some(reason);
                        let was_open = link.rfcomm.open;
                        link.rfcomm.open = false;
                        link.rfcomm.closed = true;
                        if was_open {
                            self.link_lost = true;
                        }
                    }
                }
                self.note(format!("disconnected: reason 0x{reason:02x}"));
            }
            0x06 if p.len() >= 3 => {
                if let Some(link) = self.link.as_mut() {
                    link.auth = Some(p[0]);
                }
                self.note(format!("authentication complete: status 0x{:02x}", p[0]));
            }
            0x08 if p.len() >= 4 => {
                if let Some(link) = self.link.as_mut() {
                    link.encrypt = Some((p[0], p[3]));
                }
                self.note(format!("encryption change: status 0x{:02x} mode {}", p[0], p[3]));
            }
            0x17 if p.len() >= 6 => {
                let mut addr = [0u8; 6];
                addr.copy_from_slice(&p[0..6]);
                match self.keys.get(&addr).copied() {
                    Some(key) => {
                        let mut rp = addr.to_vec();
                        rp.extend_from_slice(&key);
                        self.command(0x040B, &rp);
                        self.note("link key request: stored key".into());
                    }
                    None => {
                        self.command(0x040C, &addr);
                        self.note("link key request: no key, pairing".into());
                    }
                }
            }
            0x18 if p.len() >= 23 => {
                let mut addr = [0u8; 6];
                addr.copy_from_slice(&p[0..6]);
                let mut key = [0u8; 16];
                key.copy_from_slice(&p[6..22]);
                self.keys.insert(addr, key);
                self.save_keys();
                self.note(format!("bonded with {} (key type {})", format_bdaddr(&addr), p[22]));
            }
            0x31 if p.len() >= 6 => {
                let mut rp = p[0..6].to_vec();
                rp.extend_from_slice(&[IO_NO_INPUT_NO_OUTPUT, 0x00, AUTH_GENERAL_BONDING]);
                self.command(0x042B, &rp);
            }
            0x33 if p.len() >= 10 => {
                let value = u32::from_le_bytes([p[6], p[7], p[8], p[9]]);
                self.note(format!(
                    "pairing: confirm on the band (code {value:06}); accepted here"
                ));
                self.command(0x042C, &p[0..6]);
            }
            0x34 if p.len() >= 6 => self.command(0x042F, &p[0..6]),
            0x36 if p.len() >= 7 => {
                self.note(format!("simple pairing complete: status 0x{:02x}", p[0]));
            }
            _ => {}
        }
    }

    /* ---- ACL / L2CAP ---- */

    fn on_acl(&mut self, handle_flags: u16, data: &[u8]) {
        let Some(link) = self.link.as_mut() else { return };
        if handle_flags & 0x0FFF != link.handle {
            return;
        }
        if (handle_flags >> 12) & 0x3 == 0x1 {
            link.rx.extend_from_slice(data);
        } else {
            link.rx = data.to_vec();
        }
        if link.rx.len() < 4 || link.rx.len() < 4 + le16(&link.rx, 0) as usize {
            return;
        }
        let frame = std::mem::take(&mut link.rx);
        let len = le16(&frame, 0) as usize;
        let cid = le16(&frame, 2);
        let payload = frame[4..4 + len].to_vec();
        if cid == L2CAP_SIGNALING_CID {
            let mut at = 0;
            while at + 4 <= payload.len() {
                let n = le16(&payload, at + 2) as usize;
                if at + 4 + n > payload.len() {
                    break;
                }
                self.on_signal(payload[at], payload[at + 1], &payload[at + 4..at + 4 + n]);
                at += 4 + n;
            }
            return;
        }
        let Some(chan) = self.link.as_mut().and_then(|l| l.chan(cid)) else { return };
        match (chan.psm, chan.initiator) {
            (PSM_SDP, true) => self.on_sdp_response(&payload),
            (PSM_SDP, false) => {
                let remote = chan.remote;
                self.on_sdp_request(remote, &payload);
            }
            (PSM_RFCOMM, _) => self.on_rfcomm(&payload),
            _ => {}
        }
    }

    fn send_conf_req(&mut self, remote: u16) {
        let Some(link) = self.link.as_mut() else { return };
        let ident = link.ident();
        let mut d = remote.to_le_bytes().to_vec();
        d.extend_from_slice(&[0, 0, 0x01, 0x02]);
        d.extend_from_slice(&L2CAP_MTU.to_le_bytes());
        self.signal(SIG_CONF_REQ, ident, &d);
    }

    fn on_signal(&mut self, code: u8, ident: u8, d: &[u8]) {
        match code {
            SIG_CONN_REQ if d.len() >= 4 => {
                let (psm, scid) = (le16(d, 0), le16(d, 2));
                let Some(link) = self.link.as_mut() else { return };
                if psm == PSM_SDP {
                    let local = link.alloc_cid();
                    link.chans.push(L2Chan {
                        psm,
                        local,
                        remote: scid,
                        initiator: false,
                        state: ChanState::Config,
                        remote_mtu: 672,
                        conf_in: false,
                        conf_out: false,
                        refused: None,
                    });
                    let mut rsp = local.to_le_bytes().to_vec();
                    rsp.extend_from_slice(&scid.to_le_bytes());
                    rsp.extend_from_slice(&[0, 0, 0, 0]);
                    self.signal(SIG_CONN_RSP, ident, &rsp);
                    self.send_conf_req(scid);
                } else {
                    let mut rsp = vec![0, 0];
                    rsp.extend_from_slice(&scid.to_le_bytes());
                    rsp.extend_from_slice(&[0x02, 0x00, 0, 0]); // PSM not supported
                    self.signal(SIG_CONN_RSP, ident, &rsp);
                    self.note(format!("refused L2CAP channel for PSM 0x{psm:04x}"));
                }
            }
            SIG_CONN_RSP if d.len() >= 8 => {
                let (dcid, scid, result) = (le16(d, 0), le16(d, 2), le16(d, 4));
                let Some(chan) = self.link.as_mut().and_then(|l| l.chan(scid)) else { return };
                match result {
                    0 => {
                        chan.remote = dcid;
                        chan.state = ChanState::Config;
                        self.send_conf_req(dcid);
                    }
                    1 => {}
                    other => {
                        chan.state = ChanState::Closed;
                        chan.refused = Some(other);
                    }
                }
            }
            SIG_CONF_REQ if d.len() >= 4 => {
                let dcid = le16(d, 0);
                let Some(chan) = self.link.as_mut().and_then(|l| l.chan(dcid)) else { return };
                chan.remote_mtu = l2cap_conf_mtu(&d[4..]).unwrap_or(672);
                chan.conf_in = true;
                if chan.conf_out {
                    chan.state = ChanState::Open;
                }
                let remote = chan.remote;
                let mut rsp = remote.to_le_bytes().to_vec();
                rsp.extend_from_slice(&[0, 0, 0, 0]);
                self.signal(SIG_CONF_RSP, ident, &rsp);
            }
            SIG_CONF_RSP if d.len() >= 6 => {
                let (scid, result) = (le16(d, 0), le16(d, 4));
                let Some(chan) = self.link.as_mut().and_then(|l| l.chan(scid)) else { return };
                if result == 0 {
                    chan.conf_out = true;
                    if chan.conf_in {
                        chan.state = ChanState::Open;
                    }
                } else {
                    chan.state = ChanState::Closed;
                    chan.refused = Some(result);
                }
            }
            SIG_DISC_REQ if d.len() >= 4 => {
                let dcid = le16(d, 0);
                if let Some(link) = self.link.as_mut() {
                    if let Some(chan) = link.chan(dcid) {
                        chan.state = ChanState::Closed;
                    }
                    if link.rfcomm.cid == dcid && link.rfcomm.open {
                        link.rfcomm.open = false;
                        link.rfcomm.closed = true;
                        self.link_lost = true;
                    }
                }
                self.signal(SIG_DISC_RSP, ident, &d[0..4]);
            }
            SIG_DISC_RSP if d.len() >= 4 => {
                let scid = le16(d, 2);
                if let Some(chan) = self.link.as_mut().and_then(|l| l.chan(scid)) {
                    chan.state = ChanState::Closed;
                }
            }
            SIG_ECHO_REQ => self.signal(SIG_ECHO_RSP, ident, d),
            SIG_INFO_REQ if d.len() >= 2 => {
                let kind = le16(d, 0);
                let mut rsp = kind.to_le_bytes().to_vec();
                match kind {
                    0x0002 => rsp.extend_from_slice(&[0, 0, 0, 0, 0, 0]), // basic mode only
                    0x0003 => {
                        rsp.extend_from_slice(&[0, 0]);
                        rsp.extend_from_slice(&[0x02, 0, 0, 0, 0, 0, 0, 0]);
                    }
                    _ => rsp.extend_from_slice(&[0x01, 0x00]),
                }
                self.signal(SIG_INFO_RSP, ident, &rsp);
            }
            SIG_INFO_RSP | SIG_ECHO_RSP | SIG_COMMAND_REJECT => {}
            _ => self.signal(SIG_COMMAND_REJECT, ident, &[0, 0]),
        }
    }

    /* ---- SDP ---- */

    fn send_sdp(&mut self, remote: u16, pdu: u8, tid: u16, params: &[u8]) {
        let mut p = vec![pdu];
        p.extend_from_slice(&tid.to_be_bytes());
        p.extend_from_slice(&(params.len() as u16).to_be_bytes());
        p.extend_from_slice(params);
        self.l2cap(l2cap_frame(remote, &p));
    }

    /// The band's queries of this host: it offers no services.
    fn on_sdp_request(&mut self, remote: u16, p: &[u8]) {
        if p.len() < 5 {
            return;
        }
        let tid = be16(p, 1);
        match p[0] {
            0x02 => self.send_sdp(remote, 0x03, tid, &[0, 0, 0, 0, 0]),
            0x04 => self.send_sdp(remote, 0x01, tid, &[0x00, 0x02]),
            0x06 => self.send_sdp(remote, 0x07, tid, &[0x00, 0x02, 0x35, 0x00, 0x00]),
            _ => self.send_sdp(remote, 0x01, tid, &[0x00, 0x03]),
        }
    }

    fn on_sdp_response(&mut self, p: &[u8]) {
        let Some(link) = self.link.as_mut() else { return };
        let sdp = &mut link.sdp;
        if p.len() < 5 || be16(p, 1) != sdp.tid {
            return;
        }
        if p[0] != 0x07 || p.len() < 7 {
            sdp.result = Some(Err(format!("SDP error response 0x{:02x}", p[0])));
            return;
        }
        let count = be16(p, 5) as usize;
        let Some(lists) = p.get(7..7 + count) else {
            sdp.result = Some(Err("short SDP response".into()));
            return;
        };
        sdp.lists.extend_from_slice(lists);
        let cont = p.get(7 + count..).unwrap_or(&[]).to_vec();
        if cont.first().copied().unwrap_or(0) == 0 {
            sdp.result = Some(Ok(std::mem::take(&mut sdp.lists)));
            return;
        }
        // more to come: repeat the request with the continuation state
        sdp.tid = sdp.tid.wrapping_add(1);
        let (cid, tid) = (sdp.cid, sdp.tid);
        let mut params = sdp.request.clone();
        params.extend_from_slice(&cont);
        let remote = link.chan(cid).map(|c| c.remote).unwrap_or(0);
        self.send_sdp(remote, 0x06, tid, &params);
    }

    /* ---- RFCOMM ---- */

    fn rfcomm_send(&mut self, dlci: u8, cr: bool, control: u8, credits: Option<u8>, info: &[u8]) {
        let Some(link) = self.link.as_ref() else { return };
        let cid = link.rfcomm.cid;
        let Some(remote) = link.chans.iter().find(|c| c.local == cid).map(|c| c.remote) else {
            return;
        };
        let frame = rfcomm_frame(dlci, cr, control, credits, info);
        self.l2cap(l2cap_frame(remote, &frame));
    }

    fn on_rfcomm(&mut self, b: &[u8]) {
        let Some(link) = self.link.as_ref() else { return };
        let cfc = link.rfcomm.cfc;
        let Some(f) = rfcomm_parse(b, cfc) else { return };
        let dlci_ours = link.rfcomm.dlci;
        match (f.dlci, f.control) {
            (0, RF_UA) => self.link.as_mut().unwrap().rfcomm.mux_up = Some(true),
            (0, RF_DM) => self.link.as_mut().unwrap().rfcomm.mux_up = Some(false),
            (0, RF_UIH) => self.on_mux(&f.info),
            (0, RF_DISC) => {
                self.rfcomm_send(0, false, RF_UA | RF_PF, None, &[]);
                self.rfcomm_closed();
            }
            (d, RF_UA) if d == dlci_ours => self.link.as_mut().unwrap().rfcomm.ua = Some(true),
            (d, RF_DM) if d == dlci_ours => {
                let rf = &mut self.link.as_mut().unwrap().rfcomm;
                rf.ua = Some(false);
                self.rfcomm_closed();
            }
            (d, RF_DISC) => {
                self.rfcomm_send(d, false, RF_UA | RF_PF, None, &[]);
                if d == dlci_ours {
                    self.rfcomm_closed();
                }
            }
            (d, RF_SABM) => {
                // the band opening a DLC to this host: it has no servers
                self.rfcomm_send(d, false, RF_DM | RF_PF, None, &[]);
            }
            (d, RF_UIH) if d == dlci_ours => {
                let rf = &mut self.link.as_mut().unwrap().rfcomm;
                if let Some(c) = f.credits {
                    rf.tx_credits += c as u32;
                }
                if f.info.is_empty() {
                    return;
                }
                self.rx_bytes += f.info.len() as u64;
                self.deliveries.push(f.info);
                if rf.cfc {
                    rf.rx_given = rf.rx_given.saturating_sub(1);
                    if rf.rx_given <= 2 {
                        let grant = RFCOMM_CREDITS as u32 - rf.rx_given;
                        rf.rx_given += grant;
                        self.rfcomm_send(d, true, RF_UIH, Some(grant as u8), &[]);
                    }
                }
            }
            _ => {}
        }
    }

    fn rfcomm_closed(&mut self) {
        let Some(link) = self.link.as_mut() else { return };
        if link.rfcomm.open {
            self.link_lost = true;
        }
        link.rfcomm.open = false;
        link.rfcomm.closed = true;
    }

    fn on_mux(&mut self, m: &[u8]) {
        if m.len() < 2 {
            return;
        }
        let kind = m[0] >> 2;
        let command = m[0] & 0x02 != 0;
        let len = (m[1] >> 1) as usize;
        let v = m.get(2..2 + len).unwrap_or(&[]).to_vec();
        match (kind, command) {
            (MUX_PN, false) if v.len() >= 8 => {
                let cfc = v[1] & 0xF0 == 0xE0;
                let n1 = le16(&v, 4) as usize;
                if let Some(link) = self.link.as_mut() {
                    link.rfcomm.pn = Some((n1, v[7], cfc));
                }
            }
            (MUX_PN, true) if v.len() >= 8 => {
                let mut r = v.clone();
                if r[1] & 0xF0 == 0xF0 {
                    r[1] = (r[1] & 0x0F) | 0xE0;
                }
                r[7] = RFCOMM_CREDITS;
                self.rfcomm_send(0, true, RF_UIH, None, &rfcomm_mux(MUX_PN, false, &r));
            }
            (MUX_MSC, true) | (MUX_RLS, true) | (MUX_TEST, true) => {
                self.rfcomm_send(0, true, RF_UIH, None, &rfcomm_mux(kind, false, &v));
            }
            (MUX_RPN, true) => {
                let r = if v.len() >= 8 {
                    v
                } else {
                    // 115200 8N1, no flow control, all parameters
                    vec![v.first().copied().unwrap_or(0), 0x07, 0x03, 0x00, 0x11, 0x13, 0x7F, 0x3F]
                };
                self.rfcomm_send(0, true, RF_UIH, None, &rfcomm_mux(MUX_RPN, false, &r));
            }
            (MUX_FCON, true) | (MUX_FCOFF, true) => {
                self.rfcomm_send(0, true, RF_UIH, None, &rfcomm_mux(kind, false, &[]));
            }
            (_, true) if kind != MUX_NSC => {
                self.rfcomm_send(0, true, RF_UIH, None, &rfcomm_mux(MUX_NSC, false, &[m[0]]));
            }
            _ => {}
        }
    }
}

pub struct Session {
    st: Mutex<Stack>,
    cv: Condvar,
    tx: Mutex<mpsc::Sender<Vec<u8>>>,
    stream: TcpStream,
    alive: AtomicBool,
    cmd_lock: Mutex<()>,
    op_lock: Mutex<()>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

impl Session {
    pub fn open(host: &str, port: u16, key_store: Option<PathBuf>) -> Result<Arc<Self>, String> {
        let stream = TcpStream::connect((host, port))
            .map_err(|e| format!("cannot reach the emulator at {host}:{port}: {e}"))?;
        let _ = stream.set_nodelay(true);
        let reader = stream.try_clone().map_err(|e| e.to_string())?;
        let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let mut stack = Stack { key_store, acl_mtu: 1021, ..Default::default() };
        stack.load_keys();
        let session = Arc::new(Self {
            st: Mutex::new(stack),
            cv: Condvar::new(),
            tx: Mutex::new(tx),
            stream,
            alive: AtomicBool::new(true),
            cmd_lock: Mutex::new(()),
            op_lock: Mutex::new(()),
        });

        // the writer never blocks the reader: QEMU writes to us while it
        // waits for its own reads
        std::thread::Builder::new()
            .name("emubt-tx".into())
            .spawn(move || {
                while let Ok(pkt) = rx.recv() {
                    if writer.write_all(&pkt).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let weak = Arc::downgrade(&session);
        std::thread::Builder::new()
            .name("emubt-rx".into())
            .spawn(move || Self::reader(weak, reader))
            .map_err(|e| e.to_string())?;

        session.init()?;
        Ok(session)
    }

    fn reader(weak: std::sync::Weak<Self>, mut stream: TcpStream) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let Some(s) = weak.upgrade() else { return };
            buf.extend_from_slice(&chunk[..n]);
            let mut st = s.st.lock().unwrap();
            while let Some(pkt) = take_h4(&mut buf) {
                match pkt {
                    Ok(H4Packet::Event { code, params }) => st.on_event(code, &params),
                    Ok(H4Packet::Acl { handle_flags, data }) => st.on_acl(handle_flags, &data),
                    Err(kind) => log::warn!(target: "emubt", "skipped H4 type 0x{kind:02x}"),
                }
            }
            let out = std::mem::take(&mut st.out);
            let deliveries = std::mem::take(&mut st.deliveries);
            let found = std::mem::take(&mut st.found);
            let lost = std::mem::replace(&mut st.link_lost, false);
            let data_cb = st.data_cb.clone();
            let on_found = st.on_found.clone();
            drop(st);
            s.flush(out);
            s.cv.notify_all();
            if let Some(cb) = data_cb.as_ref() {
                for d in deliveries {
                    cb(Ok(d));
                }
                if lost {
                    cb(Err("emulator SPP channel closed".into()));
                }
            }
            if let Some(cb) = on_found {
                for (addr, name) in found {
                    cb(addr, name);
                }
            }
        }
        if let Some(s) = weak.upgrade() {
            s.alive.store(false, Ordering::SeqCst);
            let mut st = s.st.lock().unwrap();
            st.note("emulator bridge closed".into());
            let open = st.link.as_ref().is_some_and(|l| l.rfcomm.open);
            st.link = None;
            let cb = st.data_cb.clone();
            drop(st);
            s.cv.notify_all();
            if let (true, Some(cb)) = (open, cb) {
                cb(Err("emulator bridge closed".into()));
            }
        }
    }

    fn flush(&self, out: Vec<Vec<u8>>) {
        let tx = self.tx.lock().unwrap();
        for pkt in out {
            let _ = tx.send(pkt);
        }
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn close(&self) {
        self.alive.store(false, Ordering::SeqCst);
        let _ = self.stream.shutdown(Shutdown::Both);
        self.cv.notify_all();
    }

    /// Run `f` under the state lock, then send what it queued.
    fn with<T>(&self, f: impl FnOnce(&mut Stack) -> T) -> T {
        let mut st = self.st.lock().unwrap();
        let r = f(&mut st);
        let out = std::mem::take(&mut st.out);
        drop(st);
        self.flush(out);
        r
    }

    /// Wait until `f` yields a value.
    fn wait<T>(
        &self,
        timeout: Duration,
        what: &str,
        mut f: impl FnMut(&mut Stack) -> Option<T>,
    ) -> Result<T, String> {
        let end = Instant::now() + timeout;
        let mut st: MutexGuard<'_, Stack> = self.st.lock().unwrap();
        loop {
            if let Some(v) = f(&mut st) {
                return Ok(v);
            }
            if !self.alive() {
                return Err("emulator bridge closed".into());
            }
            let now = Instant::now();
            if now >= end {
                return Err(format!("timed out waiting for {what}"));
            }
            st = self.cv.wait_timeout(st, end - now).unwrap().0;
        }
    }

    /// An HCI command; returns its return parameters (Status first), or
    /// the status of a Command Status.
    fn command(&self, opcode: u16, params: &[u8]) -> Result<Vec<u8>, String> {
        let _guard = self.cmd_lock.lock().unwrap();
        self.with(|st| {
            st.cmd_done = None;
            st.command(opcode, params);
        });
        self.wait(Duration::from_secs(5), &format!("HCI command 0x{opcode:04x}"), |st| {
            match st.cmd_done.take() {
                Some((op, rp)) if op == opcode => Some(rp),
                other => {
                    st.cmd_done = other;
                    None
                }
            }
        })
    }

    fn command_ok(&self, opcode: u16, params: &[u8]) -> Result<Vec<u8>, String> {
        let rp = self.command(opcode, params)?;
        match rp.first() {
            Some(0) => Ok(rp),
            Some(s) => Err(format!("HCI command 0x{opcode:04x} failed: status 0x{s:02x}")),
            None => Err(format!("HCI command 0x{opcode:04x}: empty reply")),
        }
    }

    fn init(&self) -> Result<(), String> {
        self.command_ok(0x0C03, &[])?; // Reset
        let rp = self.command_ok(0x1009, &[])?; // Read BD_ADDR
        let rb = self.command_ok(0x1005, &[])?; // Read Buffer Size
        self.command_ok(0x0C01, &[0xFF, 0xFF, 0xFB, 0xFF, 0x07, 0xF8, 0xBF, 0x3D])?;
        self.command_ok(0x0C56, &[0x01])?; // Simple Pairing Mode
        self.command_ok(0x0C7A, &[0x01])?; // Secure Connections Host Support
        self.command_ok(0x0C45, &[0x02])?; // Inquiry Mode: extended
        let mut name = LOCAL_NAME.as_bytes().to_vec();
        name.resize(248, 0);
        self.command_ok(0x0C13, &name)?;
        self.command_ok(0x0C24, &LOCAL_COD)?;
        self.with(|st| {
            if rp.len() >= 7 {
                st.local.copy_from_slice(&rp[1..7]);
            }
            if rb.len() >= 3 {
                st.acl_mtu = le16(&rb, 1) as usize;
            }
            let local = format_bdaddr(&st.local);
            st.note(format!("bridge up: local {local}, ACL {} bytes", st.acl_mtu));
        });
        Ok(())
    }

    pub fn set_data_callback(&self, cb: Option<DataCallback>) {
        self.st.lock().unwrap().data_cb = cb;
    }

    pub fn set_connected_callback(&self, cb: Option<ConnectedCallback>) {
        self.st.lock().unwrap().connected_cb = cb;
    }

    pub fn knows(&self, addr: &[u8; 6]) -> bool {
        self.st.lock().unwrap().known.iter().any(|(a, _)| a == addr)
    }

    pub fn log_lines(&self, since: usize) -> (usize, Vec<String>) {
        let st = self.st.lock().unwrap();
        let skip = since.saturating_sub(st.log_dropped);
        (st.log_dropped + st.log.len(), st.log.iter().skip(skip).cloned().collect())
    }

    pub fn status(&self) -> SessionStatus {
        let st = self.st.lock().unwrap();
        let link = st.link.as_ref();
        SessionStatus {
            local_addr: format_bdaddr(&st.local),
            devices: st
                .known
                .iter()
                .map(|(a, n)| FoundDevice { addr: format_bdaddr(a), name: n.clone() })
                .collect(),
            link: link.filter(|l| l.up()).map(|l| format_bdaddr(&l.addr)),
            encrypted: link.is_some_and(|l| l.encrypt.is_some_and(|(s, m)| s == 0 && m != 0)),
            rfcomm_channel: link.filter(|l| l.rfcomm.open).map(|l| l.rfcomm.dlci >> 1),
            spp_open: link.is_some_and(|l| l.rfcomm.open),
            tx_bytes: st.tx_bytes,
            rx_bytes: st.rx_bytes,
        }
    }

    /// General inquiry; `on_found` sees every response.
    pub fn inquiry(&self, length: u8, on_found: Option<FoundCallback>) -> Result<(), String> {
        self.with(|st| {
            st.on_found = on_found;
            st.inquiring = true;
        });
        let st = self.command(0x0401, &[0x33, 0x8B, 0x9E, length, 0x00])?;
        if st.first() != Some(&0) {
            return Err(format!("inquiry refused: {st:02x?}"));
        }
        Ok(())
    }

    pub fn stop_inquiry(&self) -> Vec<FoundDevice> {
        let inquiring = self.with(|st| {
            st.on_found = None;
            std::mem::replace(&mut st.inquiring, false)
        });
        if inquiring {
            let _ = self.command(0x0402, &[]);
        }
        self.status().devices
    }

    pub fn max_send_len(&self) -> Option<usize> {
        let st = self.st.lock().unwrap();
        st.link.as_ref().filter(|l| l.rfcomm.open).map(|l| l.rfcomm.n1)
    }

    /// Page the band, bond, encrypt and open its SPP channel.
    pub fn connect(&self, addr: [u8; 6], fallback: &[u8], unpair: bool) -> Result<(), String> {
        let _op = self.op_lock.lock().unwrap();
        if self.st.lock().unwrap().link.as_ref().is_some_and(|l| l.up()) {
            self.disconnect_link();
        }
        self.with(|st| {
            if unpair && st.keys.remove(&addr).is_some() {
                st.save_keys();
            }
            st.link = Some(Link::new(addr));
            st.note(format!("connecting to {}", format_bdaddr(&addr)));
        });
        let mut params = addr.to_vec();
        params.extend_from_slice(&[0x18, 0xCC, 0x01, 0x00, 0x00, 0x00, 0x01]);
        self.command_ok(0x0405, &params)?;
        let status = self.wait(Duration::from_secs(15), "the connection", |st| {
            st.link.as_ref()?.connected
        })?;
        if status != 0 {
            self.with(|st| st.link = None);
            return Err(format!("the band did not answer the page (status 0x{status:02x})"));
        }

        let result = self.secure_and_open(fallback);
        if result.is_err() {
            self.disconnect_link();
        } else {
            let cb = self.st.lock().unwrap().connected_cb.clone();
            if let Some(cb) = cb {
                cb();
            }
        }
        result
    }

    fn handle(&self) -> Result<u16, String> {
        let st = self.st.lock().unwrap();
        st.link.as_ref().filter(|l| l.up()).map(|l| l.handle).ok_or_else(|| "link lost".into())
    }

    fn secure_and_open(&self, fallback: &[u8]) -> Result<(), String> {
        // authenticate, re-pairing once if the band no longer has our key
        for attempt in 0..2 {
            let h = self.handle()?;
            self.with(|st| st.link.as_mut().map(|l| l.auth = None));
            self.command_ok(0x0411, &h.to_le_bytes())?;
            let status = self.wait(Duration::from_secs(60), "pairing", |st| {
                let l = st.link.as_ref()?;
                l.auth.or_else(|| l.disconnected.map(|_| 0xFF))
            })?;
            match status {
                0 => break,
                0x06 if attempt == 0 => {
                    self.with(|st| {
                        let addr = st.link.as_ref().map(|l| l.addr);
                        if let Some(a) = addr {
                            st.keys.remove(&a);
                            st.save_keys();
                        }
                    });
                }
                0x05 => {
                    return Err("pairing was declined on the band (confirm it on the band's screen)"
                        .into())
                }
                s => return Err(format!("authentication failed (status 0x{s:02x})")),
            }
        }
        let h = self.handle()?;
        self.command_ok(0x0413, &[h as u8, (h >> 8) as u8, 0x01])?;
        let (st, mode) = self.wait(Duration::from_secs(10), "encryption", |st| {
            st.link.as_ref()?.encrypt
        })?;
        if st != 0 || mode == 0 {
            return Err(format!("encryption failed (status 0x{st:02x})"));
        }

        let mut channels = self.sdp_spp_channels().unwrap_or_else(|err| {
            self.with(|st| st.note(format!("SDP: {err}; using fallback channels")));
            Vec::new()
        });
        for ch in fallback {
            if !channels.contains(ch) {
                channels.push(*ch);
            }
        }
        self.rfcomm_open_mux()?;
        for ch in channels {
            match self.rfcomm_open_dlc(ch) {
                Ok(()) => return Ok(()),
                Err(err) => self.with(|st| st.note(format!("RFCOMM channel {ch}: {err}"))),
            }
        }
        Err("no SPP channel accepted the connection".into())
    }

    fn l2cap_open(&self, psm: u16) -> Result<u16, String> {
        let local = self.with(|st| {
            let link = st.link.as_mut()?;
            let local = link.alloc_cid();
            link.chans.push(L2Chan {
                psm,
                local,
                remote: 0,
                initiator: true,
                state: ChanState::WaitConnRsp,
                remote_mtu: 672,
                conf_in: false,
                conf_out: false,
                refused: None,
            });
            let ident = link.ident();
            let mut d = psm.to_le_bytes().to_vec();
            d.extend_from_slice(&local.to_le_bytes());
            st.signal(SIG_CONN_REQ, ident, &d);
            Some(local)
        });
        let local = local.ok_or("link lost")?;
        let state = self.wait(Duration::from_secs(10), "the L2CAP channel", |st| {
            let link = st.link.as_mut()?;
            if !link.up() {
                return Some(Err("link lost".to_string()));
            }
            let chan = link.chan(local)?;
            match chan.state {
                ChanState::Open => Some(Ok(())),
                ChanState::Closed => Some(Err(format!(
                    "L2CAP channel for PSM 0x{psm:04x} refused (0x{:04x})",
                    chan.refused.unwrap_or(0)
                ))),
                _ => None,
            }
        })?;
        state.map(|_| local)
    }

    fn l2cap_close(&self, local: u16) {
        self.with(|st| {
            let Some(link) = st.link.as_mut() else { return };
            let Some(remote) = link.chan(local).map(|c| c.remote) else { return };
            let ident = link.ident();
            let mut d = remote.to_le_bytes().to_vec();
            d.extend_from_slice(&local.to_le_bytes());
            st.signal(SIG_DISC_REQ, ident, &d);
        });
    }

    fn sdp_spp_channels(&self) -> Result<Vec<u8>, String> {
        let cid = self.l2cap_open(PSM_SDP)?;
        // ServiceSearchAttributeRequest: (SerialPort) all attributes
        let mut req = sdp_seq(&[0x19, 0x11, 0x01]);
        req.extend_from_slice(&[0xFF, 0xFF]);
        req.extend_from_slice(&sdp_seq(&[0x0A, 0x00, 0x00, 0xFF, 0xFF]));
        self.with(|st| {
            let Some(link) = st.link.as_mut() else { return };
            link.sdp = SdpClient { cid, tid: 1, request: req.clone(), ..Default::default() };
            let remote = link.chan(cid).map(|c| c.remote).unwrap_or(0);
            let mut params = req.clone();
            params.push(0x00);
            st.send_sdp(remote, 0x06, 1, &params);
        });
        let lists = self.wait(Duration::from_secs(10), "the SDP response", |st| {
            st.link.as_mut()?.sdp.result.take()
        });
        self.l2cap_close(cid);
        let lists = lists??;
        let (el, _) = sdp_parse(&lists).ok_or("malformed SDP attribute lists")?;
        let channels = sdp_rfcomm_channels(&el);
        self.with(|st| st.note(format!("SDP: SPP on RFCOMM channel(s) {channels:?}")));
        Ok(channels)
    }

    fn rfcomm_open_mux(&self) -> Result<(), String> {
        let cid = self.l2cap_open(PSM_RFCOMM)?;
        let mtu = self.with(|st| {
            let link = st.link.as_mut()?;
            let remote_mtu = link.chan(cid)?.remote_mtu;
            link.rfcomm = Rfcomm { cid, ..Default::default() };
            st.rfcomm_send(0, true, RF_SABM | RF_PF, None, &[]);
            Some(remote_mtu.min(L2CAP_MTU) as usize)
        });
        let mtu = mtu.ok_or("link lost")?;
        let up = self.wait(Duration::from_secs(10), "the RFCOMM multiplexer", |st| {
            st.link.as_ref()?.rfcomm.mux_up
        })?;
        if !up {
            return Err("the band refused the RFCOMM multiplexer".into());
        }
        self.with(|st| {
            if let Some(link) = st.link.as_mut() {
                link.rfcomm.n1 = mtu - 6;
            }
        });
        Ok(())
    }

    fn rfcomm_open_dlc(&self, channel: u8) -> Result<(), String> {
        let dlci = channel << 1;
        self.with(|st| {
            let Some(link) = st.link.as_mut() else { return };
            let n1 = link.rfcomm.n1 as u16;
            link.rfcomm.dlci = dlci;
            link.rfcomm.pn = None;
            link.rfcomm.ua = None;
            link.rfcomm.closed = false;
            let mut pn = vec![dlci, 0xF0, 0x07, 0x00];
            pn.extend_from_slice(&n1.to_le_bytes());
            pn.extend_from_slice(&[0x00, RFCOMM_CREDITS]);
            st.rfcomm_send(0, true, RF_UIH, None, &rfcomm_mux(MUX_PN, true, &pn));
        });
        let (n1, credits, cfc) = self.wait(Duration::from_secs(10), "RFCOMM parameters", |st| {
            st.link.as_ref()?.rfcomm.pn
        })?;
        self.with(|st| {
            let Some(link) = st.link.as_mut() else { return };
            let rf = &mut link.rfcomm;
            rf.n1 = n1.min(rf.n1).max(1);
            rf.cfc = cfc;
            rf.tx_credits = if cfc { credits as u32 } else { u32::MAX };
            rf.rx_given = RFCOMM_CREDITS as u32;
            st.rfcomm_send(dlci, true, RF_SABM | RF_PF, None, &[]);
        });
        let ua = self.wait(Duration::from_secs(10), "the RFCOMM channel", |st| {
            st.link.as_ref()?.rfcomm.ua
        })?;
        if !ua {
            return Err("refused".into());
        }
        self.with(|st| {
            let msc = [(dlci << 2) | 0x03, 0x8D];
            st.rfcomm_send(0, true, RF_UIH, None, &rfcomm_mux(MUX_MSC, true, &msc));
            if let Some(link) = st.link.as_mut() {
                link.rfcomm.open = true;
                link.rfcomm.closed = false;
            }
            st.note(format!("SPP open on RFCOMM channel {channel} (frame {n1})"));
        });
        Ok(())
    }

    /// Data to the band's SPP channel.
    pub fn send(&self, data: &[u8]) -> Result<(), String> {
        let n1 = self.max_send_len().ok_or("not connected")?;
        for chunk in data.chunks(n1.max(1)) {
            self.wait(Duration::from_secs(10), "RFCOMM credits", |st| {
                let rf = &mut st.link.as_mut()?.rfcomm;
                if !rf.open {
                    return Some(Err("SPP channel closed".to_string()));
                }
                if rf.tx_credits == 0 {
                    return None;
                }
                if rf.cfc {
                    rf.tx_credits -= 1;
                }
                Some(Ok(()))
            })??;
            self.with(|st| {
                let dlci = st.link.as_ref().map(|l| l.rfcomm.dlci).unwrap_or(0);
                st.tx_bytes += chunk.len() as u64;
                st.rfcomm_send(dlci, true, RF_UIH, None, chunk);
            });
        }
        Ok(())
    }

    fn disconnect_link(&self) {
        let handle = {
            let st = self.st.lock().unwrap();
            st.link.as_ref().filter(|l| l.up()).map(|l| l.handle)
        };
        if let Some(h) = handle {
            let _ = self.command(0x0406, &[h as u8, (h >> 8) as u8, 0x13]);
            let _ = self.wait(Duration::from_secs(3), "the disconnection", |st| {
                st.link.as_ref().map_or(Some(()), |l| l.disconnected.map(|_| ()))
            });
        }
        self.with(|st| st.link = None);
    }

    pub fn disconnect(&self) {
        let _op = self.op_lock.lock().unwrap();
        self.disconnect_link();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}
