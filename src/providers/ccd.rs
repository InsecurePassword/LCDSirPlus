//! Native Cache/Frequency CCD topology detection — no Process Lasso.
//!
//! Strategy (locked in the Rust port plan):
//! 1. Group logical processors by L3 cache domain via
//!    `GetLogicalProcessorInformationEx(RelationCache)` — a dual-CCD Ryzen
//!    enumerates as two distinct L3 domains.
//! 2. Label which domain is the cache die via pinned-thread CPUID
//!    `Fn8000_001D`: the 3D V-Cache die reports the larger L3 (e.g. 96 MB vs
//!    32 MB on the 9950X3D). Equal sizes (non-X3D) keep domain order.
//! 3. One domain (or detection failure) = single-CCD display mode.
//! 4. NUMA-node masks are the fallback grouping when cache enumeration is
//!    unavailable.

#![cfg(windows)]

use windows::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, CACHE_RELATIONSHIP, GROUP_AFFINITY,
    LOGICAL_PROCESSOR_RELATIONSHIP, NUMA_NODE_RELATIONSHIP, PROCESSOR_RELATIONSHIP,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};

#[derive(Clone, Debug, PartialEq)]
pub struct CcdTopology {
    pub dual: bool,
    /// Kernel logical-processor masks (single-group systems).
    pub cache_mask: u64,
    pub freq_mask: u64,
    pub detail: String,
}

impl CcdTopology {
    pub fn single(mask: u64, detail: impl Into<String>) -> Self {
        CcdTopology {
            dual: false,
            cache_mask: mask,
            freq_mask: 0,
            detail: detail.into(),
        }
    }
}

/// Detect topology on the live machine.
pub fn detect() -> CcdTopology {
    let mut masks = l3_domain_masks();
    let mut source = "L3 cache domains";
    if masks.len() < 2 {
        let numa = numa_node_masks();
        if numa.len() > masks.len() {
            masks = numa;
            source = "NUMA nodes";
        }
    }
    if masks.len() >= 2 {
        let mut sorted = masks.clone();
        sorted.sort_by_key(|m| (group_of(*m), lowest_bit(*m)));
        let (a, b) = (sorted[0], sorted[1]);
        let (cache, freq) = label_by_l3_size(a, b);
        return CcdTopology {
            dual: true,
            cache_mask: cache,
            freq_mask: freq,
            detail: format!("{}: cache=0x{:x} frequency=0x{:x}", source, cache, freq),
        };
    }
    if let Some(&only) = masks.first() {
        return CcdTopology::single(only, format!("{}: single domain 0x{:x}", source, only));
    }
    CcdTopology::single(all_lp_mask(), "fallback: all logical processors")
}

/// Build a topology from explicit logical-processor lists (manual override).
pub fn from_lists(cache: &[u32], freq: &[u32]) -> CcdTopology {
    let to_mask = |list: &[u32]| -> u64 {
        list.iter()
            .filter(|&&lp| lp < 64)
            .fold(0u64, |acc, &lp| acc | (1u64 << lp))
    };
    let cache_mask = to_mask(cache);
    let freq_mask = to_mask(freq);
    if cache_mask != 0 && freq_mask != 0 {
        CcdTopology {
            dual: true,
            cache_mask,
            freq_mask,
            detail: "manual processor lists".into(),
        }
    } else {
        CcdTopology::single(cache_mask | freq_mask, "manual processor lists (single)")
    }
}

fn group_of(mask: u64) -> u64 {
    mask >> 32
}

fn lowest_bit(mask: u64) -> u32 {
    if mask == 0 {
        u32::MAX
    } else {
        mask.trailing_zeros()
    }
}

fn l3_domain_masks() -> Vec<u64> {
    let records = query_information_ex(LOGICAL_PROCESSOR_RELATIONSHIP(2)); // RelationCache
    let mut out: Vec<u64> = Vec::new();
    for record in &records {
        if record_relationship(record) != Some(LOGICAL_PROCESSOR_RELATIONSHIP(2)) {
            continue;
        }
        let body = std::mem::offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous);
        let Some(level) = read_u8(
            record,
            body + std::mem::offset_of!(CACHE_RELATIONSHIP, Level),
        ) else {
            return Vec::new();
        };
        let group = body + std::mem::offset_of!(CACHE_RELATIONSHIP, Anonymous);
        let Some((group_mask, group_number)) = read_group_affinity(record, group) else {
            return Vec::new();
        };
        if level != 3 {
            continue;
        }
        let mask = (group_mask as u64) | ((group_number as u64) << 32);
        if mask != 0 && !out.contains(&mask) {
            out.push(mask);
        }
    }
    out
}

fn numa_node_masks() -> Vec<u64> {
    let records = query_information_ex(LOGICAL_PROCESSOR_RELATIONSHIP(1)); // RelationNumaNode
    let mut out: Vec<u64> = Vec::new();
    for record in &records {
        if record_relationship(record) != Some(LOGICAL_PROCESSOR_RELATIONSHIP(1)) {
            continue;
        }
        let body = std::mem::offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous);
        let group = body + std::mem::offset_of!(NUMA_NODE_RELATIONSHIP, Anonymous);
        let Some((group_mask, group_number)) = read_group_affinity(record, group) else {
            return Vec::new();
        };
        let mask = (group_mask as u64) | ((group_number as u64) << 32);
        if mask != 0 && !out.contains(&mask) {
            out.push(mask);
        }
    }
    out
}

fn all_lp_mask() -> u64 {
    let records = query_information_ex(LOGICAL_PROCESSOR_RELATIONSHIP(0)); // RelationProcessorCore
    let mut mask: u64 = 0;
    for record in &records {
        if record_relationship(record) != Some(LOGICAL_PROCESSOR_RELATIONSHIP(0)) {
            continue;
        }
        let body = std::mem::offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous);
        let count_offset = body + std::mem::offset_of!(PROCESSOR_RELATIONSHIP, GroupCount);
        let group_offset = body + std::mem::offset_of!(PROCESSOR_RELATIONSHIP, GroupMask);
        let Some(group_count) = read_u16(record, count_offset) else {
            return 0;
        };
        for index in 0..group_count as usize {
            let Some(offset) = index
                .checked_mul(std::mem::size_of::<GROUP_AFFINITY>())
                .and_then(|distance| group_offset.checked_add(distance))
            else {
                return 0;
            };
            let Some((group_mask, group_number)) = read_group_affinity(record, offset) else {
                return 0;
            };
            mask |= (group_mask as u64) | ((group_number as u64) << 32);
        }
    }
    mask
}

fn query_information_ex(relationship: LOGICAL_PROCESSOR_RELATIONSHIP) -> Vec<Vec<u8>> {
    unsafe {
        let mut needed = 0u32;
        let probe = GetLogicalProcessorInformationEx(relationship, None, &mut needed);
        if !valid_size_probe(probe, needed) {
            return Vec::new();
        }
        let words = (needed as usize).div_ceil(std::mem::size_of::<usize>());
        let mut buffer = vec![0usize; words];
        if GetLogicalProcessorInformationEx(
            relationship,
            Some(buffer.as_mut_ptr().cast()),
            &mut needed,
        )
        .is_err()
        {
            return Vec::new();
        }
        let byte_capacity = buffer.len() * std::mem::size_of::<usize>();
        if needed as usize > byte_capacity {
            return Vec::new();
        }
        let bytes = std::slice::from_raw_parts(buffer.as_ptr().cast(), needed as usize);
        parse_records(bytes)
            .map(|records| records.into_iter().map(<[u8]>::to_vec).collect())
            .unwrap_or_default()
    }
}

fn parse_records(bytes: &[u8]) -> Option<Vec<&[u8]>> {
    let mut records = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let remaining = bytes.get(offset..)?;
        let size = read_u32(remaining, 4)? as usize;
        if size < 8 {
            return None;
        }
        let end = offset.checked_add(size)?;
        records.push(bytes.get(offset..end)?);
        offset = end;
    }
    Some(records)
}

fn record_relationship(record: &[u8]) -> Option<LOGICAL_PROCESSOR_RELATIONSHIP> {
    Some(LOGICAL_PROCESSOR_RELATIONSHIP(read_i32(record, 0)?))
}

fn read_group_affinity(record: &[u8], offset: usize) -> Option<(usize, u16)> {
    record.get(offset..offset.checked_add(std::mem::size_of::<GROUP_AFFINITY>())?)?;
    let mask = read_usize(
        record,
        offset.checked_add(std::mem::offset_of!(GROUP_AFFINITY, Mask))?,
    )?;
    let group = read_u16(
        record,
        offset.checked_add(std::mem::offset_of!(GROUP_AFFINITY, Group))?,
    )?;
    Some((mask, group))
}

fn read_bytes<const N: usize>(record: &[u8], offset: usize) -> Option<[u8; N]> {
    record.get(offset..offset.checked_add(N)?)?.try_into().ok()
}

fn read_u8(record: &[u8], offset: usize) -> Option<u8> {
    Some(read_bytes::<1>(record, offset)?[0])
}

fn read_u16(record: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(read_bytes(record, offset)?))
}

fn read_u32(record: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(read_bytes(record, offset)?))
}

fn read_i32(record: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_ne_bytes(read_bytes(record, offset)?))
}

fn read_usize(record: &[u8], offset: usize) -> Option<usize> {
    Some(usize::from_ne_bytes(read_bytes(record, offset)?))
}

fn valid_size_probe(result: windows::core::Result<()>, needed: u32) -> bool {
    needed > 0
        && matches!(result, Err(error) if error.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult())
}

/// Label two CCD masks: the die with the larger L3 (CPUID Fn8000_001D) is the
/// 3D V-Cache die. Equal sizes keep ascending order.
fn label_by_l3_size(a: u64, b: u64) -> (u64, u64) {
    let size_a = l3_size_for_mask(a);
    let size_b = l3_size_for_mask(b);
    if size_b > size_a {
        (b, a)
    } else {
        (a, b)
    }
}

fn l3_size_for_mask(mask: u64) -> u64 {
    let first_bit = mask.trailing_zeros();
    if first_bit >= 64 {
        return 0;
    }
    let Some(previous) = pin_to(first_bit) else {
        return 0;
    };
    let size = cpuid_l3_size();
    restore_affinity(previous);
    size
}

fn pin_to(lp: u32) -> Option<usize> {
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
        let thread = GetCurrentThread();
        let previous = SetThreadAffinityMask(thread, 1usize << lp);
        if previous == 0 {
            None
        } else {
            Some(previous)
        }
    }
}

fn restore_affinity(mask: usize) {
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
        let _ = SetThreadAffinityMask(GetCurrentThread(), mask);
    }
}

#[cfg(target_arch = "x86_64")]
fn cpuid_l3_size() -> u64 {
    use std::arch::x86_64::__cpuid_count;
    // Max subleaf for Fn8000_001D is in ECX when ECX=0.
    let max_leaf = __cpuid_count(0x8000_001D, 0).eax;
    for subleaf in 0..=max_leaf {
        let r = __cpuid_count(0x8000_001D, subleaf);
        let cache_type = r.eax & 0x1F;
        let level = (r.eax >> 5) & 0x7;
        if cache_type != 0 && level == 3 {
            let ways = ((r.ebx >> 22) & 0x3FF) + 1;
            let partitions = ((r.ebx >> 12) & 0x3FF) + 1;
            let line_size = (r.ebx & 0xFFF) + 1;
            let sets = r.ecx + 1;
            return ways as u64 * partitions as u64 * line_size as u64 * sets as u64;
        }
    }
    0
}

#[cfg(not(target_arch = "x86_64"))]
fn cpuid_l3_size() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_lists_builds_masks() {
        let t = from_lists(&[0, 1, 2, 3], &[4, 5, 6, 7]);
        assert!(t.dual);
        assert_eq!(t.cache_mask, 0b1111);
        assert_eq!(t.freq_mask, 0b1111_0000);
    }

    #[test]
    fn from_lists_single_when_one_side_missing() {
        let t = from_lists(&[0, 1], &[]);
        assert!(!t.dual);
        assert_eq!(t.cache_mask, 0b11);
    }

    #[test]
    fn ordering_uses_lowest_bit_then_group() {
        assert!(group_of(1u64 << 40) > group_of(1));
        assert!(lowest_bit(0b1000) < lowest_bit(0b10000));
        assert_eq!(lowest_bit(0), u32::MAX);
    }

    #[test]
    fn size_probe_requires_insufficient_buffer_and_nonzero_size() {
        use windows::Win32::Foundation::ERROR_ACCESS_DENIED;

        assert!(valid_size_probe(ERROR_INSUFFICIENT_BUFFER.ok(), 1));
        assert!(!valid_size_probe(Ok(()), 1));
        assert!(!valid_size_probe(ERROR_ACCESS_DENIED.ok(), 1));
        assert!(!valid_size_probe(ERROR_INSUFFICIENT_BUFFER.ok(), 0));
    }

    #[test]
    fn raw_records_reject_truncated_inventory_without_partial_results() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0i32.to_ne_bytes());
        bytes.extend_from_slice(&8u32.to_ne_bytes());
        bytes.extend_from_slice(&0i32.to_ne_bytes());
        bytes.extend_from_slice(&16u32.to_ne_bytes());
        assert!(parse_records(&bytes).is_none());
    }

    #[test]
    #[ignore = "native 9950X3D topology check"]
    fn live_9950x3d_topology() {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};

        let thread = unsafe { GetCurrentThread() };
        let before = unsafe { SetThreadAffinityMask(thread, usize::MAX) };
        assert_ne!(before, 0, "failed to probe affinity before detect");
        assert_ne!(unsafe { SetThreadAffinityMask(thread, before) }, 0);
        let topology = detect();
        let after = unsafe { SetThreadAffinityMask(thread, usize::MAX) };
        assert_ne!(after, 0, "failed to probe affinity after detect");
        assert_ne!(unsafe { SetThreadAffinityMask(thread, after) }, 0);
        println!("affinity before=0x{before:x} after=0x{after:x}; {topology:?}");
        assert_eq!(before, after);
        assert!(topology.dual);
        assert_eq!(topology.cache_mask, 0xFFFF);
        assert_eq!(topology.freq_mask, 0xFFFF0000);
        assert!(topology.detail.starts_with("L3 cache domains:"));
    }
}
