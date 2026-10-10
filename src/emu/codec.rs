//! Wire formats of the emulator Bluetooth bridge: H4 framing, L2CAP
//! signaling, SDP data elements and RFCOMM (TS 07.10) frames.

/// H4 packet indicators (Bluetooth Core v5.3 Vol 4 Part A).
pub const H4_CMD: u8 = 0x01;
pub const H4_ACL: u8 = 0x02;
pub const H4_EVT: u8 = 0x04;

pub fn hci_command(opcode: u16, params: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(4 + params.len());
    pkt.push(H4_CMD);
    pkt.extend_from_slice(&opcode.to_le_bytes());
    pkt.push(params.len() as u8);
    pkt.extend_from_slice(params);
    pkt
}

/// One H4 ACL packet; `first` selects Packet_Boundary_Flag 0b10 (first
/// automatically flushable fragment) over 0b01 (continuing fragment).
pub fn hci_acl(handle: u16, first: bool, data: &[u8]) -> Vec<u8> {
    let hf = (handle & 0x0FFF) | if first { 0x2000 } else { 0x1000 };
    let mut pkt = Vec::with_capacity(5 + data.len());
    pkt.push(H4_ACL);
    pkt.extend_from_slice(&hf.to_le_bytes());
    pkt.extend_from_slice(&(data.len() as u16).to_le_bytes());
    pkt.extend_from_slice(data);
    pkt
}

/// A complete H4 packet split off the front of `buf`, if one is there.
pub enum H4Packet {
    Event { code: u8, params: Vec<u8> },
    Acl { handle_flags: u16, data: Vec<u8> },
}

pub fn take_h4(buf: &mut Vec<u8>) -> Option<Result<H4Packet, u8>> {
    let (len, pkt) = match *buf.first()? {
        H4_EVT => {
            if buf.len() < 3 || buf.len() < 3 + buf[2] as usize {
                return None;
            }
            let n = 3 + buf[2] as usize;
            (n, H4Packet::Event { code: buf[1], params: buf[3..n].to_vec() })
        }
        H4_ACL => {
            if buf.len() < 5 {
                return None;
            }
            let n = 5 + u16::from_le_bytes([buf[3], buf[4]]) as usize;
            if buf.len() < n {
                return None;
            }
            (
                n,
                H4Packet::Acl {
                    handle_flags: u16::from_le_bytes([buf[1], buf[2]]),
                    data: buf[5..n].to_vec(),
                },
            )
        }
        other => {
            buf.remove(0);
            return Some(Err(other));
        }
    };
    buf.drain(..len);
    Some(Ok(pkt))
}

/// "AA:BB:CC:DD:EE:FF" to HCI order (least significant octet first).
pub fn parse_bdaddr(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut addr = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        addr[5 - i] = u8::from_str_radix(p, 16).ok()?;
    }
    Some(addr)
}

pub fn format_bdaddr(addr: &[u8]) -> String {
    addr.iter()
        .rev()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Complete or shortened local name from an extended inquiry response.
pub fn eir_name(eir: &[u8]) -> Option<String> {
    let mut i = 0;
    while i < eir.len() {
        let len = eir[i] as usize;
        if len == 0 || i + 1 + len > eir.len() {
            break;
        }
        let kind = eir[i + 1];
        if kind == 0x08 || kind == 0x09 {
            let raw = &eir[i + 2..i + 1 + len];
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            return Some(String::from_utf8_lossy(&raw[..end]).into_owned());
        }
        i += 1 + len;
    }
    None
}

pub fn cstr(raw: &[u8]) -> String {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

/* ---- L2CAP (Vol 3 Part A) ---- */

pub const L2CAP_SIGNALING_CID: u16 = 0x0001;
pub const PSM_SDP: u16 = 0x0001;
pub const PSM_RFCOMM: u16 = 0x0003;

pub const SIG_COMMAND_REJECT: u8 = 0x01;
pub const SIG_CONN_REQ: u8 = 0x02;
pub const SIG_CONN_RSP: u8 = 0x03;
pub const SIG_CONF_REQ: u8 = 0x04;
pub const SIG_CONF_RSP: u8 = 0x05;
pub const SIG_DISC_REQ: u8 = 0x06;
pub const SIG_DISC_RSP: u8 = 0x07;
pub const SIG_ECHO_REQ: u8 = 0x08;
pub const SIG_ECHO_RSP: u8 = 0x09;
pub const SIG_INFO_REQ: u8 = 0x0A;
pub const SIG_INFO_RSP: u8 = 0x0B;

pub fn l2cap_frame(cid: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(4 + payload.len());
    f.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    f.extend_from_slice(&cid.to_le_bytes());
    f.extend_from_slice(payload);
    f
}

pub fn l2cap_signal(code: u8, ident: u8, data: &[u8]) -> Vec<u8> {
    let mut c = Vec::with_capacity(4 + data.len());
    c.push(code);
    c.push(ident);
    c.extend_from_slice(&(data.len() as u16).to_le_bytes());
    c.extend_from_slice(data);
    l2cap_frame(L2CAP_SIGNALING_CID, &c)
}

pub fn le16(v: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([v[at], v[at + 1]])
}

pub fn be16(v: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([v[at], v[at + 1]])
}

/// MTU option of an L2CAP configuration request, if present.
pub fn l2cap_conf_mtu(options: &[u8]) -> Option<u16> {
    let mut i = 0;
    while i + 2 <= options.len() {
        let kind = options[i] & 0x7F;
        let len = options[i + 1] as usize;
        if i + 2 + len > options.len() {
            break;
        }
        if kind == 0x01 && len == 2 {
            return Some(le16(options, i + 2));
        }
        i += 2 + len;
    }
    None
}

/* ---- SDP data elements (Vol 3 Part B 3) ---- */

/// The data elements this host interprets; the rest are `Other`.
#[derive(Debug, Clone)]
pub enum DataElement {
    Uint(u64),
    Uuid(Vec<u8>),
    Seq(Vec<DataElement>),
    Other,
}

impl DataElement {
    pub fn uuid16(&self) -> Option<u16> {
        match self {
            DataElement::Uuid(b) if b.len() == 2 => Some(u16::from_be_bytes([b[0], b[1]])),
            DataElement::Uuid(b) if b.len() == 4 && b[0] == 0 && b[1] == 0 => {
                Some(u16::from_be_bytes([b[2], b[3]]))
            }
            // Bluetooth base UUID 0000xxxx-0000-1000-8000-00805F9B34FB
            DataElement::Uuid(b)
                if b.len() == 16
                    && b[0] == 0
                    && b[1] == 0
                    && b[4..] == [0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0x80, 0x5F, 0x9B, 0x34, 0xFB] =>
            {
                Some(u16::from_be_bytes([b[2], b[3]]))
            }
            _ => None,
        }
    }

    pub fn seq(&self) -> Option<&[DataElement]> {
        match self {
            DataElement::Seq(v) => Some(v),
            _ => None,
        }
    }

    pub fn uint(&self) -> Option<u64> {
        match self {
            DataElement::Uint(v) => Some(*v),
            _ => None,
        }
    }
}

/// Parse one data element; returns it and the bytes it took.
pub fn sdp_parse(b: &[u8]) -> Option<(DataElement, usize)> {
    let d = *b.first()?;
    let kind = d >> 3;
    let size = d & 0x07;
    let (len, hdr) = match size {
        0 if kind == 0 => (0, 1),
        0 => (1, 1),
        1 => (2, 1),
        2 => (4, 1),
        3 => (8, 1),
        4 => (16, 1),
        5 => (*b.get(1)? as usize, 2),
        6 => (be16(b.get(..3)?, 1) as usize, 3),
        _ => (u32::from_be_bytes(b.get(1..5)?.try_into().ok()?) as usize, 5),
    };
    let body = b.get(hdr..hdr + len)?;
    let uint = |body: &[u8]| body.iter().fold(0u64, |acc, &x| (acc << 8) | x as u64);
    let el = match kind {
        1 if len <= 8 => DataElement::Uint(uint(body)),
        3 => DataElement::Uuid(body.to_vec()),
        6 | 7 => {
            let mut items = Vec::new();
            let mut at = 0;
            while at < body.len() {
                let (el, n) = sdp_parse(&body[at..])?;
                items.push(el);
                at += n;
            }
            DataElement::Seq(items)
        }
        0..=8 => DataElement::Other,
        _ => return None,
    };
    Some((el, hdr + len))
}

/// A data element sequence header for `len` bytes of contents.
pub fn sdp_seq(contents: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(3 + contents.len());
    if contents.len() < 256 {
        v.push(0x35);
        v.push(contents.len() as u8);
    } else {
        v.push(0x36);
        v.extend_from_slice(&(contents.len() as u16).to_be_bytes());
    }
    v.extend_from_slice(contents);
    v
}

/// RFCOMM server channels of the records in an attribute list sequence.
pub fn sdp_rfcomm_channels(lists: &DataElement) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(records) = lists.seq() else { return out };
    for record in records {
        let Some(attrs) = record.seq() else { continue };
        for pair in attrs.chunks(2) {
            if pair.len() != 2 || pair[0].uint() != Some(0x0004) {
                continue;
            }
            // ProtocolDescriptorList: ( (L2CAP ...), (RFCOMM, channel), ... )
            for proto in pair[1].seq().unwrap_or(&[]) {
                let Some(p) = proto.seq() else { continue };
                if p.first().and_then(DataElement::uuid16) == Some(0x0003) {
                    if let Some(ch) = p.get(1).and_then(DataElement::uint) {
                        out.push(ch as u8);
                    }
                }
            }
        }
    }
    out
}

/* ---- RFCOMM (TS 07.10 as profiled by the RFCOMM specification) ---- */

pub const RF_SABM: u8 = 0x2F;
pub const RF_UA: u8 = 0x63;
pub const RF_DM: u8 = 0x0F;
pub const RF_DISC: u8 = 0x43;
pub const RF_UIH: u8 = 0xEF;
pub const RF_PF: u8 = 0x10;

pub const MUX_PN: u8 = 0x20;
pub const MUX_TEST: u8 = 0x08;
pub const MUX_FCON: u8 = 0x28;
pub const MUX_FCOFF: u8 = 0x18;
pub const MUX_MSC: u8 = 0x38;
pub const MUX_NSC: u8 = 0x04;
pub const MUX_RPN: u8 = 0x24;
pub const MUX_RLS: u8 = 0x14;

fn crc8_table() -> [u8; 256] {
    let mut t = [0u8; 256];
    for (i, slot) in t.iter_mut().enumerate() {
        let mut c = i as u8;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xE0 } else { c >> 1 };
        }
        *slot = c;
    }
    t
}

pub fn rfcomm_fcs(data: &[u8]) -> u8 {
    let t = crc8_table();
    0xFF - data.iter().fold(0xFFu8, |fcs, &b| t[(fcs ^ b) as usize])
}

/// One RFCOMM frame. `cr` is the address C/R bit; `credits` is only
/// carried by UIH frames with the P/F bit set (credit based flow control).
pub fn rfcomm_frame(dlci: u8, cr: bool, control: u8, credits: Option<u8>, info: &[u8]) -> Vec<u8> {
    let addr = 0x01 | ((cr as u8) << 1) | (dlci << 2);
    let control = if credits.is_some() { control | RF_PF } else { control };
    let mut f = vec![addr, control];
    if info.len() < 128 {
        f.push(((info.len() as u8) << 1) | 1);
    } else {
        f.extend_from_slice(&((info.len() as u16) << 1).to_le_bytes());
    }
    let fcs_len = if control & !RF_PF == RF_UIH { 2 } else { f.len() };
    if let Some(c) = credits {
        f.push(c);
    }
    f.extend_from_slice(info);
    let fcs = rfcomm_fcs(&f[..fcs_len]);
    f.push(fcs);
    f
}

pub struct RfcommFrame {
    pub dlci: u8,
    pub control: u8,
    pub credits: Option<u8>,
    pub info: Vec<u8>,
}

pub fn rfcomm_parse(b: &[u8], cfc: bool) -> Option<RfcommFrame> {
    if b.len() < 4 {
        return None;
    }
    let dlci = b[0] >> 2;
    let pf = b[1] & RF_PF != 0;
    let control = b[1] & !RF_PF;
    let (len, mut at) = if b[2] & 1 != 0 {
        ((b[2] >> 1) as usize, 3)
    } else {
        ((le16(b, 2) >> 1) as usize, 4)
    };
    let credits = if control == RF_UIH && pf && cfc && dlci != 0 {
        at += 1;
        Some(*b.get(at - 1)?)
    } else {
        None
    };
    let info = b.get(at..at + len)?.to_vec();
    Some(RfcommFrame { dlci, control, credits, info })
}

/// A multiplexer control command or response on DLCI 0.
pub fn rfcomm_mux(kind: u8, command: bool, values: &[u8]) -> Vec<u8> {
    let mut v = vec![(kind << 2) | ((command as u8) << 1) | 1, ((values.len() as u8) << 1) | 1];
    v.extend_from_slice(values);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fcs_matches_ts0710_examples() {
        // SABM on DLCI 0 from the initiator: 03 3F 01 1C
        assert_eq!(rfcomm_frame(0, true, RF_SABM | RF_PF, None, &[]), vec![0x03, 0x3F, 0x01, 0x1C]);
        // UA on DLCI 0 from the responder: 03 73 01 D7
        assert_eq!(rfcomm_frame(0, true, RF_UA | RF_PF, None, &[]), vec![0x03, 0x73, 0x01, 0xD7]);
    }

    #[test]
    fn rfcomm_channel_from_record() {
        // ((0x0001 0x1101) (0x0004 ((0x0100) (0x0003 5))))
        let rec = [
            0x35, 0x19, 0x09, 0x00, 0x01, 0x35, 0x03, 0x19, 0x11, 0x01, 0x09, 0x00, 0x04, 0x35,
            0x0C, 0x35, 0x03, 0x19, 0x01, 0x00, 0x35, 0x05, 0x19, 0x00, 0x03, 0x08, 0x05,
        ];
        let lists = sdp_seq(&rec);
        let (el, n) = sdp_parse(&lists).unwrap();
        assert_eq!(n, lists.len());
        assert_eq!(sdp_rfcomm_channels(&el), vec![5]);
    }

    #[test]
    fn bdaddr_round_trip() {
        let a = parse_bdaddr("C0:FF:EE:15:03:01").unwrap();
        assert_eq!(a, [0x01, 0x03, 0x15, 0xEE, 0xFF, 0xC0]);
        assert_eq!(format_bdaddr(&a), "C0:FF:EE:15:03:01");
    }
}
