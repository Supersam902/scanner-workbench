//! J2534 04.04 CAN, K-line and J1850 backend. Uses the Windows ABI (32-bit ULONG).
//! Vendor drivers must match the process architecture. No OEM DLLs are bundled.

use crate::adapter::{DiagnosticAdapter, DiagnosticResponse, DiagnosticTarget};
use crate::elm327::{ConnectionInfo, Elm327Error};
use libloading::Library;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const J1850_VPW: u32 = 1;
const J1850_PWM: u32 = 2;
const CAN: u32 = 5;
const ISO15765: u32 = 6;
const SET_CONFIG: u32 = 0x02;
const NODE_ADDRESS: u32 = 0x04;
const ADD_TO_FUNCT_MSG_LOOKUP_TABLE: u32 = 0x0c;
const CAN_29BIT_ID: u32 = 0x100;
const TX_LOOPBACK: u32 = 0x01;
const RX_START: u32 = 0x02;
const RX_BREAK: u32 = 0x04;
const TX_DONE: u32 = 0x08;
const ISO_TP_PADDING_ERROR: u32 = 0x10;
const MAX_EXTENDED_ID: u32 = 0x1fffffff;
const FRAME_PAD: u32 = 0x40;
const ERR_INVALID_CHANNEL_ID: u32 = 2;
const ERR_DEVICE_NOT_CONNECTED: u32 = 8;
const ERR_TIMEOUT: u32 = 9;
const ERR_BUFFER_EMPTY: u32 = 0x10;
const ERR_INVALID_DEVICE_ID: u32 = 0x1a;
const EXTENDED_RESPONSE_BASE: u32 = 0x18DAF100;
const EXTENDED_RESPONSE_MASK: u32 = 0xFFFFFF00;
const MAX_OBD_ECUS: usize = 8;
const TESTER_ADDRESS: u8 = 0xf1;
const J1850_RESPONSE_ADDRESS: u8 = 0x6b;
pub(crate) const KLINE_SESSION_IDLE_MS: u32 = 5000;
// SAE J1699/4 §5.4 recommends this gap before a 5-baud init after a fast pulse.
const KLINE_FAST_WAKE_IDLE_MS: u32 = 2600;

pub(crate) fn is_pid00_reply(reply: &DiagnosticResponse) -> bool {
    reply.transport_error.is_none()
        && reply.payload.starts_with(&[0x41, 0])
        && reply.payload.len() >= 6
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum CanProtocol {
    #[default]
    Can11Bit500K,
    Can29Bit500K,
    Can11Bit250K,
    Can29Bit250K,
    Iso9141,
    KwpFast,
    KwpSlow,
    J1850Vpw,
    J1850Pwm,
}

impl CanProtocol {
    pub const ALL: [Self; 9] = [
        Self::Can11Bit500K,
        Self::Can29Bit500K,
        Self::Can11Bit250K,
        Self::Can29Bit250K,
        Self::Iso9141,
        Self::KwpFast,
        Self::KwpSlow,
        Self::J1850Vpw,
        Self::J1850Pwm,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Can11Bit500K => "CAN 11-bit / 500 kbit/s",
            Self::Can29Bit500K => "CAN 29-bit / 500 kbit/s",
            Self::Can11Bit250K => "CAN 11-bit / 250 kbit/s",
            Self::Can29Bit250K => "CAN 29-bit / 250 kbit/s",
            Self::Iso9141 => "ISO 9141-2 / 10.4 kbit/s / 5-baud init",
            Self::KwpFast => "ISO 14230 KWP2000 / fast init",
            Self::KwpSlow => "ISO 14230 KWP2000 / 5-baud init",
            Self::J1850Vpw => "SAE J1850 VPW / 10.4 kbit/s",
            Self::J1850Pwm => "SAE J1850 PWM / 41.6 kbit/s",
        }
    }
    pub(crate) fn short_label(self) -> &'static str {
        match self {
            Self::Iso9141 => "ISO 9141-2",
            Self::KwpFast => "KWP2000 fast init",
            Self::KwpSlow => "KWP2000 5-baud init",
            _ => self.label(),
        }
    }
    pub fn extended(self) -> bool {
        matches!(self, Self::Can29Bit500K | Self::Can29Bit250K)
    }
    pub fn is_can(self) -> bool {
        matches!(
            self,
            Self::Can11Bit500K | Self::Can29Bit500K | Self::Can11Bit250K | Self::Can29Bit250K
        )
    }
    pub(crate) fn is_kline(self) -> bool {
        self == Self::Iso9141 || self.is_kwp()
    }
    fn is_kwp(self) -> bool {
        matches!(self, Self::KwpFast | Self::KwpSlow)
    }
    fn is_j1850(self) -> bool {
        matches!(self, Self::J1850Vpw | Self::J1850Pwm)
    }
    fn protocol_id(self) -> u32 {
        match self {
            Self::J1850Vpw => J1850_VPW,
            Self::J1850Pwm => J1850_PWM,
            Self::Iso9141 => 3,
            Self::KwpFast | Self::KwpSlow => 4,
            Self::Can11Bit500K | Self::Can29Bit500K | Self::Can11Bit250K | Self::Can29Bit250K => {
                ISO15765
            }
        }
    }
    fn baud(self) -> u32 {
        match self {
            Self::Can11Bit500K | Self::Can29Bit500K => 500000,
            Self::Can11Bit250K | Self::Can29Bit250K => 250000,
            Self::Iso9141 | Self::KwpFast | Self::KwpSlow | Self::J1850Vpw => 10400,
            Self::J1850Pwm => 41600,
        }
    }
    fn flags(self) -> u32 {
        if self.extended() { CAN_29BIT_ID } else { 0 }
    }
    fn request_id(self) -> u32 {
        if self.extended() { 0x18DB33F1 } else { 0x7DF }
    }
    fn addresses(self, ecu: u32) -> (u32, u32) {
        if self.extended() {
            (EXTENDED_RESPONSE_BASE | ecu, 0x18DA00F1 | (ecu << 8))
        } else {
            (0x7E8 + ecu, 0x7E0 + ecu)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DriverInfo {
    pub name: String,
    pub path: PathBuf,
}

pub(crate) fn same_driver_path(a: &Path, b: &Path) -> bool {
    a == b
        || cfg!(windows)
            && a.to_str()
                .zip(b.to_str())
                .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// Enumerate the matching Windows registry view. Does not load driver code.
pub fn discover_drivers() -> Vec<DriverInfo> {
    #[cfg(windows)]
    {
        registry::drivers()
    }
    #[cfg(not(windows))]
    Vec::new()
}

#[cfg(windows)]
mod registry {
    use super::DriverInfo;
    use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf, ptr};
    use windows_sys::Win32::{Foundation::ERROR_SUCCESS, System::Registry::*};
    use windows_sys::w;

    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: Key owns a handle returned by RegOpenKeyExW.
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn string(key: &Key, subkey: &[u16], name: *const u16) -> Option<OsString> {
        // Allow the full Windows long-path length, including the terminator.
        let mut value = vec![0u16; 32_768];
        let mut bytes = (value.len() * size_of::<u16>()) as u32;
        // SAFETY: subkey and name are terminated; bytes is the buffer capacity.
        let status = unsafe {
            RegGetValueW(
                key.0,
                subkey.as_ptr(),
                name,
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
                ptr::null_mut(),
                value.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let end = value.iter().position(|&c| c == 0)?;
        (end > 0).then(|| OsString::from_wide(&value[..end]))
    }

    pub(super) fn drivers() -> Vec<DriverInfo> {
        let view = if cfg!(target_pointer_width = "64") {
            KEY_WOW64_64KEY
        } else {
            KEY_WOW64_32KEY
        };
        let mut raw = ptr::null_mut();
        // SAFETY: registry path is terminated and raw is a writable handle slot.
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                w!("SOFTWARE\\PassThruSupport.04.04"),
                0,
                KEY_READ | view,
                &mut raw,
            )
        };
        if status != ERROR_SUCCESS {
            return Vec::new();
        }
        let key = Key(raw);
        let mut drivers = Vec::new();
        for index in 0.. {
            let mut subkey = [0u16; 256]; // Maximum subkey name is 255 characters.
            let mut len = subkey.len() as u32;
            // SAFETY: key is live and subkey has len UTF-16 code units.
            let status = unsafe {
                RegEnumKeyExW(
                    key.0,
                    index,
                    subkey.as_mut_ptr(),
                    &mut len,
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if status != ERROR_SUCCESS {
                break;
            }
            let Some(path) = string(&key, &subkey, w!("FunctionLibrary")) else {
                continue;
            };
            let name = string(&key, &subkey, w!("Name"))
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| String::from_utf16_lossy(&subkey[..len as usize]));
            drivers.push(DriverInfo {
                name,
                path: PathBuf::from(path),
            });
        }
        drivers.sort_by(|a, b| a.name.cmp(&b.name));
        drivers
    }
}

#[repr(C)]
struct ByteArray {
    count: u32,
    bytes: *mut u8,
}

#[repr(C)]
struct Config {
    parameter: u32,
    value: u32,
}

#[repr(C)]
struct ConfigList {
    count: u32,
    entries: *mut Config,
}

#[repr(C)]
struct Message {
    protocol: u32,
    rx_status: u32,
    tx_flags: u32,
    timestamp: u32,
    data_size: u32,
    extra_data_index: u32,
    data: [u8; 4128],
}

impl Message {
    fn legacy(protocol: CanProtocol, payload: &[u8]) -> Result<Self, Elm327Error> {
        let limit = if protocol.is_j1850() { 7 } else { 63 };
        if !(1..=limit).contains(&payload.len()) {
            return Err(error(&format!(
                "{} request must contain 1..{limit} payload bytes",
                protocol.short_label()
            )));
        }
        // SAE J1979 §4.2.5 specifies the three-byte functional headers.
        let header = match protocol {
            CanProtocol::Iso9141 | CanProtocol::J1850Vpw => [0x68, 0x6a, TESTER_ADDRESS],
            CanProtocol::J1850Pwm => [0x61, 0x6a, TESTER_ADDRESS],
            CanProtocol::KwpFast | CanProtocol::KwpSlow => {
                [0xc0 | payload.len() as u8, 0x33, TESTER_ADDRESS]
            }
            _ => return Err(error("Legacy framing requires a non-CAN protocol")),
        };
        let mut message = Self::new(0, &[], 0)?;
        message.protocol = protocol.protocol_id();
        message.data_size = (3 + payload.len()) as u32;
        message.data[..3].copy_from_slice(&header);
        message.data[3..3 + payload.len()].copy_from_slice(payload);
        Ok(message)
    }
    fn new(id: u32, payload: &[u8], flags: u32) -> Result<Self, Elm327Error> {
        if payload.len() > 4095 {
            return Err(error("ISO-TP payload exceeds 4095 bytes"));
        }
        let mut message = Self {
            protocol: ISO15765,
            rx_status: 0,
            tx_flags: flags,
            timestamp: 0,
            data_size: (4 + payload.len()) as u32,
            extra_data_index: 0,
            data: [0; 4128],
        };
        message.data[..4].copy_from_slice(&id.to_be_bytes());
        message.data[4..4 + payload.len()].copy_from_slice(payload);
        Ok(message)
    }

    fn obd_discovery_source(&self) -> Option<u8> {
        // A Mode 01 PID 00 reply fits in one frame: PCI, 41 00, four bitmap bytes.
        if self.protocol != CAN
            || self.rx_status & (TX_LOOPBACK | RX_START | TX_DONE) != 0
            || !(11..=12).contains(&self.data_size)
            || (self.extra_data_index != 0
                && !(11..=self.data_size).contains(&self.extra_data_index))
            || self.data[4..7] != [6, 0x41, 0]
        {
            return None;
        }
        let id = u32::from_be_bytes(self.data[..4].try_into().unwrap());
        (id & EXTENDED_RESPONSE_MASK == EXTENDED_RESPONSE_BASE).then_some(id as u8)
    }

    fn reported_source(&self, protocol: CanProtocol) -> Option<u32> {
        if self.protocol != protocol.protocol_id() {
            return None;
        }
        if protocol.is_can() {
            (self.data_size >= 4).then(|| u32::from_be_bytes(self.data[..4].try_into().unwrap()))
        } else {
            (self.data_size >= 3 && self.data[2] != TESTER_ADDRESS)
                .then_some(u32::from(self.data[2]))
        }
    }

    fn response(&self, protocol: CanProtocol) -> Result<Option<DiagnosticResponse>, Elm327Error> {
        // Indications have no diagnostic payload.
        if self.rx_status & (TX_LOOPBACK | RX_START | RX_BREAK | TX_DONE) != 0 {
            return Ok(None);
        }
        if protocol.is_can() && self.rx_status & ISO_TP_PADDING_ERROR != 0 {
            return Err(error("Driver reported an ISO-TP padding error"));
        }
        if self.protocol != protocol.protocol_id() {
            return Err(error("Driver returned the wrong protocol"));
        }
        if !protocol.is_can() {
            let bus = if protocol.is_j1850() {
                "J1850"
            } else {
                "K-line"
            };
            if !(4..=4128).contains(&self.data_size) {
                return Err(error(&format!("Invalid {bus} response length")));
            }
            let end = if self.extra_data_index == 0 {
                self.data_size
            } else {
                self.extra_data_index
            } as usize;
            if end < 4 || end > self.data_size as usize {
                return Err(error(&format!("Invalid {bus} extra-data index")));
            }
            if self.data[2] == TESTER_ADDRESS {
                return Ok(None);
            }
            if protocol.is_j1850() {
                // SAE J1699/4 §5.2 documents ECUs using the request priority in replies.
                let priority_matches = if protocol == CanProtocol::J1850Pwm {
                    matches!(self.data[0], 0x41 | 0x61)
                } else {
                    matches!(self.data[0], 0x48 | 0x68)
                };
                if self.data[1] != J1850_RESPONSE_ADDRESS || !priority_matches {
                    return Ok(None);
                }
                // The J2534 driver puts any appended PWM IFR bytes beyond ExtraDataIndex.
                if end > 10 {
                    return Err(error("J1850 diagnostic payload exceeds seven bytes"));
                }
            }
            // KWP may use a separate payload-length byte when the low six
            // format bits are zero. Never decode that byte as a service.
            let start = if protocol.is_kwp() && self.data[0] & 0x3f == 0 {
                4
            } else {
                3
            };
            if start >= end {
                return Err(error("Empty K-line diagnostic payload"));
            }
            return Ok(Some(DiagnosticResponse::new(
                Some(u32::from(self.data[2])),
                self.data[start..end].to_vec(),
            )));
        }
        if !(5..=4128).contains(&self.data_size) {
            return Err(error("Driver returned an invalid message length"));
        }
        let id = u32::from_be_bytes(self.data[..4].try_into().unwrap());
        // Address matching happens in exchange: K-line ECUs can
        // legitimately reply outside the generic 7E8..7EF range.
        let valid = id
            <= if protocol.extended() {
                MAX_EXTENDED_ID
            } else {
                0x7ff
            };
        if !valid {
            return Ok(None);
        }
        let end = if self.extra_data_index == 0 {
            self.data_size
        } else {
            self.extra_data_index
        };
        if end < 5 || end > self.data_size {
            return Err(error("Driver returned invalid extra-data index"));
        }
        Ok(Some(DiagnosticResponse::new(
            Some(id),
            self.data[4..end as usize].to_vec(),
        )))
    }
}

type Open = unsafe extern "system" fn(*const c_void, *mut u32) -> u32;
type Close = unsafe extern "system" fn(u32) -> u32;
type Connect = unsafe extern "system" fn(u32, u32, u32, u32, *mut u32) -> u32;
type Messages = unsafe extern "system" fn(u32, *mut Message, *mut u32, u32) -> u32;
type Filter =
    unsafe extern "system" fn(u32, u32, *mut Message, *mut Message, *mut Message, *mut u32) -> u32;
type Ioctl = unsafe extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> u32;
type LastError = unsafe extern "system" fn(*mut c_char) -> u32;
type StopFilter = unsafe extern "system" fn(u32, u32) -> u32;
type StartPeriodic = unsafe extern "system" fn(u32, *mut Message, *mut u32, u32) -> u32;

struct Api {
    open: Open,
    close: Close,
    connect: Connect,
    disconnect: Close,
    read: Messages,
    write: Messages,
    filter: Filter,
    ioctl: Ioctl,
    last_error: LastError,
    stop_filter: Option<StopFilter>,
    start_periodic: Option<StartPeriodic>,
    stop_periodic: Option<StopFilter>,
    link_failure: Cell<Option<&'static str>>,
    // Kept alive until all device/channel cleanup has finished.
    _library: Library,
}

impl Api {
    fn load(path: &Path) -> Result<Self, Elm327Error> {
        if !path.is_absolute() {
            return Err(error(
                "Select an absolute path to the vendor driver library",
            ));
        }
        // SAFETY: the caller supplies an installed native J2534 04.04 library. All
        // function pointers use its documented Windows ABI and remain owned here.
        unsafe {
            let library = Library::new(path).map_err(|e| {
                error(&format!(
                    "Cannot load {}: {e}. Driver and app must both be {}-bit",
                    path.display(),
                    usize::BITS
                ))
            })?;
            macro_rules! symbol {
                ($name:literal) => {
                    *library
                        .get(concat!($name, "\0").as_bytes())
                        .map_err(|e| error(&format!("Missing J2534 04.04 export: {e}")))?
                };
            }
            Ok(Self {
                open: symbol!("PassThruOpen"),
                close: symbol!("PassThruClose"),
                connect: symbol!("PassThruConnect"),
                disconnect: symbol!("PassThruDisconnect"),
                read: symbol!("PassThruReadMsgs"),
                write: symbol!("PassThruWriteMsgs"),
                filter: symbol!("PassThruStartMsgFilter"),
                ioctl: symbol!("PassThruIoctl"),
                last_error: symbol!("PassThruGetLastError"),
                stop_filter: library
                    .get::<StopFilter>(b"PassThruStopMsgFilter\0")
                    .ok()
                    .map(|symbol| *symbol),
                start_periodic: library
                    .get::<StartPeriodic>(b"PassThruStartPeriodicMsg\0")
                    .ok()
                    .map(|symbol| *symbol),
                stop_periodic: library
                    .get::<StopFilter>(b"PassThruStopPeriodicMsg\0")
                    .ok()
                    .map(|symbol| *symbol),
                link_failure: Cell::new(None),
                _library: library,
            })
        }
    }

    fn check(&self, status: u32, operation: &str) -> Result<(), Elm327Error> {
        if status == 0 {
            return Ok(());
        }
        let link_failure = match status {
            ERR_INVALID_CHANNEL_ID => Some("J2534 diagnostic channel is no longer valid"),
            ERR_DEVICE_NOT_CONNECTED | ERR_INVALID_DEVICE_ID => {
                Some("J2534 adapter connection lost")
            }
            _ => None,
        };
        if link_failure.is_some() {
            self.link_failure.set(link_failure);
        }
        let mut text = [0u8; 256];
        // SAFETY: J2534 requires a 256-byte error buffer.
        unsafe {
            (self.last_error)(text.as_mut_ptr().cast());
        }
        let end = text.iter().position(|b| *b == 0).unwrap_or(text.len());
        let detail = format!(
            "{operation} failed (0x{status:02X}): {}",
            String::from_utf8_lossy(&text[..end])
        );
        Err(error(&match link_failure {
            Some(reason) => format!("{reason} ({detail})"),
            None => detail,
        }))
    }
}

fn error(message: &str) -> Elm327Error {
    Elm327Error::ProtocolError(format!("J2534: {message}"))
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[doc(hidden)]
pub enum ProbePhase {
    Channel,
    Setup,
    Initialization,
    Transmit,
    Reply,
    Keepalive,
    Cleanup,
}

#[derive(Debug)]
pub(crate) struct ProbeFailure {
    pub(crate) phase: ProbePhase,
    pub(crate) error: Elm327Error,
    pub(crate) cleanup: Option<Elm327Error>,
}

impl ProbeFailure {
    fn at(phase: ProbePhase) -> impl Fn(Elm327Error) -> Self {
        move |error| Self {
            phase,
            error,
            cleanup: None,
        }
    }
}

impl From<ProbeFailure> for Elm327Error {
    fn from(failure: ProbeFailure) -> Self {
        failure.cleanup.unwrap_or(failure.error)
    }
}

pub struct J2534 {
    api: Api,
    device: u32,
    channel: Option<u32>,
    periodic: Option<u32>,
    protocol: CanProtocol,
    kline_retry_ms: u32,
    extended_ecus: BTreeSet<u8>,
    info: ConnectionInfo,
}

impl J2534 {
    /// Open one explicitly selected OBD protocol. A successful 0100 response
    /// is required before the GUI reports a vehicle connection.
    #[allow(dead_code)] // Convenience wrapper for the common 11-bit OBD address.
    pub fn connect(path: &Path, protocol: CanProtocol) -> Result<Self, Elm327Error> {
        Self::connect_to(path, protocol, 0x10)
    }

    /// For 29-bit OBD, select the ECU source address explicitly. Unlike 11-bit
    /// OBD, responders are not confined to eight consecutive CAN identifiers.
    pub fn connect_to(
        path: &Path,
        protocol: CanProtocol,
        ecu_address: u8,
    ) -> Result<Self, Elm327Error> {
        let mut adapter = Self::open_device(path)?;
        adapter.probe_vehicle(protocol, Some(ecu_address))?;
        adapter.info.voltage = crate::elm327::block_on(adapter.voltage()).ok();
        if adapter.link_failure().is_some() {
            return Err(error(
                "Interface disconnected while checking vehicle voltage",
            ));
        }
        Ok(adapter)
    }

    pub(crate) fn open_device(path: &Path) -> Result<Self, Elm327Error> {
        let api = Api::load(path)?;
        let mut device = 0;
        // SAFETY: output pointers are valid and calls are serialised on the worker.
        api.check(
            unsafe { (api.open)(std::ptr::null(), &mut device) },
            "PassThruOpen",
        )?;
        Ok(Self {
            api,
            device,
            channel: None,
            periodic: None,
            protocol: CanProtocol::default(),
            kline_retry_ms: 0,
            extended_ecus: BTreeSet::new(),
            info: ConnectionInfo {
                port: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                baud: 0,
                protocol: String::new(),
                elm_version: "J2534 04.04".into(),
                voltage: None,
            },
        })
    }

    pub(crate) fn link_failure(&self) -> Option<&'static str> {
        self.api.link_failure.get()
    }

    pub(crate) fn kline_retry_ms(&self) -> u32 {
        self.kline_retry_ms
    }

    pub(crate) fn read_battery_millivolts(&self) -> Result<u32, Elm327Error> {
        let mut millivolts = 0u32;
        // READ_VBATT uses the device handle and does not require a channel.
        self.api.check(
            unsafe {
                (self.api.ioctl)(
                    self.device,
                    3,
                    std::ptr::null_mut(),
                    (&mut millivolts as *mut u32).cast(),
                )
            },
            "READ_VBATT",
        )?;
        Ok(millivolts)
    }

    /// Probe an emissions OBD channel on the open device.
    /// Release a failed channel before trying another protocol.
    pub(crate) fn probe_vehicle(
        &mut self,
        protocol: CanProtocol,
        ecu_override: Option<u8>,
    ) -> Result<Vec<DiagnosticResponse>, ProbeFailure> {
        self.kline_retry_ms = 0;
        if self.channel.is_some() {
            return Err(ProbeFailure::at(ProbePhase::Channel)(error(
                "Close the current diagnostic channel before trying a protocol",
            )));
        }
        self.try_protocol(protocol, ecu_override)
            .map_err(|mut failure| {
                if let Err(cleanup) = self.close_channel() {
                    failure.cleanup = Some(cleanup);
                }
                failure
            })
    }

    fn try_protocol(
        &mut self,
        protocol: CanProtocol,
        ecu_override: Option<u8>,
    ) -> Result<Vec<DiagnosticResponse>, ProbeFailure> {
        let setup = ProbeFailure::at(ProbePhase::Setup);
        let initialize = ProbeFailure::at(ProbePhase::Initialization);
        let keepalive_failure = ProbeFailure::at(ProbePhase::Keepalive);
        self.protocol = protocol;
        self.extended_ecus.clear();
        if protocol.extended() {
            self.extended_ecus = match ecu_override {
                Some(address) => BTreeSet::from([address]),
                None => self.discover_extended_ecus()?,
            };
        }
        let channel = self
            .open_channel(protocol.protocol_id())
            .map_err(ProbeFailure::at(ProbePhase::Channel))?;
        if protocol.is_can() {
            let ecu_addresses: Vec<u8> = if protocol.extended() {
                self.extended_ecus.iter().copied().collect()
            } else {
                (0..MAX_OBD_ECUS as u8).collect()
            };
            for ecu in ecu_addresses {
                let (response, request) = protocol.addresses(u32::from(ecu));
                self.flow_filter(channel, response, request)
                    .map_err(&setup)?;
            }
        } else {
            self.setup_legacy_channel(channel).map_err(setup)?;
        }
        if protocol.is_kline() {
            if protocol == CanProtocol::KwpFast {
                let mut input = Message::legacy(protocol, &[0x81]).map_err(&initialize)?;
                let mut output = Message::new(0, &[], 0).map_err(&initialize)?;
                self.kline_retry_ms = KLINE_FAST_WAKE_IDLE_MS;
                self.api
                    .check(
                        unsafe {
                            (self.api.ioctl)(
                                channel,
                                5,
                                (&mut input as *mut Message).cast(),
                                (&mut output as *mut Message).cast(),
                            )
                        },
                        "FAST_INIT",
                    )
                    .map_err(&initialize)?;
                let response = output
                    .response(protocol)
                    .and_then(|reply| reply.ok_or_else(|| error("No KWP initialization response")))
                    .map_err(&initialize)?;
                if response.payload.first() != Some(&0xc1) {
                    return Err(initialize(error("KWP StartCommunication rejected")));
                }
                self.kline_retry_ms = KLINE_SESSION_IDLE_MS;
            } else {
                let mut address = 0x33u8;
                let mut keywords = [0u8; 64];
                let mut input = ByteArray {
                    count: 1,
                    bytes: &mut address,
                };
                let mut output = ByteArray {
                    count: keywords.len() as u32,
                    bytes: keywords.as_mut_ptr(),
                };
                self.kline_retry_ms = KLINE_SESSION_IDLE_MS;
                self.api
                    .check(
                        unsafe {
                            (self.api.ioctl)(
                                channel,
                                4,
                                (&mut input as *mut ByteArray).cast(),
                                (&mut output as *mut ByteArray).cast(),
                            )
                        },
                        "FIVE_BAUD_INIT",
                    )
                    .map_err(&initialize)?;
                if !(2..=keywords.len() as u32).contains(&output.count) {
                    return Err(initialize(error("Invalid K-line initialization keywords")));
                }
                let [first, second] = [keywords[0], keywords[1]];
                let iso9141 = matches!([first, second], [0x08, 0x08] | [0x94, 0x94]);
                let kwp = matches!([first, second], [0x8f, 0xe9 | 0x6b | 0x6d | 0xef | 0xda]);
                if (protocol == CanProtocol::Iso9141 && kwp)
                    || (protocol == CanProtocol::KwpSlow && iso9141)
                {
                    return Err(initialize(error(&format!(
                        "5-baud keywords {first:02X} {second:02X} identify {}",
                        if iso9141 { "ISO 9141-2" } else { "KWP2000" }
                    ))));
                }
            }
        }
        let probe_timeout = if protocol.is_can() { 3000 } else { 2000 };
        let mut replies = self.exchange(&[1, 0], probe_timeout, None)?;
        let reply_detail = replies
            .iter()
            .find_map(|reply| reply.transport_error.as_deref());
        let reply_failure = ProbeFailure::at(ProbePhase::Reply);
        if let Some(reason) = self.link_failure() {
            let detail = reply_detail.map_or_else(
                || error(reason),
                |detail| error(&format!("{reason}. {detail}")),
            );
            return Err(reply_failure(detail));
        }
        if !replies.iter().any(is_pid00_reply) {
            let detail = reply_detail
                .unwrap_or("Vehicle did not answer the supported-PID request with a valid bitmap");
            return Err(reply_failure(error(detail)));
        }
        if protocol.is_kline() {
            let start = self
                .api
                .start_periodic
                .ok_or_else(|| keepalive_failure(error("Driver lacks K-line keepalive support")))?;
            if self.api.stop_periodic.is_none() {
                return Err(keepalive_failure(error(
                    "Driver lacks K-line keepalive cleanup",
                )));
            }
            let mut keepalive = Message::legacy(
                protocol,
                if protocol == CanProtocol::Iso9141 {
                    &[1, 0]
                } else {
                    &[0x3e]
                },
            )
            .map_err(&keepalive_failure)?;
            let mut id = 0;
            self.api
                .check(
                    unsafe { start(channel, &mut keepalive, &mut id, 2000) },
                    "K-line keepalive",
                )
                .map_err(keepalive_failure)?;
            self.periodic = Some(id);
        }
        for &address in &self.extended_ecus {
            let source = Some(protocol.addresses(u32::from(address)).0);
            if !replies.iter().any(|reply| {
                reply.source == source
                    && (reply.payload.starts_with(&[0x41, 0])
                        || reply.payload == [0x7f, 1, 0x78]
                        || reply.transport_error.is_some())
            }) {
                replies.push(DiagnosticResponse::receive_failure(
                    source,
                    "ECU answered raw CAN discovery but did not repeat its support bitmap".into(),
                ));
            }
        }
        self.info.protocol = protocol.label().into();
        Ok(replies)
    }

    pub(crate) fn close_channel(&mut self) -> Result<(), Elm327Error> {
        let Some(channel) = self.channel.take() else {
            return Ok(());
        };
        // SAFETY: the channel belongs to this device and calls are serialized.
        unsafe {
            if let (Some(id), Some(stop)) = (self.periodic.take(), self.api.stop_periodic) {
                stop(channel, id);
            }
            let closed = self
                .api
                .check((self.api.disconnect)(channel), "PassThruDisconnect");
            if closed.is_err() && self.api.link_failure.get().is_none() {
                self.api
                    .link_failure
                    .set(Some("Could not release the diagnostic channel"));
            }
            closed
        }
    }

    fn open_channel(&mut self, protocol_id: u32) -> Result<u32, Elm327Error> {
        let mut channel = 0;
        // SAFETY: the device is open and the channel output pointer is writable.
        self.api.check(
            unsafe {
                (self.api.connect)(
                    self.device,
                    protocol_id,
                    self.protocol.flags(),
                    self.protocol.baud(),
                    &mut channel,
                )
            },
            "PassThruConnect",
        )?;
        self.channel = Some(channel);
        Ok(channel)
    }

    fn setup_legacy_channel(&self, channel: u32) -> Result<(), Elm327Error> {
        if self.protocol == CanProtocol::J1850Pwm {
            // J2534-1 §7.3.2 and §7.3.12 configure the PWM physical address
            // and the functional response address used by SAE J1979.
            let mut node = Config {
                parameter: NODE_ADDRESS,
                value: u32::from(TESTER_ADDRESS),
            };
            let mut config = ConfigList {
                count: 1,
                entries: &mut node,
            };
            self.api.check(
                unsafe {
                    (self.api.ioctl)(
                        channel,
                        SET_CONFIG,
                        (&mut config as *mut ConfigList).cast(),
                        std::ptr::null_mut(),
                    )
                },
                "J1850 PWM SET_CONFIG NODE_ADDRESS",
            )?;
            let mut address = J1850_RESPONSE_ADDRESS;
            let mut addresses = ByteArray {
                count: 1,
                bytes: &mut address,
            };
            self.api.check(
                unsafe {
                    (self.api.ioctl)(
                        channel,
                        ADD_TO_FUNCT_MSG_LOOKUP_TABLE,
                        (&mut addresses as *mut ByteArray).cast(),
                        std::ptr::null_mut(),
                    )
                },
                "J1850 PWM ADD_TO_FUNCT_MSG_LOOKUP_TABLE",
            )?;
        }
        let mut mask = Message::new(0, &[], 0)?;
        let mut pattern = Message::new(0, &[], 0)?;
        mask.protocol = self.protocol.protocol_id();
        pattern.protocol = mask.protocol;
        if self.protocol.is_j1850() {
            mask.data_size = 2;
            mask.data[1] = 0xff;
            pattern.data[1] = J1850_RESPONSE_ADDRESS;
        } else {
            mask.data_size = 1;
        }
        pattern.data_size = mask.data_size;
        let mut filter = 0;
        self.api.check(
            unsafe {
                (self.api.filter)(
                    channel,
                    1,
                    &mut mask,
                    &mut pattern,
                    std::ptr::null_mut(),
                    &mut filter,
                )
            },
            "PassThruStartMsgFilter",
        )
    }

    fn flow_filter(&self, channel: u32, response: u32, request: u32) -> Result<u32, Elm327Error> {
        let flags = self.protocol.flags();
        let mask_id = if self.protocol.extended() {
            u32::MAX
        } else {
            0x7ff
        };
        let mut mask = Message::new(mask_id, &[], flags)?;
        let mut pattern = Message::new(response, &[], flags)?;
        let mut flow = Message::new(request, &[], flags | FRAME_PAD)?;
        let mut filter = 0;
        self.api.check(
            unsafe {
                (self.api.filter)(channel, 3, &mut mask, &mut pattern, &mut flow, &mut filter)
            },
            "PassThruStartMsgFilter",
        )?;
        Ok(filter)
    }

    fn discover_extended_ecus(&mut self) -> Result<BTreeSet<u8>, ProbeFailure> {
        // SAE J2534 flow filters are point-to-point, including for single frames.
        // Discover sources on raw CAN before giving each one its ISO-TP filter.
        let channel = self
            .open_channel(CAN)
            .map_err(ProbeFailure::at(ProbePhase::Channel))?;
        let discovered = self.read_extended_ecus(channel);
        if let Err(cleanup) = self.close_channel() {
            return Err(match discovered {
                Ok(_) => ProbeFailure::at(ProbePhase::Cleanup)(cleanup),
                Err(mut failure) => {
                    failure.cleanup = Some(cleanup);
                    failure
                }
            });
        }
        discovered
    }

    fn read_extended_ecus(&self, channel: u32) -> Result<BTreeSet<u8>, ProbeFailure> {
        let setup = ProbeFailure::at(ProbePhase::Setup);
        let reply = ProbeFailure::at(ProbePhase::Reply);
        let mut mask = Message::new(EXTENDED_RESPONSE_MASK & MAX_EXTENDED_ID, &[], CAN_29BIT_ID)
            .map_err(&setup)?;
        let mut pattern =
            Message::new(EXTENDED_RESPONSE_BASE, &[], CAN_29BIT_ID).map_err(&setup)?;
        mask.protocol = CAN;
        pattern.protocol = CAN;
        let mut filter = 0;
        self.api
            .check(
                unsafe {
                    (self.api.filter)(
                        channel,
                        1,
                        &mut mask,
                        &mut pattern,
                        std::ptr::null_mut(),
                        &mut filter,
                    )
                },
                "29-bit discovery PASS_FILTER",
            )
            .map_err(&setup)?;
        // ISO-TP Single Frame with a two-byte, read-only Mode 01 request.
        let mut message = Message::new(
            self.protocol.request_id(),
            &[2, 1, 0, 0, 0, 0, 0, 0],
            CAN_29BIT_ID,
        )
        .map_err(setup)?;
        message.protocol = CAN;
        self.transmit(channel, &mut message)?;
        let deadline = Instant::now() + Duration::from_millis(750);
        let mut sources = BTreeSet::new();
        while let Some(wait) = read_wait(deadline) {
            let Some(message) = self.read_message(channel, wait).map_err(&reply)? else {
                continue;
            };
            if let Some(source) = message.obd_discovery_source() {
                sources.insert(source);
                if sources.len() > MAX_OBD_ECUS {
                    return Err(reply(error(
                        "More than eight 29-bit emissions ECUs answered discovery",
                    )));
                }
            }
        }
        if sources.is_empty() {
            return Err(reply(Elm327Error::Timeout(
                "J2534: no 29-bit ECU answered the supported-PID broadcast".into(),
            )));
        }
        Ok(sources)
    }

    fn transmit(&self, channel: u32, message: &mut Message) -> Result<(), ProbeFailure> {
        // Clear stale replies before each request. All buffers are owned and sized to 04.04.
        self.api
            .check(
                unsafe { (self.api.ioctl)(channel, 8, std::ptr::null_mut(), std::ptr::null_mut()) },
                "CLEAR_RX_BUFFER",
            )
            .map_err(ProbeFailure::at(ProbePhase::Setup))?;
        let failure = ProbeFailure::at(ProbePhase::Transmit);
        let mut count = 1;
        self.api
            .check(
                unsafe { (self.api.write)(channel, message, &mut count, 1000) },
                "PassThruWriteMsgs",
            )
            .map_err(&failure)?;
        if count != 1 {
            return Err(failure(error("Driver did not transmit the request")));
        }
        Ok(())
    }

    fn read_message(&self, channel: u32, wait: u32) -> Result<Option<Message>, Elm327Error> {
        let mut message = Message::new(0, &[], 0)?;
        let mut count = 1;
        let status = unsafe { (self.api.read)(channel, &mut message, &mut count, wait) };
        if status != ERR_TIMEOUT && status != ERR_BUFFER_EMPTY {
            self.api.check(status, "PassThruReadMsgs")?;
        }
        if count > 1 {
            return Err(error("Driver returned an invalid message count"));
        }
        if count == 0 || status == ERR_BUFFER_EMPTY {
            std::thread::sleep(Duration::from_millis(1));
            Ok(None)
        } else {
            Ok(Some(message))
        }
    }

    fn expected_reply(&self, source: Option<u32>, target: Option<DiagnosticTarget>) -> bool {
        if let Some(target) = target {
            return source == Some(target.response);
        }
        if self.protocol.extended() {
            return source.is_some_and(|id| {
                id & EXTENDED_RESPONSE_MASK == EXTENDED_RESPONSE_BASE
                    && self.extended_ecus.contains(&(id as u8))
            });
        }
        !self.protocol.is_can() || source.is_some_and(|id| (0x7e8..=0x7ef).contains(&id))
    }
}

fn read_wait(deadline: Instant) -> Option<u32> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    (!remaining.is_zero()).then(|| remaining.as_millis().clamp(1, 50) as u32)
}

impl J2534 {
    fn exchange(
        &mut self,
        payload: &[u8],
        timeout_ms: u64,
        target: Option<DiagnosticTarget>,
    ) -> Result<Vec<DiagnosticResponse>, ProbeFailure> {
        let setup = ProbeFailure::at(ProbePhase::Setup);
        if payload.is_empty() || timeout_ms == 0 {
            return Err(setup(error("Request and timeout must be non-empty")));
        }
        let channel = self
            .channel
            .ok_or_else(|| ProbeFailure::at(ProbePhase::Channel)(error("No open channel")))?;
        let mut message = if self.protocol.is_can() {
            Message::new(
                target.map_or(self.protocol.request_id(), |target| target.request),
                payload,
                self.protocol.flags() | FRAME_PAD,
            )
            .map_err(&setup)?
        } else {
            let mut message = Message::legacy(self.protocol, payload).map_err(&setup)?;
            if let Some(target) = target {
                if target.request > 0xff || target.response > 0xff {
                    return Err(setup(error(
                        "K-line ECU addresses must be one byte; a CAN profile cannot be used on K-line",
                    )));
                }
                message.data[0] = if self.protocol == CanProtocol::Iso9141 {
                    0x68
                } else {
                    0x80 | payload.len() as u8
                };
                message.data[1] = target.request as u8;
            }
            message
        };
        self.transmit(channel, &mut message)?;
        let reply_failure = ProbeFailure::at(ProbePhase::Reply);
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(timeout_ms))
            .ok_or_else(|| reply_failure(error("Request timeout is too large")))?;
        let mut replies = Vec::new();
        let mut last_reply = None;
        let mut pending = BTreeMap::new();
        let mut started = BTreeSet::new();
        let mut faults = BTreeMap::new();
        let mut read_error = None;
        while let Some(wait) = read_wait(deadline) {
            match self.read_message(channel, wait) {
                Err(error) => {
                    read_error = Some(error);
                    break;
                }
                Ok(Some(message)) => {
                    let source = message.reported_source(self.protocol);
                    if source.is_none_or(|id| self.expected_reply(Some(id), target)) {
                        // J2534 04.04 gives the sender's CAN ID with an RxStart
                        // indication. Keep listening for that ISO-TP transfer.
                        if self.protocol.is_can()
                            && message.rx_status & (TX_LOOPBACK | RX_START | TX_DONE) == RX_START
                            && message.data_size == 4
                        {
                            if let Some(id) = source {
                                started.insert(id);
                            }
                        } else {
                            match message.response(self.protocol) {
                                Ok(Some(reply)) => {
                                    if let Some(id) = reply.source {
                                        started.remove(&id);
                                    }
                                    if reply.payload == [0x7F, payload[0], 0x78] {
                                        pending.insert(reply.source, reply);
                                    } else if matches_request(
                                        &reply.payload,
                                        payload,
                                        self.protocol.is_can(),
                                    ) {
                                        pending.remove(&reply.source);
                                        faults.remove(&reply.source);
                                        replies.push(reply);
                                        last_reply = Some(Instant::now());
                                    }
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    if let Some(id) = source {
                                        started.remove(&id);
                                    }
                                    faults.insert(source.or(target.map(|t| t.response)), error);
                                }
                            }
                        }
                    }
                }
                Ok(None) => {}
            }
            if pending.is_empty()
                && started.is_empty()
                && last_reply.is_some_and(|time| time.elapsed() >= Duration::from_millis(150))
            {
                break;
            }
        }
        if replies.is_empty() && pending.is_empty() && started.is_empty() {
            return Err(reply_failure(
                read_error
                    .or_else(|| faults.into_values().next())
                    .unwrap_or_else(|| Elm327Error::Timeout("J2534: no vehicle response".into())),
            ));
        }
        if let Some(error) = read_error {
            faults.entry(target.map(|t| t.response)).or_insert(error);
        }
        for source in started {
            faults.entry(Some(source)).or_insert_with(|| {
                Elm327Error::Timeout(
                    "J2534: ECU started a multi-frame reply that did not finish".into(),
                )
            });
        }
        // Preserve completed replies, even if another ECU's transfer fails.
        replies.extend(
            pending
                .into_iter()
                .filter_map(|(source, reply)| (!faults.contains_key(&source)).then_some(reply)),
        );
        replies.extend(
            faults.into_iter().map(|(source, error)| {
                DiagnosticResponse::receive_failure(source, error.to_string())
            }),
        );
        Ok(replies)
    }
}

fn matches_request(reply: &[u8], request: &[u8], can: bool) -> bool {
    reply.starts_with(&[0x7F, request[0]])
        || (reply.first() == Some(&request[0].wrapping_add(0x40))
            && match request[0] {
                1 if can && (3..=7).contains(&request.len()) => {
                    reply.get(1).is_some_and(|pid| request[1..].contains(pid))
                }
                1 | 6 | 8 | 9 | 0x19 | 0x21 => reply.get(1) == request.get(1),
                5 => {
                    reply.get(1) == request.get(1)
                        && reply
                            .get(2)
                            .is_none_or(|sensor| Some(sensor) == request.get(2))
                }
                2 | 0x22 => reply.get(1..3) == request.get(1..3),
                _ => true,
            })
}

impl DiagnosticAdapter for J2534 {
    async fn request(
        &mut self,
        payload: &[u8],
        timeout_ms: u64,
    ) -> Result<Vec<DiagnosticResponse>, Elm327Error> {
        self.exchange(payload, timeout_ms, None).map_err(Into::into)
    }

    async fn request_to(
        &mut self,
        target: DiagnosticTarget,
        payload: &[u8],
        timeout_ms: u64,
    ) -> Result<Vec<DiagnosticResponse>, Elm327Error> {
        if self.protocol.is_j1850() {
            return Err(error(
                "J1850 uses functional addressing for generic OBD-II requests",
            ));
        }
        let max_id = if self.protocol.extended() {
            MAX_EXTENDED_ID
        } else {
            0x7ff
        };
        if self.protocol.is_can() && (target.request > max_id || target.response > max_id) {
            return Err(error(
                "ECU identifier does not match the selected CAN format",
            ));
        }
        let standard_filter = if self.protocol.extended() {
            self.extended_ecus.iter().any(|&address| {
                self.protocol.addresses(u32::from(address)) == (target.response, target.request)
            })
        } else {
            (0x7e0..=0x7e7).contains(&target.request) && target.response == target.request + 8
        };
        if !self.protocol.is_can() || standard_filter {
            return self
                .exchange(payload, timeout_ms, Some(target))
                .map_err(Into::into);
        }
        let stop_filter = self.api.stop_filter.ok_or_else(|| {
            error(
                "Driver lacks PassThruStopMsgFilter; cannot safely install a temporary ECU filter",
            )
        })?;
        let channel = self.channel.ok_or_else(|| error("No open channel"))?;
        let filter = self.flow_filter(channel, target.response, target.request)?;
        let result = self.exchange(payload, timeout_ms, Some(target));
        let cleanup = self.api.check(
            unsafe { stop_filter(channel, filter) },
            "PassThruStopMsgFilter",
        );
        cleanup?;
        result.map_err(Into::into)
    }

    fn connection_info(&self) -> &ConnectionInfo {
        &self.info
    }
    async fn voltage(&mut self) -> Result<String, Elm327Error> {
        self.read_battery_millivolts().map(format_voltage)
    }
    async fn delay(&mut self, ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

pub(crate) fn format_voltage(millivolts: u32) -> String {
    format!("{:.1} V", f64::from(millivolts) / 1000.0)
}

impl Drop for J2534 {
    fn drop(&mut self) {
        // Disconnect releases filters too. Library remains loaded through cleanup.
        let _ = self.close_channel();
        // SAFETY: the device was successfully opened and the library remains loaded.
        unsafe { (self.api.close)(self.device) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_driver_message_lengths_and_indications() {
        assert_eq!(std::mem::size_of::<Message>(), 4152);
        let mut message = Message::new(0x7E8, &[0x41, 0x0c, 0x1a, 0xf8], 0).unwrap();
        assert_eq!(
            message
                .response(CanProtocol::default())
                .unwrap()
                .unwrap()
                .source,
            Some(0x7E8)
        );
        message.rx_status = 2;
        assert!(message.response(CanProtocol::default()).unwrap().is_none());
        message.rx_status = 0;
        message.data_size = 5000;
        assert!(message.response(CanProtocol::default()).is_err());
    }
}
