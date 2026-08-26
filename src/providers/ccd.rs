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

use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, LOGICAL_PROCESSOR_RELATIONSHIP,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};

#[derive(Clone, Debug, PartialEq)]
pub struct CcdTopology {
    pub dual: bool,
    /// Kernel logical-processor masks (single-group systems).
    pub cache_mask: u64,
    pub freq_mask: u64,
    pub lp_count: usize,
    pub detail: String,
}

impl CcdTopology {
    pub fn single(mask: u64, detail: impl Into<String>) -> Self {
        CcdTopology {
            dual: false,
            cache_mask: mask,
            freq_mask: 0,
            lp_count: mask.count_ones() as usize,
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
        let lp_count = (a | b).count_ones() as usize;
        return CcdTopology {
            dual: true,
            cache_mask: cache,
            freq_mask: freq,
            lp_count,
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
            lp_count: (cache_mask | freq_mask).count_ones() as usize,
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
        if record.Relationship != LOGICAL_PROCESSOR_RELATIONSHIP(2) {
            continue;
        }
        // RelationCache arms are CACHE_RELATIONSHIP: Level + GroupMask union.
        let cache = unsafe { &record.Anonymous.Cache };
        if cache.Level != 3 {
            continue;
        }
        let group_mask = unsafe { cache.Anonymous.GroupMask };
        let mask = (group_mask.Mask as u64) | ((group_mask.Group as u64) << 32);
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
        if record.Relationship != LOGICAL_PROCESSOR_RELATIONSHIP(1) {
            continue;
        }
        let group_mask = unsafe { record.Anonymous.NumaNode.Anonymous.GroupMask };
        let mask = (group_mask.Mask as u64) | ((group_mask.Group as u64) << 32);
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
        if record.Relationship != LOGICAL_PROCESSOR_RELATIONSHIP(0) {
            continue;
        }
        for group_mask in unsafe { &record.Anonymous.Processor.GroupMask } {
            mask |= (group_mask.Mask as u64) | ((group_mask.Group as u64) << 32);
        }
    }
    mask
}

fn query_information_ex(
    relationship: LOGICAL_PROCESSOR_RELATIONSHIP,
) -> Vec<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX> {
    unsafe {
        let mut needed = 0u32;
        if GetLogicalProcessorInformationEx(relationship, None, &mut needed).is_err() || needed == 0
        {
            return Vec::new();
        }
        let mut buffer = vec![0u8; needed as usize];
        if GetLogicalProcessorInformationEx(
            relationship,
            Some(buffer.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut needed,
        )
        .is_err()
        {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut offset = 0usize;
        while offset + std::mem::size_of::<u32>() * 2 <= buffer.len() {
            let record =
                &*(buffer.as_ptr().add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
            if record.Size == 0 {
                break;
            }
            // Copy out to detach from the byte buffer.
            out.push(std::ptr::read(record));
            offset += record.Size as usize;
        }
        out
    }
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
    pin_to(first_bit);
    let size = cpuid_l3_size();
    unpin();
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

fn unpin() {
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
        // Restore to the process-wide affinity (all allowed processors).
        let _ = SetThreadAffinityMask(GetCurrentThread(), usize::MAX);
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
        assert_eq!(t.lp_count, 8);
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
}
