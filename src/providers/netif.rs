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

fn snapshot() -> Result<SystemCounters, String> {
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        let status = GetIfTable2(&mut table);
        if status != NO_ERROR || table.is_null() {
            return Err(format!("GetIfTable2 failed with status {}", status.0));
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
        Ok(out)
    }
}

impl NetProvider {
    pub fn new() -> Self {
        NetProvider::default()
    }

    /// Sample throughput. `Ok(None)` means a baseline was captured but no
    /// rate exists yet; a real idle delta is `Ok(Some(0, 0))`.
    pub fn update(&mut self) -> Result<Option<NetThroughput>, String> {
        self.update_from(snapshot(), std::time::Instant::now())
    }

    fn update_from(
        &mut self,
        current: Result<SystemCounters, String>,
        now: std::time::Instant,
    ) -> Result<Option<NetThroughput>, String> {
        let current = current?;
        let Some(prev) = self.prev.take() else {
            self.prev = Some(current);
            self.prev_at = Some(now);
            return Ok(None);
        };
        let elapsed = now
            .duration_since(self.prev_at.unwrap_or(now))
            .as_secs_f64();
        self.prev_at = Some(now);
        let mut result = NetThroughput::default();
        if elapsed <= 0.0 {
            self.prev = Some(current);
            return Ok(Some(result));
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
        Ok(Some(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_idle_and_failure_are_distinct() {
        let mut p = NetProvider::new();
        let now = std::time::Instant::now();
        let counters = || SystemCounters {
            entries: vec![(1, 100, 200)],
        };
        assert!(p.update_from(Ok(counters()), now).unwrap().is_none());
        let n = p
            .update_from(Ok(counters()), now + std::time::Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(n.in_bps, 0.0);
        assert_eq!(n.out_bps, 0.0);
        assert!(p
            .update_from(
                Err("failed".into()),
                now + std::time::Duration::from_secs(2)
            )
            .is_err());
    }
}
