//! Diagnostic operations independent of the computer-to-adapter connection.
//!
//! Payloads exclude CAN headers and ISO-TP framing. ELM command handling stays
//! in the ELM implementation; pass-through drivers never emulate AT commands.

use crate::elm327::{ConnectionInfo, Elm327Error, ElmAdapter};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiagnosticResponse {
    /// ECU address when the backend supplies one.
    pub source: Option<u32>,
    pub payload: Vec<u8>,
    /// Receive failures have no complete payload to decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_error: Option<String>,
}

impl DiagnosticResponse {
    pub fn new(source: Option<u32>, payload: Vec<u8>) -> Self {
        Self {
            source,
            payload,
            transport_error: None,
        }
    }

    pub(crate) fn receive_failure(source: Option<u32>, message: String) -> Self {
        Self {
            source,
            payload: Vec::new(),
            transport_error: Some(message),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiagnosticTarget {
    pub request: u32,
    pub response: u32,
}

#[allow(async_fn_in_trait)]
pub trait DiagnosticAdapter {
    async fn request(
        &mut self,
        payload: &[u8],
        timeout_ms: u64,
    ) -> Result<Vec<DiagnosticResponse>, Elm327Error>;
    async fn request_to(
        &mut self,
        _target: DiagnosticTarget,
        _payload: &[u8],
        _timeout_ms: u64,
    ) -> Result<Vec<DiagnosticResponse>, Elm327Error> {
        Err(Elm327Error::ProtocolError(
            "ECU-addressed requests require the native J2534 connection".into(),
        ))
    }
    fn connection_info(&self) -> &ConnectionInfo;
    async fn voltage(&mut self) -> Result<String, Elm327Error>;
    async fn delay(&mut self, ms: u64);
}

impl<T: ElmAdapter> DiagnosticAdapter for T {
    async fn request(
        &mut self,
        payload: &[u8],
        timeout_ms: u64,
    ) -> Result<Vec<DiagnosticResponse>, Elm327Error> {
        if payload.is_empty() {
            return Err(Elm327Error::ProtocolError(
                "Empty diagnostic request".into(),
            ));
        }
        let lines = self.send(&encode_hex(payload), timeout_ms).await?;
        decode_elm_payloads(&lines)
    }

    fn connection_info(&self) -> &ConnectionInfo {
        self.info()
    }

    async fn voltage(&mut self) -> Result<String, Elm327Error> {
        self.read_voltage().await
    }

    async fn delay(&mut self, ms: u64) {
        self.sleep_ms(ms).await;
    }
}

pub fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(text, "{byte:02X}").expect("writing to String");
    }
    text
}

pub fn decode_hex(text: &str) -> Result<Vec<u8>, Elm327Error> {
    let compact: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if compact.is_empty() || compact.len() % 2 != 0 || !compact.iter().all(u8::is_ascii_hexdigit) {
        return Err(Elm327Error::ProtocolError(format!(
            "Invalid hex payload: {text}"
        )));
    }
    Ok(compact
        .chunks_exact(2)
        .map(|pair| {
            let digit = |b: u8| {
                if b.is_ascii_digit() {
                    b - b'0'
                } else {
                    b.to_ascii_uppercase() - b'A' + 10
                }
            };
            digit(pair[0]) * 16 + digit(pair[1])
        })
        .collect())
}

/// Normalise ELM headers-off output, including numbered multi-line messages.
/// An incomplete message is an error, never a partial diagnostic result.
fn decode_elm_payloads(lines: &[String]) -> Result<Vec<DiagnosticResponse>, Elm327Error> {
    let mut responses = Vec::new();
    let mut expected_length = None;
    let mut assembled = Vec::new();
    let mut next_index = 0;
    for line in lines {
        let text = line.split_whitespace().collect::<String>().to_uppercase();
        if text.is_empty() || text == "SEARCHING..." || text == "BUSINIT:OK" {
            continue;
        }
        if text == "NODATA"
            || text == "STOPPED"
            || text == "?"
            || text.contains("ERROR")
            || text.contains("UNABLE")
            || text.contains("BUFFERFULL")
            || text.contains("BUSINIT")
        {
            return Err(Elm327Error::ProtocolError(line.clone()));
        }
        if text.len() == 3 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
            if expected_length.is_some() {
                return Err(Elm327Error::ProtocolError("Incomplete ELM message".into()));
            }
            expected_length = Some(usize::from_str_radix(&text, 16).unwrap());
            next_index = 0;
            continue;
        }
        if let Some((index, data)) = text.split_once(':') {
            let index = usize::from_str_radix(index, 16)
                .map_err(|_| Elm327Error::ProtocolError(line.clone()))?;
            if expected_length.is_none() || index != next_index {
                return Err(Elm327Error::ProtocolError(
                    "Invalid ELM message sequence".into(),
                ));
            }
            assembled.extend(decode_hex(data)?);
            next_index = (next_index + 1) % 16;
            let length = expected_length.unwrap();
            if assembled.len() >= length {
                assembled.truncate(length);
                responses.push(DiagnosticResponse::new(
                    None,
                    std::mem::take(&mut assembled),
                ));
                expected_length = None;
            }
        } else {
            if expected_length.is_some() {
                return Err(Elm327Error::ProtocolError("Incomplete ELM message".into()));
            }
            responses.push(DiagnosticResponse::new(None, decode_hex(&text)?));
        }
    }
    if expected_length.is_some() || responses.is_empty() {
        return Err(Elm327Error::ProtocolError(
            "No complete diagnostic response".into(),
        ));
    }
    Ok(responses)
}

/// Positive responses and any rejected responders from a functional request.
pub struct DiagnosticRead {
    pub lines: Vec<String>,
    pub warnings: Vec<String>,
}

pub async fn request_hex_report<A: DiagnosticAdapter>(
    adapter: &mut A,
    command: &str,
    timeout_ms: u64,
) -> Result<DiagnosticRead, Elm327Error> {
    let request = decode_hex(command)?;
    let replies = adapter.request(&request, timeout_ms).await?;
    let mut lines = Vec::new();
    let mut warnings = Vec::new();
    for reply in replies {
        if let Some(error) = &reply.transport_error {
            let source = reply
                .source
                .map_or_else(|| "unidentified source".into(), |id| format!("ECU {id:X}"));
            warnings.push(format!("Diagnostic request {command} to {source}: {error}"));
            continue;
        } else if reply.payload == [0x7F, request[0], 0x78] {
            warnings.push(format!(
                "Diagnostic request {command} remained pending at ECU {:?}",
                reply.source
            ));
            continue;
        } else if reply.payload.starts_with(&[0x7F, request[0]]) {
            warnings.push(format!(
                "Diagnostic request {command} rejected by {:?}: {}",
                reply.source,
                encode_hex(&reply.payload)
            ));
            continue;
        }
        if reply.payload.first() != Some(&request[0].wrapping_add(0x40)) {
            continue;
        }
        if request.len() > 1 && reply.payload.get(1) != request.get(1) {
            continue;
        }
        lines.push(encode_hex(&reply.payload));
    }
    // Only known read services may return partial results. A rejected clear
    // (or any other state-changing service) must still fail explicitly.
    let read_only = matches!(request[0], 0x01 | 0x02 | 0x03 | 0x07 | 0x09 | 0x0A);
    if !warnings.is_empty() && (!read_only || lines.is_empty()) {
        return Err(Elm327Error::ProtocolError(warnings.join("; ")));
    }
    if lines.is_empty() {
        return Err(Elm327Error::ProtocolError(format!(
            "No matching response to {command}"
        )));
    }
    Ok(DiagnosticRead { lines, warnings })
}

/// Compatibility boundary for callers that only need the positive payloads.
pub async fn request_hex<A: DiagnosticAdapter>(
    adapter: &mut A,
    command: &str,
    timeout_ms: u64,
) -> Result<Vec<String>, Elm327Error> {
    let report = request_hex_report(adapter, command, timeout_ms).await?;
    for warning in &report.warnings {
        tracing::warn!(%warning, "Partial diagnostic response");
    }
    Ok(report.lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconstructs_long_vin_and_preserves_separate_replies() {
        let lines = [
            "014",
            "0:490201574630",
            "1:4131323334353637",
            "2:3839303132333435",
            "410C1AF8",
        ];
        let replies = decode_elm_payloads(&lines.map(str::to_string)).unwrap();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].payload.len(), 20);
        assert_eq!(&replies[0].payload[..3], &[0x49, 2, 1]);
        assert_eq!(replies[1].payload, [0x41, 0x0c, 0x1a, 0xf8]);
    }

    #[test]
    fn rejects_truncated_reordered_and_error_responses() {
        for lines in [
            vec!["014", "0:490201574630"],
            vec!["009", "1:490201574630"],
            vec!["NO DATA"],
            vec!["410C1AF8", "BUFFER FULL"],
        ] {
            assert!(
                decode_elm_payloads(&lines.into_iter().map(str::to_string).collect::<Vec<_>>())
                    .is_err()
            );
        }
        assert!(decode_hex("é").is_err());
    }
}
