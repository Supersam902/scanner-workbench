//! ELM diagnostic sessions with ECU addresses, independent of the host transport.
use crate::adapter::{DiagnosticResponse, decode_hex, encode_hex};
use crate::elm_tcp::TcpElm;
use crate::elm327::{ConnectionInfo, Elm327, Elm327Error, ElmAdapter, ElmCanMode};
use crate::j2534::{CanProtocol, is_pid00_reply, matches_request};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum AdapterKind {
    #[default]
    J2534,
    Serial,
    Tcp,
}

impl AdapterKind {
    pub(super) const ALL: [Self; 3] = [Self::J2534, Self::Serial, Self::Tcp];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::J2534 => "J2534 pass-through",
            Self::Serial => "ELM-compatible serial",
            Self::Tcp => "ELM-compatible Wi-Fi / TCP",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElmConnection {
    Serial { port: String, baud: Option<u32> },
    Tcp(String),
}

impl ElmConnection {
    pub(super) fn kind(&self) -> AdapterKind {
        match self {
            Self::Serial { .. } => AdapterKind::Serial,
            Self::Tcp(_) => AdapterKind::Tcp,
        }
    }

    pub(super) fn label(&self) -> String {
        match self {
            Self::Serial { port, .. } => format!("Serial · {port}"),
            Self::Tcp(address) => format!("TCP · {address}"),
        }
    }

    pub(super) fn endpoints(&self, last_baud: Option<u32>) -> Vec<ElmEndpoint> {
        match self {
            Self::Serial { port, baud } => {
                let mut rates =
                    baud.map_or_else(|| crate::elm327::COMMON_BAUDS.to_vec(), |baud| vec![baud]);
                if let Some(index) =
                    last_baud.and_then(|last| rates.iter().position(|&baud| baud == last))
                {
                    rates[..=index].rotate_right(1);
                }
                rates
                    .into_iter()
                    .map(|baud| ElmEndpoint::Serial {
                        port: port.clone(),
                        baud,
                    })
                    .collect()
            }
            Self::Tcp(address) => vec![ElmEndpoint::Tcp(address.clone())],
        }
    }

    pub(super) fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Serial { port, baud } => {
                if port.is_empty() || port.len() > 512 || port.chars().any(char::is_control) {
                    return Err("Choose or enter a serial port.");
                }
                if baud.is_some_and(|rate| !(1..=2_000_000).contains(&rate)) {
                    return Err("Enter a baud rate between 1 and 2,000,000.");
                }
            }
            Self::Tcp(address) => {
                let (host, port) = address.rsplit_once(':').unwrap_or_default();
                if host.is_empty()
                    || address.len() > 259
                    || address.chars().any(char::is_whitespace)
                    || host.contains(['/', '@', '?', '#'])
                    || host.contains(':') && !(host.starts_with('[') && host.ends_with(']'))
                    || port.parse::<u16>().ok().is_none_or(|port| port == 0)
                {
                    return Err("Enter the adapter address and port, such as 192.168.0.10:35000.");
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ElmEndpoint {
    Serial { port: String, baud: u32 },
    Tcp(String),
}

impl ElmEndpoint {
    pub(super) fn baud(&self) -> Option<u32> {
        match self {
            Self::Serial { baud, .. } => Some(*baud),
            Self::Tcp(_) => None,
        }
    }
}

enum Transport {
    Serial(Elm327),
    Tcp(TcpElm),
}

impl ElmAdapter for Transport {
    async fn send(&mut self, command: &str, timeout: u64) -> Result<Vec<String>, Elm327Error> {
        match self {
            Self::Serial(adapter) => adapter.send(command, timeout).await,
            Self::Tcp(adapter) => adapter.send(command, timeout).await,
        }
    }

    fn info(&self) -> &ConnectionInfo {
        match self {
            Self::Serial(adapter) => adapter.info(),
            Self::Tcp(adapter) => adapter.info(),
        }
    }

    fn info_mut(&mut self) -> &mut ConnectionInfo {
        match self {
            Self::Serial(adapter) => adapter.info_mut(),
            Self::Tcp(adapter) => adapter.info_mut(),
        }
    }

    async fn sleep_ms(&mut self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

pub(super) struct Session {
    transport: Transport,
    protocol: Option<CanProtocol>,
    pub(super) failure: Option<String>,
}

impl Session {
    pub(super) fn open(endpoint: ElmEndpoint) -> Result<Self, Elm327Error> {
        let transport = match endpoint {
            ElmEndpoint::Serial { port, baud } => Transport::Serial(Elm327::open(&port, baud)?),
            ElmEndpoint::Tcp(address) => Transport::Tcp(TcpElm::open(&address)?),
        };
        Ok(Self {
            transport,
            protocol: None,
            failure: None,
        })
    }

    pub(super) async fn probe(
        &mut self,
    ) -> Result<(CanProtocol, String, Vec<DiagnosticResponse>), Elm327Error> {
        self.protocol = None;
        if let Some(reason) = &self.failure {
            return Err(Elm327Error::Serial(reason.clone()));
        }
        crate::obd_ops::configure_elm(&mut self.transport, ElmCanMode::Auto, true).await?;
        let lines = self.transport.send("0100", 8000).await?;
        let number = self
            .transport
            .send("ATDPN", 1000)
            .await?
            .into_iter()
            .next()
            .unwrap_or_default();
        let code = number.trim().trim_start_matches(['A', 'a']);
        let protocol = match code {
            "1" => CanProtocol::J1850Pwm,
            "2" => CanProtocol::J1850Vpw,
            "3" => CanProtocol::Iso9141,
            "4" => CanProtocol::KwpSlow,
            "5" => CanProtocol::KwpFast,
            "6" => CanProtocol::Can11Bit500K,
            "7" => CanProtocol::Can29Bit500K,
            "8" => CanProtocol::Can11Bit250K,
            "9" => CanProtocol::Can29Bit250K,
            _ => {
                return Err(Elm327Error::InitFailed(format!(
                    "No standard OBD-II protocol selected ({number}). Check ignition and adapter connection."
                )));
            }
        };
        let replies = decode_replies(&lines, protocol, &[1, 0])?;
        if !replies.iter().any(is_pid00_reply) {
            return Err(Elm327Error::InitFailed(
                "No complete supported-PID reply".into(),
            ));
        }
        self.protocol = Some(protocol);
        let identity = self.transport.info().elm_version.clone();
        Ok((protocol, identity, replies))
    }

    pub(super) async fn read(
        &mut self,
        payload: &[u8],
        timeout_ms: u64,
    ) -> Result<Vec<DiagnosticResponse>, Elm327Error> {
        let protocol = self
            .protocol
            .ok_or_else(|| Elm327Error::InitFailed("No verified diagnostic connection".into()))?;
        let lines = self.command(&encode_hex(payload), timeout_ms).await?;
        decode_replies(&lines, protocol, payload)
    }

    pub(super) async fn voltage(&mut self) -> Result<u32, Elm327Error> {
        let lines = self.command("ATRV", 1000).await?;
        lines
            .iter()
            .find_map(|line| millivolts(line))
            .ok_or_else(|| Elm327Error::ProtocolError("Adapter did not report a voltage".into()))
    }

    async fn command(&mut self, command: &str, timeout: u64) -> Result<Vec<String>, Elm327Error> {
        if let Some(reason) = &self.failure {
            return Err(Elm327Error::Serial(reason.clone()));
        }
        let reply = self.transport.send(command, timeout).await;
        if let Err(error) = &reply {
            // A failed host exchange can leave the previous response in flight.
            self.failure = Some(error.to_string());
        }
        reply
    }

    pub(super) async fn close(mut self) {
        if self.protocol.is_some() && self.failure.is_none() {
            let _ = self.transport.send("ATPC", 500).await;
        }
    }
}

fn millivolts(line: &str) -> Option<u32> {
    let volts: f64 = line.trim().strip_suffix(['V', 'v'])?.trim().parse().ok()?;
    // Allow pin 16 readings on both 12 V and 24 V vehicles.
    (0.0..=60.0)
        .contains(&volts)
        .then_some((volts * 1000.0).round() as u32)
}

#[derive(Default)]
struct CanTransfer {
    length: usize,
    next: u8,
    payload: Vec<u8>,
}

fn parse_can_frame(
    source: u32,
    frame: &[u8],
    transfers: &mut BTreeMap<u32, CanTransfer>,
) -> Result<Option<Vec<u8>>, &'static str> {
    let Some((&pci, data)) = frame.split_first() else {
        return Err("Empty CAN response");
    };
    if frame.len() > 8 {
        return Err("Invalid CAN frame length");
    }
    match pci >> 4 {
        0 => {
            let size = usize::from(pci & 0x0f);
            if size == 0 || transfers.contains_key(&source) {
                return Err("Invalid ISO-TP single frame");
            }
            data.get(..size)
                .map(|data| Some(data.to_vec()))
                .ok_or("Truncated ISO-TP single frame")
        }
        1 => {
            let Some((&low, payload)) = data.split_first() else {
                return Err("Missing ISO-TP payload length");
            };
            let length = (usize::from(pci & 0x0f) << 8) | usize::from(low);
            if length <= 7 || frame.len() != 8 || transfers.contains_key(&source) {
                return Err("Invalid ISO-TP first frame");
            }
            transfers.insert(
                source,
                CanTransfer {
                    length,
                    next: 1,
                    payload: payload.to_vec(),
                },
            );
            Ok(None)
        }
        2 => {
            let transfer = transfers
                .get_mut(&source)
                .ok_or("ISO-TP continuation without a first frame")?;
            let remaining = transfer.length - transfer.payload.len();
            if transfer.next != pci & 0x0f || data.len() < remaining.min(7) {
                return Err("Truncated or out-of-order ISO-TP continuation");
            }
            transfer.next = (transfer.next + 1) & 0x0f;
            transfer
                .payload
                .extend_from_slice(&data[..remaining.min(data.len())]);
            if transfer.payload.len() == transfer.length {
                Ok(transfers.remove(&source).map(|transfer| transfer.payload))
            } else {
                Ok(None)
            }
        }
        3 => Ok(None),
        _ => Err("Invalid ISO-TP frame type"),
    }
}

fn legacy_payload(protocol: CanProtocol, frame: &[u8]) -> Result<Vec<u8>, &'static str> {
    let data = frame
        .get(3..frame.len().saturating_sub(1))
        .ok_or("Truncated legacy reply")?;
    let data = if matches!(protocol, CanProtocol::KwpFast | CanProtocol::KwpSlow) {
        let declared = usize::from(frame[0] & 0x3f);
        let (size, payload) = if declared == 0 {
            let (&size, payload) = data.split_first().ok_or("Missing KWP length")?;
            (usize::from(size), payload)
        } else {
            (declared, data)
        };
        if size != payload.len() {
            return Err("KWP reply length mismatch");
        }
        payload
    } else {
        data
    };
    if data.is_empty() {
        return Err("Empty legacy reply");
    }
    Ok(data.to_vec())
}

fn decode_replies(
    lines: &[String],
    protocol: CanProtocol,
    request: &[u8],
) -> Result<Vec<DiagnosticResponse>, Elm327Error> {
    let can = protocol.is_can();
    let mut replies = Vec::new();
    let mut transfers = BTreeMap::new();
    let mut faults = BTreeMap::new();
    let mut adapter_error = None;
    for line in lines {
        let mut text = line
            .split_ascii_whitespace()
            .collect::<String>()
            .to_ascii_uppercase();
        for prefix in ["SEARCHING...", "BUSINIT:OK"] {
            if text.starts_with(prefix) {
                text.drain(..prefix.len());
            }
        }
        if text.is_empty() || text.starts_with("FC:") || text.ends_with("RTR") {
            continue;
        }
        if text == "NODATA" {
            continue;
        }
        let address_chars = if can {
            if protocol.extended() { 8 } else { 3 }
        } else {
            6
        };
        let address = text
            .get(..address_chars)
            .and_then(|head| u32::from_str_radix(head, 16).ok());
        let Some(address) = address else {
            adapter_error.get_or_insert(line);
            continue;
        };
        let source = if can { address } else { address & 0xff };
        if can
            && if protocol.extended() {
                source & 0xffff_ff00 != 0x18da_f100
            } else {
                !(0x7e8..=0x7ef).contains(&source)
            }
        {
            continue;
        }
        if faults.contains_key(&source) {
            continue;
        }
        let mut raw = if can { &text[address_chars..] } else { &text };
        // Some firmware shows the optional CAN DLC nibble before the PCI.
        if can && raw.len() % 2 == 1 {
            let declared = raw.as_bytes()[0];
            if !(b'1'..=b'8').contains(&declared)
                || usize::from(declared - b'0') * 2 != raw.len() - 1
            {
                faults.insert(source, "Invalid displayed CAN data length");
                transfers.remove(&source);
                continue;
            }
            raw = &raw[1..];
        }
        let payload = match decode_hex(raw) {
            Ok(frame) if can => parse_can_frame(source, &frame, &mut transfers),
            Ok(frame) => legacy_payload(protocol, &frame).map(Some),
            Err(_) => Err("Malformed OBD-II response bytes"),
        };
        match payload {
            Ok(Some(payload)) if matches_request(&payload, request, can) => {
                if !payload.starts_with(&[0x7f, request[0], 0x78]) {
                    replies.retain(|reply: &DiagnosticResponse| {
                        reply.source != Some(source)
                            || !reply.payload.starts_with(&[0x7f, request[0], 0x78])
                    });
                }
                replies.push(DiagnosticResponse::new(Some(source), payload));
            }
            Err(problem) => {
                faults.insert(source, problem);
                transfers.remove(&source);
            }
            _ => {}
        }
    }
    for source in transfers.into_keys() {
        faults.insert(source, "Incomplete ISO-TP reply");
    }
    replies.retain(|reply| {
        reply
            .source
            .is_none_or(|source| !faults.contains_key(&source))
    });
    replies.extend(faults.into_iter().map(|(source, problem)| {
        DiagnosticResponse::receive_failure(Some(source), problem.into())
    }));
    if let Some(error) = adapter_error {
        if replies.is_empty() {
            return Err(Elm327Error::ProtocolError(error.clone()));
        }
        replies.push(DiagnosticResponse::receive_failure(None, error.clone()));
    }
    if replies.is_empty() {
        Err(Elm327Error::Timeout("No matching OBD-II reply".into()))
    } else {
        Ok(replies)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_elm_can_replies_keep_complete_and_failed_ecus_separate() {
        let lines = [
            "7E8 10 14 49 02 01 4A 54 44",
            "7E9 10 14 49 02 01 4A 54 44",
            "7E8 21 42 52 33 32 45 37 32",
            "7E9 22 30 31 32 33 34 35 37",
            "7E8 22 30 31 32 33 34 35 36",
        ]
        .map(str::to_owned);
        let replies = decode_replies(&lines, CanProtocol::Can11Bit500K, &[9, 2]).unwrap();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].source, Some(0x7e8));
        assert_eq!(&replies[0].payload[3..], b"JTDBR32E720123456");
        assert_eq!(replies[1].source, Some(0x7e9));
        assert!(
            replies[1]
                .transport_error
                .as_deref()
                .unwrap()
                .contains("out-of-order")
        );
        assert!(replies[1].payload.is_empty());
    }

    #[test]
    fn elm_protocol_formats_preserve_addresses_and_only_application_data() {
        let cases = [
            (
                CanProtocol::Can29Bit500K,
                "SEARCHING...18 DA F1 27 8 06 41 00 80 00 00 00 00",
                0x18DAF127,
            ),
            (CanProtocol::Iso9141, "48 6B 10 41 00 80 00 00 00 84", 0x10),
            (CanProtocol::J1850Vpw, "48 6B 18 41 00 80 00 00 00 C0", 0x18),
            (CanProtocol::KwpFast, "86 F1 10 41 00 80 00 00 00 48", 0x10),
            (
                CanProtocol::KwpSlow,
                "80 F1 10 06 41 00 80 00 00 00 48",
                0x10,
            ),
        ];
        for (protocol, line, address) in cases {
            assert_eq!(
                decode_replies(&[line.into()], protocol, &[1, 0]).unwrap(),
                [DiagnosticResponse::new(
                    Some(address),
                    vec![0x41, 0, 0x80, 0, 0, 0]
                )]
            );
        }
        assert!(matches!(
            decode_replies(&["NO DATA".into()], CanProtocol::Can11Bit500K, &[1, 0]),
            Err(Elm327Error::Timeout(_))
        ));
        assert_eq!(millivolts("12.6 V"), Some(12600));
        assert_eq!(millivolts("0.0V"), Some(0));
        assert_eq!(millivolts("NaNV"), None);
    }
}
