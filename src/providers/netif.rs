//! Network throughput from `GetIfTable2` interface byte counters.
//! Deltas between samples; software and loopback interfaces excluded.

#![cfg(windows)]

use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, MIB_IF_ROW2, MIB_IF_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::IF_OPER_STATUS;

const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const IF_OPER_STATUS_UP: i32 = 1;

#[derive(Clone, Copy, Debug, Default)]
pub struct NetThroughput {
    /// Receive bytes/second across all physical interfaces.
    pub in_bps: f64,
    /// Transmit bytes/second across all physical interfaces.
    pub out_bps: f64,
}

#[derive(Default)]
pub struct NetProvider {
    prev: Option<SystemCounters>,
    prev_at: Option<std::time::Instant>,
}

#[derive(Default)]
struct SystemCounters {
    entries: Vec<(usize, u64, u64)>, // (interface index, inOctets, outOctets)
}

fn snapshot() -> Option<SystemCounters> {
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table) != NO_ERROR || table.is_null() {
            return None;
        }
        let mut out = SystemCounters::default();
        let rows = std::ptr::addr_of!((*table).Table) as *const MIB_IF_ROW2;
        let slice = std::slice::from_raw_parts(rows, (*table).NumEntries as usize);
        for row in slice {
            // Skip loopback and non-operational interfaces.
            if row.Type == IF_TYPE_SOFTWARE_LOOPBACK {
                continue;
            }
            if row.OperStatus != IF_OPER_STATUS(IF_OPER_STATUS_UP) {
                continue;
            }
            out.entries
                .push((row.InterfaceIndex as usize, row.InOctets, row.OutOctets));
        }
        FreeMibTable(table.cast());
        Some(out)
    }
}

impl NetProvider {
    pub fn new() -> Self {
        NetProvider::default()
    }

    /// Sample throughput. The first call returns zeros (no delta yet).
    pub fn update(&mut self) -> NetThroughput {
        let Some(current) = snapshot() else {
            return NetThroughput::default();
        };
        let now = std::time::Instant::now();
        let Some(prev) = self.prev.take() else {
            self.prev = Some(current);
            self.prev_at = Some(now);
            return NetThroughput::default();
        };
        let elapsed = now
            .duration_since(self.prev_at.unwrap_or(now))
            .as_secs_f64();
        self.prev_at = Some(now);
        let mut result = NetThroughput::default();
        if elapsed <= 0.0 {
            self.prev = Some(current);
            return result;
        }
        for (luid, in_octets, out_octets) in &current.entries {
            if let Some((_, p_in, p_out)) =
                prev.entries.iter().find(|(p_luid, _, _)| p_luid == luid)
            {
                // Counter wrap: saturating delta.
                let d_in = in_octets.saturating_sub(*p_in);
                let d_out = out_octets.saturating_sub(*p_out);
                result.in_bps += d_in as f64 / elapsed;
                result.out_bps += d_out as f64 / elapsed;
            }
        }
        self.prev = Some(current);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_is_zero() {
        let mut p = NetProvider::new();
        let n = p.update();
        assert_eq!(n.in_bps, 0.0);
        assert_eq!(n.out_bps, 0.0);
    }
}
