use super::{
    DiagnosticResponse, MONITOR_INVALID_LIMITS, MONITOR_UNKNOWN_SCALING, Query, Reading,
    encode_hex, row,
};

pub(crate) const THRESHOLD: &str = "ECU parameter";
pub(crate) const COMPLETION_UNKNOWN: &str = "Monitor completion unverified";
pub(crate) const LIMITS_UNKNOWN: &str = "Limits unavailable";

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn request_parts(request: &str) -> Option<(u8, u8)> {
    if request.len() != 6 || !request.starts_with("05") {
        return None;
    }
    let test = u8::from_str_radix(request.get(2..4)?, 16).ok()?;
    let sensor = u8::from_str_radix(request.get(4..6)?, 16).ok()?;
    (test % 32 != 0 && sensor.is_power_of_two()).then_some((test, sensor))
}

pub(crate) fn test_name(id: u8) -> String {
    match id {
        0x01 => "Rich-to-lean threshold",
        0x02 => "Lean-to-rich threshold",
        0x03 => "Low voltage for switch timing",
        0x04 => "High voltage for switch timing",
        0x05 => "Rich-to-lean switch time",
        0x06 => "Lean-to-rich switch time",
        0x07 => "Minimum sensor voltage",
        0x08 => "Maximum sensor voltage",
        0x09 => "Time between transitions",
        0x0a => "Sensor period",
        _ => return format!("Test {id:02X}"),
    }
    .into()
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn sensor_label(mask: u8, sensors_per_bank: Option<u8>) -> String {
    if let Some(width) = sensors_per_bank.filter(|_| mask.is_power_of_two()) {
        let bit = mask.trailing_zeros() as u8;
        format!("Bank {} sensor {}", bit / width + 1, bit % width + 1)
    } else {
        format!("Sensor mask {mask:02X}")
    }
}

pub(super) fn decode(
    query: &Query,
    reply: &DiagnosticResponse,
    data: &[u8],
) -> Result<Vec<Reading>, String> {
    let [5, id, sensor] = query.bytes.as_slice() else {
        return Err("Mode 05 needs a test ID and sensor mask".into());
    };
    if !sensor.is_power_of_two() {
        return Err("Mode 05 requires one sensor per request".into());
    }
    if *id % 32 == 0 {
        if data.len() != 4 {
            return Err("Invalid oxygen-test support bitmap".into());
        }
        return Ok(vec![row(
            query,
            reply,
            query.name.clone(),
            encode_hex(data),
            "raw".into(),
            None,
            "Oxygen-test support bitmap",
        )]);
    }
    if data.is_empty() || data.len() > 3 {
        return Err("Invalid Mode 05 test record length".into());
    }
    // J1979 Appendix C gives these scales for the standardized test IDs.
    let (factor, unit, decimals) = match id {
        1..=4 | 7..=8 | 0x41..=0x4f => (5.0, "mV", 0),
        5..=6 | 0x21..=0x2f => (4.0, "ms", 0),
        9..=10 | 0x30..=0x3f => (40.0, "ms", 0),
        0x50..=0x5f => (50.0, "mV", 0),
        0x61..=0x6f => (0.1, "Hz", 1),
        0x70..=0x7f => (1.0, "count", 0),
        _ => {
            return Ok(vec![row(
                query,
                reply,
                test_name(*id),
                encode_hex(data),
                "raw".into(),
                None,
                MONITOR_UNKNOWN_SCALING,
            )]);
        }
    };
    let scale = |value| f64::from(value) * factor;
    let format_scaled = |value| format!("{:.decimals$}", scale(value));
    let measurement = scale(data[0]);
    let text = format_scaled(data[0]);
    let (value, status) = match (*id, data) {
        (1..=4, [_]) => (text, THRESHOLD),
        (1..=4, _) => return Err("Unexpected limits for a Mode 05 parameter".into()),
        (_, [_, min, max]) => (
            format!(
                "{text} (min {}, max {})",
                format_scaled(*min),
                format_scaled(*max)
            ),
            if min > max {
                MONITOR_INVALID_LIMITS
            } else {
                COMPLETION_UNKNOWN
            },
        ),
        (_, [_, bound]) => (
            format!("{text} (bound {})", format_scaled(*bound)),
            LIMITS_UNKNOWN,
        ),
        _ => (text, LIMITS_UNKNOWN),
    };
    Ok(vec![row(
        query,
        reply,
        test_name(*id),
        value,
        unit.into(),
        Some(measurement),
        status,
    )])
}
