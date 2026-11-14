//! The scanner UI never loads a vendor DLL. This private pipe protocol runs
//! J2534 and ELM I/O in a child of the same executable and bounds every call.
use super::elm::{ElmEndpoint, ProbeFailure as ElmProbeFailure, Session};
use crate::adapter::{DiagnosticAdapter, DiagnosticResponse, DiagnosticTarget};
use crate::elm327::{Elm327Error, block_on};
use crate::j2534::{CanProtocol, J2534, ProbePhase};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_REQUEST_MS: u64 = 10_000;
const CANCEL_GRACE: Duration = Duration::from_millis(250);
const CLOSE_TIMEOUT: Duration = Duration::from_millis(1200);

#[derive(Serialize, Deserialize)]
pub(super) enum Request {
    Open(PathBuf),
    OpenElm(ElmEndpoint),
    Probe(CanProtocol, Option<u8>),
    ProbeElm,
    Read {
        bytes: Vec<u8>,
        target: Option<DiagnosticTarget>,
        timeout_ms: u64,
    },
    Voltage,
    Close,
}

impl Request {
    fn limits(&self) -> (&'static str, Duration) {
        let (name, millis) = match self {
            Self::Open(_) | Self::OpenElm(_) => ("opening the adapter", 5_000),
            Self::Probe(protocol, _) => (
                "checking the vehicle protocol",
                if protocol.is_kline() { 12_000 } else { 8_000 },
            ),
            Self::ProbeElm => ("detecting the vehicle protocol with ELM", 30_000),
            Self::Read { timeout_ms, .. } => (
                "reading the ECU response",
                timeout_ms.min(&MAX_REQUEST_MS) + 2_000,
            ),
            Self::Voltage => ("reading DLC voltage", 2_000),
            Self::Close => return ("closing the cable", CLOSE_TIMEOUT),
        };
        (name, Duration::from_millis(millis))
    }
}

#[derive(Serialize, Deserialize)]
pub(super) enum Value {
    Done,
    Replies(Vec<DiagnosticResponse>),
    ProbeFailed {
        phase: ProbePhase,
        message: String,
        kline_retry_ms: u32,
    },
    ElmProbed {
        protocol: CanProtocol,
        identity: String,
        replies: Vec<DiagnosticResponse>,
    },
    ElmSetupFailed(String),
    ElmTransportFailed(String),
    ElmProtocolFailed(String),
    ElmReplyFailed {
        protocol: Option<CanProtocol>,
        message: String,
    },
    ElmUnverified {
        protocol: CanProtocol,
        replies: Vec<DiagnosticResponse>,
    },
    ReadTimeout(String),
    Voltage(u32),
}

#[derive(Serialize, Deserialize)]
struct Reply {
    value: Result<Value, String>,
    link_failure: Option<String>,
}

fn write_frame(writer: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    let data = serde_json::to_vec(value)?;
    if data.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Oversized driver message",
        ));
    }
    writer.write_all(&(data.len() as u32).to_le_bytes())?;
    writer.write_all(&data)?;
    writer.flush()
}

fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<Option<T>> {
    let mut size = [0; 4];
    if reader.read(&mut size[..1])? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut size[1..])?;
    let size = u32::from_le_bytes(size) as usize;
    if size == 0 || size > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid driver message size",
        ));
    }
    let mut data = vec![0; size];
    reader.read_exact(&mut data)?;
    Ok(Some(serde_json::from_slice(&data)?))
}

/// Child entry point. Its stdout is reserved for replies to the parent.
pub fn serve() -> io::Result<()> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SetErrorMode,
        };
        // A driver failure should close this child, not wait behind an OS dialog.
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
    }
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut adapter = None;
    while let Some(request) = read_frame::<Request>(&mut input)? {
        let closing = matches!(request, Request::Close);
        let value = handle_request(&mut adapter, request);
        let link_failure = adapter
            .as_ref()
            .and_then(Device::failure)
            .map(str::to_owned);
        write_frame(
            &mut output,
            &Reply {
                value,
                link_failure,
            },
        )?;
        if closing {
            break;
        }
    }
    Ok(())
}

enum Device {
    J2534(Box<J2534>),
    Elm(Session),
}

impl Device {
    fn failure(&self) -> Option<&str> {
        match self {
            Self::J2534(adapter) => adapter.link_failure(),
            Self::Elm(adapter) => adapter.failure.as_deref(),
        }
    }
}

fn handle_request(adapter: &mut Option<Device>, request: Request) -> Result<Value, String> {
    match request {
        Request::Open(path) => {
            if adapter.is_some() {
                return Err("A cable is already open".into());
            }
            *adapter = Some(Device::J2534(Box::new(
                J2534::open_device(&path).map_err(|e| e.to_string())?,
            )));
            Ok(Value::Done)
        }
        Request::OpenElm(endpoint) => {
            if adapter.is_some() {
                return Err("An adapter is already open".into());
            }
            *adapter = Some(Device::Elm(
                Session::open(endpoint).map_err(|e| e.to_string())?,
            ));
            Ok(Value::Done)
        }
        Request::Close => {
            if let Some(Device::Elm(elm)) = adapter.take() {
                block_on(elm.close());
            }
            Ok(Value::Done)
        }
        other => {
            let adapter = adapter.as_mut().ok_or("No open cable")?;
            match (adapter, other) {
                (Device::J2534(adapter), Request::Probe(protocol, ecu)) => {
                    match adapter.probe_vehicle(protocol, ecu) {
                        Ok(replies) => Ok(Value::Replies(replies)),
                        Err(failure) => {
                            let mut message = failure.error.to_string();
                            if let Some(cleanup) = failure.cleanup {
                                message.push_str(&format!(" Channel cleanup failed. {cleanup}"));
                            }
                            Ok(Value::ProbeFailed {
                                phase: failure.phase,
                                message,
                                kline_retry_ms: adapter.kline_retry_ms(),
                            })
                        }
                    }
                }
                (Device::Elm(adapter), Request::ProbeElm) => Ok(match block_on(adapter.probe()) {
                    Ok((protocol, identity, replies)) => Value::ElmProbed {
                        protocol,
                        identity,
                        replies,
                    },
                    Err(ElmProbeFailure::Setup(error)) => Value::ElmSetupFailed(error.to_string()),
                    Err(ElmProbeFailure::Transport(error)) => {
                        Value::ElmTransportFailed(error.to_string())
                    }
                    Err(ElmProbeFailure::Protocol(error)) => {
                        Value::ElmProtocolFailed(error.to_string())
                    }
                    Err(ElmProbeFailure::Reply(protocol, error)) => Value::ElmReplyFailed {
                        protocol,
                        message: error.to_string(),
                    },
                    Err(ElmProbeFailure::Unverified(protocol, replies)) => {
                        Value::ElmUnverified { protocol, replies }
                    }
                }),
                (Device::J2534(adapter), Request::Voltage) => {
                    adapter.read_battery_millivolts().map(Value::Voltage)
                }
                (Device::Elm(adapter), Request::Voltage) => {
                    block_on(adapter.voltage()).map(Value::Voltage)
                }
                (
                    adapter,
                    Request::Read {
                        bytes,
                        target,
                        timeout_ms,
                    },
                ) => {
                    if bytes.is_empty()
                        || bytes.len() > 16
                        || !(1..=MAX_REQUEST_MS).contains(&timeout_ms)
                    {
                        return Err("Invalid diagnostic request length or timeout".into());
                    }
                    let replies = match (adapter, target) {
                        (Device::J2534(adapter), Some(target)) => {
                            block_on(adapter.request_to(target, &bytes, timeout_ms))
                        }
                        (Device::J2534(adapter), None) => {
                            block_on(adapter.request(&bytes, timeout_ms))
                        }
                        (Device::Elm(adapter), None) => block_on(adapter.read(&bytes, timeout_ms)),
                        (Device::Elm(_), Some(_)) => {
                            return Err("ECU-targeted requests require J2534".into());
                        }
                    };
                    match replies {
                        Ok(replies) => Ok(Value::Replies(replies)),
                        Err(Elm327Error::Timeout(message)) => Ok(Value::ReadTimeout(message)),
                        Err(error) => Err(error),
                    }
                }
                _ => return Err("The adapter cannot run this command".into()),
            }
            .map_err(|e| e.to_string())
        }
    }
}

pub(super) struct DriverProcess {
    child: Option<Child>,
    #[cfg(windows)]
    job: Option<std::os::windows::io::OwnedHandle>,
    send: mpsc::SyncSender<Request>,
    receive: mpsc::Receiver<io::Result<Reply>>,
    shutdown: Arc<AtomicBool>,
    failure: Option<String>,
}

impl DriverProcess {
    pub(super) fn start(exe: &Path, shutdown: Arc<AtomicBool>) -> io::Result<Self> {
        let mut command = Command::new(exe);
        command
            .arg("--adapter-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn()?;
        #[cfg(windows)]
        let job = match isolate(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                reap(child);
                return Err(error);
            }
        };
        let mut input = child.stdin.take().expect("piped child input");
        let mut output = child.stdout.take().expect("piped child output");
        let (send, calls) = mpsc::sync_channel(1);
        let (answers, receive) = mpsc::sync_channel(1);
        // Pipe I/O itself may block if a driver crashes or never reads again.
        // The supervisor only waits on a bounded Rust channel.
        std::thread::spawn(move || {
            for request in calls {
                let reply = write_frame(&mut input, &request).and_then(|()| {
                    read_frame::<Reply>(&mut output)?.ok_or_else(|| {
                        io::Error::new(io::ErrorKind::UnexpectedEof, "Driver process exited")
                    })
                });
                let failed = reply.is_err();
                if answers.send(reply).is_err() || failed {
                    break;
                }
            }
        });
        Ok(Self {
            child: Some(child),
            #[cfg(windows)]
            job: Some(job),
            send,
            receive,
            shutdown,
            failure: None,
        })
    }

    pub(super) fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    pub(super) fn call(
        &mut self,
        request: Request,
        cancelled: impl Fn() -> bool,
    ) -> Result<Value, String> {
        if self.child.is_none() {
            return Err(self
                .failure
                .clone()
                .unwrap_or_else(|| "Driver process has exited".into()));
        }
        let closing = matches!(&request, Request::Close);
        if cancelled() || (!closing && self.shutdown.load(Ordering::Relaxed)) {
            return Err("Operation cancelled".into());
        }
        let (name, timeout) = request.limits();
        let valid_reply: fn(&Value) -> bool = match &request {
            Request::Read { .. } => {
                |value| matches!(value, Value::Replies(_) | Value::ReadTimeout(_))
            }
            Request::Probe(..) => {
                |value| matches!(value, Value::Replies(_) | Value::ProbeFailed { .. })
            }
            Request::ProbeElm => |value| {
                matches!(
                    value,
                    Value::ElmProbed { .. }
                        | Value::ElmSetupFailed(_)
                        | Value::ElmTransportFailed(_)
                        | Value::ElmProtocolFailed(_)
                        | Value::ElmReplyFailed { .. }
                        | Value::ElmUnverified { .. }
                )
            },
            Request::Voltage => |value| matches!(value, Value::Voltage(_)),
            _ => |value| matches!(value, Value::Done),
        };
        if self.send.try_send(request).is_err() {
            return Err(self.fail(format!("Driver process unavailable while {name}")));
        }
        let deadline = Instant::now() + timeout;
        let mut stop_by = None;
        loop {
            if !closing && self.shutdown.load(Ordering::Relaxed) {
                return Err(self.fail("Driver process stopped during shutdown".into()));
            }
            let now = Instant::now();
            if cancelled() {
                let grace = stop_by.get_or_insert(now + CANCEL_GRACE);
                if now >= *grace {
                    return Err(self.fail(format!("Stopped the unresponsive driver while {name}")));
                }
            }
            if now >= deadline {
                return Err(self.fail(format!(
                    "Driver did not respond while {name}. The driver process was stopped"
                )));
            }
            match self.receive.recv_timeout(Duration::from_millis(25)) {
                Ok(Ok(reply)) => {
                    if let Some(reason) = reply.link_failure {
                        self.failure = Some(reply.value.as_ref().err().cloned().unwrap_or(reason));
                    }
                    if reply.value.as_ref().is_ok_and(|value| !valid_reply(value)) {
                        return Err(self.fail(format!("Unexpected driver reply while {name}")));
                    }
                    return reply.value;
                }
                Ok(Err(e)) => {
                    return Err(self.fail(format!("Driver process exited while {name}: {e}")));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(self.fail(format!("Driver process exited while {name}")));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn fail(&mut self, message: String) -> String {
        self.failure = Some(message.clone());
        self.terminate();
        message
    }

    fn terminate(&mut self) {
        if let Some(mut child) = self.child.take() {
            #[cfg(windows)]
            drop(self.job.take()); // The job terminates the driver even if it spawned descendants.
            let _ = child.kill();
            reap(child);
        }
    }
}

impl Drop for DriverProcess {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.call(Request::Close, || false);
            self.terminate();
        }
    }
}

fn reap(mut child: Child) {
    if !matches!(child.try_wait(), Ok(Some(_))) {
        // Never wait for kernel/driver cleanup on the scanner supervisor.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[cfg(windows)]
fn isolate(child: &Child) -> io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    // SAFETY: the handles are owned, and the driver is only loaded after this succeeds.
    unsafe {
        let raw = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = OwnedHandle::from_raw_handle(raw);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
            || AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }
}
