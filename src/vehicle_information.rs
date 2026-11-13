use super::{Query, Reading, decode_vin, row};
use crate::adapter::{DiagnosticResponse, encode_hex};
use std::collections::BTreeMap;

pub(super) fn normalize_can(replies: Vec<DiagnosticResponse>) -> Vec<DiagnosticResponse> {
    use std::collections::btree_map::Entry;
    let mut positive = BTreeMap::new();
    let mut responses = Vec::new();
    for reply in replies {
        if reply.payload.first() != Some(&0x49) || reply.source.is_none() {
            responses.push(reply);
            continue;
        }
        match positive.entry(reply.source) {
            Entry::Vacant(entry) => {
                entry.insert(reply);
            }
            Entry::Occupied(mut entry) if entry.get().payload != reply.payload => {
                entry.insert(DiagnosticResponse::receive_failure(
                    reply.source,
                    "ECU sent conflicting vehicle-information replies".into(),
                ));
            }
            _ => {}
        }
    }
    responses.extend(positive.into_values());
    responses
}

// SAE J1979 §§6.9 and 7.9 and J1979-DA Appendix G.
pub(crate) fn information_name(pid: u8) -> Option<&'static str> {
    Some(match pid {
        0x02 => "VIN",
        0x04 => "Calibration ID",
        0x06 => "Calibration verification number",
        0x08 => "In-use performance (spark)",
        0x0a => "ECU name",
        0x0b => "In-use performance (compression)",
        0x0d => "Engine serial number",
        0x0f => "Emissions type approval",
        _ => return None,
    })
}

const SPARK_COUNTERS: [&str; 9] = [
    "Catalyst bank 1",
    "Catalyst bank 2",
    "Oxygen sensor bank 1",
    "Oxygen sensor bank 2",
    "EGR / VVT",
    "Secondary air",
    "EVAP",
    "Secondary oxygen sensor bank 1",
    "Secondary oxygen sensor bank 2",
];
const COMPRESSION_COUNTERS: [&str; 8] = [
    "NMHC catalyst",
    "NOx catalyst",
    "NOx adsorber",
    "Particulate filter",
    "Exhaust gas sensor",
    "EGR / VVT",
    "Boost pressure",
    "Fuel system",
];

pub(super) fn decode(
    query: &Query,
    reply: &DiagnosticResponse,
    data: &[u8],
    can: bool,
) -> Result<Vec<Reading>, String> {
    let &pid = query.bytes.get(1).ok_or("Missing vehicle-information ID")?;
    let name = information_name(pid).unwrap_or(&query.name);
    let reading = |name: &str, value: String, unit: &str, status: &str| {
        row(query, reply, name.into(), value, unit.into(), None, status)
    };
    if pid == 2 {
        let vin = decode_vin(data, can)?;
        return Ok(vec![reading(name, vin, "", "ECU responded")]);
    }
    if pid % 32 == 0 {
        let bitmap = if can {
            data
        } else {
            data.strip_prefix(&[1]).unwrap_or(&[])
        };
        if bitmap.len() != 4 {
            return Err("Invalid Mode 09 support bitmap".into());
        }
        return Ok(vec![reading(
            name,
            encode_hex(bitmap),
            "raw",
            "Vehicle information support bitmap",
        )]);
    }
    if !can && matches!(pid, 1 | 3 | 5 | 7 | 9 | 0x0c | 0x0e) {
        let [count @ 1..=255] = data else {
            return Err("Invalid Mode 09 message count".into());
        };
        return Ok(vec![reading(
            name,
            count.to_string(),
            "messages",
            "ECU responded",
        )]);
    }
    if information_name(pid).is_none() {
        if data.is_empty() {
            return Err("Vehicle-information reply contains no data".into());
        }
        return Ok(vec![reading(
            name,
            encode_hex(data),
            "raw",
            "Raw vehicle-information reply",
        )]);
    }
    let (&count, payload) = data
        .split_first()
        .filter(|(count, data)| **count > 0 && !data.is_empty())
        .ok_or("Missing vehicle-information item count or data")?;
    if !can && count != 1 {
        return Err("Incomplete legacy vehicle-information sequence".into());
    }
    if matches!(pid, 8 | 0x0b) {
        return performance_rows(pid, can.then_some(count), payload, reading);
    }

    let (width, multiple) = match pid {
        4 => (16, true),
        6 => (4, true),
        0x0a => (20, false),
        0x0d | 0x0f => (17, false),
        _ => unreachable!(),
    };
    // Legacy serial and approval numbers use the same three leading fill bytes as VIN.
    let payload = if !can && matches!(pid, 0x0d | 0x0f) && payload.len() == 20 {
        payload
            .strip_prefix(&[0, 0, 0])
            .ok_or("Invalid legacy identifier padding")?
    } else {
        payload
    };
    let items = payload.len() / width;
    if items == 0
        || payload.len() % width != 0
        || !multiple && items != 1
        || can && items != usize::from(count)
    {
        return Err(format!("Invalid {name} length or item count"));
    }
    let values = payload
        .chunks_exact(width)
        .map(|record| match pid {
            6 => Ok(encode_hex(record)),
            // The ECU acronym and name have separate padding within a 20-byte record.
            0x0a if record[4] == b'-' => {
                let acronym = printable(&record[..4])?;
                let full_name = printable(&record[5..])?;
                Ok(format!("{acronym} - {full_name}"))
            }
            0x0d | 0x0f => {
                let text = record.iter().position(|b| *b != 0).unwrap_or(record.len());
                printable(&record[text..]).map(str::to_owned)
            }
            _ => printable(record).map(str::to_owned),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(vec![reading(name, values.join(" | "), "", "ECU responded")])
}

fn printable(bytes: &[u8]) -> Result<&str, String> {
    let end = bytes.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
    let text = &bytes[..end];
    if text.iter().all(|b| (b' '..=b'~').contains(b)) {
        let text = std::str::from_utf8(text)
            .expect("printable ASCII")
            .trim_end();
        if !text.trim().is_empty() {
            return Ok(text);
        }
    }
    Err("Vehicle identifier is blank or contains invalid characters".into())
}

fn performance_rows(
    pid: u8,
    count: Option<u8>,
    data: &[u8],
    reading: impl Fn(&str, String, &str, &str) -> Reading,
) -> Result<Vec<Reading>, String> {
    let counters = data.len() / 2;
    let labels: &[&str] = match (pid, counters) {
        (8, 16 | 20) => &SPARK_COUNTERS,
        (0x0b, 16 | 18) => &COMPRESSION_COUNTERS,
        _ => return Err("Invalid in-use performance counter length".into()),
    };
    if data.len() % 2 != 0 || count.is_some_and(|n| usize::from(n) != counters) {
        return Err("In-use performance count does not match the data".into());
    }
    let mut rows = Vec::new();
    for (name, bytes) in ["OBD monitoring conditions", "Ignition cycles"]
        .into_iter()
        .zip(data[..4].chunks_exact(2))
    {
        let n = u16::from_be_bytes([bytes[0], bytes[1]]);
        let mut result = reading(name, n.to_string(), "count", "ECU responded");
        result.numeric = Some(f64::from(n));
        rows.push(result);
    }
    for (name, bytes) in labels.iter().zip(data[4..].chunks_exact(4)) {
        let complete = u16::from_be_bytes([bytes[0], bytes[1]]);
        let conditions = u16::from_be_bytes([bytes[2], bytes[3]]);
        rows.push(reading(
            name,
            format!("{complete} / {conditions}"),
            "",
            "Completions / conditions encountered",
        ));
    }
    Ok(rows)
}
