use crate::adapter::{DiagnosticAdapter, DiagnosticResponse, encode_hex};
use crate::diagnostics::{self, Query, Reading, SupportScan};
use std::collections::{BTreeMap, BTreeSet};

use super::model::unavailable;

const MAX_GROUP_PIDS: usize = 6;
const GROUP_TIMEOUT_MS: u64 = 1200;

type Sources = BTreeSet<Option<u32>>;
type Replies = BTreeMap<Option<u32>, Vec<Reading>>;

pub(super) fn reported_sources<'a>(query: &Query, scan: &'a SupportScan) -> Option<&'a Sources> {
    match (query.bytes.as_slice(), query.target) {
        ([1, pid], None) if *pid <= scan.support.checked_through => scan
            .pid_sources(*pid)
            .filter(|sources| !sources.contains(&None)),
        _ => None,
    }
}

// SAE J1979 Mode 01 records have no length field. Only group PIDs with
// known fixed widths. In particular, the support bitmaps are separate reads.
fn data_width(pid: u8) -> Option<usize> {
    match pid {
        0x01 => Some(4),
        0x04..=0x0b | 0x0d..=0x0f | 0x11 | 0x2f | 0x33 | 0x45 | 0x46 | 0x5c => Some(1),
        0x0c | 0x10 | 0x1f | 0x21 | 0x31 | 0x42 | 0x5e => Some(2),
        _ => None,
    }
}

pub(super) struct GroupPid<'a> {
    pub(super) query: &'a Query,
    width: usize,
    expected: Sources,
    replies: Replies,
}

impl GroupPid<'_> {
    pub(super) fn complete(&self) -> bool {
        self.expected.iter().all(|id| self.replies.contains_key(id))
    }

    pub(super) fn into_rows(self, single_rows: Option<Vec<Reading>>) -> Vec<Reading> {
        if let Some(single_rows) = single_rows {
            complete_replies(self.query, &self.expected, self.replies, single_rows)
        } else {
            self.replies.into_values().flatten().collect()
        }
    }
}

pub(super) struct Group<'a> {
    pub(super) bytes: Vec<u8>,
    pub(super) pids: Vec<GroupPid<'a>>,
}

impl<'a> Group<'a> {
    pub(super) fn new(queries: &'a [Query], scan: &'a SupportScan) -> Option<Self> {
        let mut bytes = vec![1];
        let mut pids = Vec::new();
        for query in queries.iter().take(MAX_GROUP_PIDS) {
            let Some(expected) = reported_sources(query, scan) else {
                break;
            };
            let pid = query.bytes[1];
            let Some(width) = data_width(pid) else { break };
            if bytes[1..].contains(&pid) {
                break;
            }
            bytes.push(pid);
            pids.push(GroupPid {
                query,
                width,
                expected: expected.clone(),
                replies: Replies::new(),
            });
        }
        (pids.len() >= 2).then_some(Self { bytes, pids })
    }

    pub(super) fn include_observed(&mut self, responders: &BTreeMap<[u8; 2], Sources>) {
        for pid in &mut self.pids {
            if let Some(observed) = responders.get(&[1, pid.query.bytes[1]]) {
                pid.expected.extend(observed);
            }
        }
    }

    pub(super) async fn read(&mut self, adapter: &mut impl DiagnosticAdapter) -> bool {
        let mut expected_sources = true;
        if let Ok(replies) = adapter.request(&self.bytes, GROUP_TIMEOUT_MS).await {
            for reply in replies {
                if let Some(records) = self.split_reply(&reply) {
                    for (index, rows) in records {
                        let pid = &mut self.pids[index];
                        expected_sources &= pid.expected.contains(&reply.source);
                        pid.replies.entry(reply.source).or_insert(rows);
                    }
                }
            }
        }
        adapter.delay(20).await;
        expected_sources && self.pids.iter().all(GroupPid::complete)
    }

    fn split_reply(&self, reply: &DiagnosticResponse) -> Option<Vec<(usize, Vec<Reading>)>> {
        if reply.transport_error.is_some() {
            return None;
        }
        let mut data = reply.payload.strip_prefix(&[0x41])?;
        let mut seen = BTreeSet::new();
        let mut records = Vec::new();
        while let Some((&pid, tail)) = data.split_first() {
            let index = self.bytes[1..].iter().position(|&id| id == pid)?;
            if !seen.insert(pid) {
                return None;
            }
            let record = &self.pids[index];
            let (value, rest) = tail.split_at_checked(record.width)?;
            let mut payload = vec![0x41, pid];
            payload.extend_from_slice(value);
            let single = DiagnosticResponse::new(reply.source, payload);
            records.push((
                index,
                diagnostics::decode(record.query, &single, true).ok()?,
            ));
            data = rest;
        }
        // A bad record invalidates this packet. Other ECUs' packets still count.
        Some(records)
    }
}

pub(super) fn individual_rows(
    query: &Query,
    expected: &Sources,
    rows: Vec<Reading>,
) -> Vec<Reading> {
    complete_replies(query, expected, Replies::new(), rows)
}

fn complete_replies(
    query: &Query,
    expected: &Sources,
    grouped: Replies,
    singles: Vec<Reading>,
) -> Vec<Reading> {
    let mut covered: Sources = grouped.keys().copied().collect();
    let mut rows = Vec::new();
    let mut request_error = None;
    for row in singles {
        if grouped.contains_key(&row.source) {
            continue;
        }
        if row.source.is_none() && unavailable(&row) && !row.is_signal_failure() {
            request_error = Some(row);
        } else {
            covered.insert(row.source);
            rows.push(row);
        }
    }
    rows.extend(grouped.into_values().flatten());
    let mut missing = expected.difference(&covered).peekable();
    if missing.peek().is_none() {
        rows.extend(request_error);
        return rows;
    }
    let status = request_error.map_or_else(
        || match query.bytes.as_slice() {
            [3 | 7 | 10] => "ECU did not return this code list".into(),
            [6, _] | [5, _, _] => "ECU did not return this monitor result".into(),
            [9, _] => "ECU did not return this information".into(),
            _ => "ECU did not return this PID".into(),
        },
        |row| row.status,
    );
    let request = encode_hex(&query.bytes);
    for &source in missing {
        rows.push(Reading {
            source,
            request: request.clone(),
            name: query.name.clone(),
            value: "Unavailable".into(),
            unit: String::new(),
            raw: String::new(),
            numeric: None,
            status: status.clone(),
            signal_failure: false,
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_uses_one_can_request_and_stops_at_unknown_widths() {
        let mut scan = SupportScan::new(1, 0);
        scan.accept(Ok(vec![DiagnosticResponse::new(
            Some(0x7e8),
            vec![0x41, 0, 0xff, 0xff, 0xff, 0xfe],
        )]));
        let mut queries: Vec<_> = (4..=10)
            .map(|pid| Query {
                bytes: vec![1, pid],
                target: None,
                name: format!("PID {pid:02X}"),
                signals: vec![],
                verified: false,
            })
            .collect();
        let group = Group::new(&queries, &scan).unwrap();
        assert_eq!(group.bytes, [1, 4, 5, 6, 7, 8, 9]);
        assert_eq!(group.pids.len(), 6);

        queries[1].bytes[1] = 0x12;
        assert!(scan.pid_sources(0x12).is_some());
        assert!(Group::new(&queries, &scan).is_none());
    }
}
