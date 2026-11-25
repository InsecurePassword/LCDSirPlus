//! Count established IPv4 and IPv6 TCP connections.

#![cfg(windows)]
#![allow(dead_code)]

use std::mem::{align_of, size_of};

use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    GetTcp6Table2, GetTcpTable2, MIB_TCP6ROW2, MIB_TCP6TABLE2, MIB_TCPROW2, MIB_TCPTABLE2,
    MIB_TCP_STATE_ESTAB,
};

const MAX_TABLE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct ConnectionsSample {
    pub established: u32,
    pub sampled_at: std::time::SystemTime,
}

pub fn sample() -> Result<ConnectionsSample, String> {
    sample_with(query_table)
}

fn query_table(ipv6: bool, storage: Option<&mut [usize]>, size: &mut u32) -> u32 {
    unsafe {
        if ipv6 {
            GetTcp6Table2(
                storage.map_or(std::ptr::null_mut(), |words| {
                    words.as_mut_ptr().cast::<MIB_TCP6TABLE2>()
                }),
                size,
                false,
            )
        } else {
            GetTcpTable2(
                storage.map(|words| words.as_mut_ptr().cast::<MIB_TCPTABLE2>()),
                size,
                false,
            )
        }
    }
}

fn sample_with(
    mut query: impl FnMut(bool, Option<&mut [usize]>, &mut u32) -> u32,
) -> Result<ConnectionsSample, String> {
    let v4 = table_with(false, &mut query, |bytes| {
        decode(bytes, size_of::<MIB_TCPROW2>(), offset_of_state_v4())
    })?;
    // Perform the v6 read before surfacing a v4 decode failure.
    let established = table_with(true, &mut query, |bytes| {
        v4?.checked_add(decode(
            bytes,
            size_of::<MIB_TCP6ROW2>(),
            offset_of_state_v6(),
        )?)
        .ok_or_else(|| "established TCP count overflow".to_string())
    })??;
    Ok(ConnectionsSample {
        established,
        sampled_at: std::time::SystemTime::now(),
    })
}

fn table_with<T>(
    ipv6: bool,
    query: &mut impl FnMut(bool, Option<&mut [usize]>, &mut u32) -> u32,
    consume: impl FnOnce(&[u8]) -> T,
) -> Result<T, String> {
    let mut size = 0u32;
    let first = query(ipv6, None, &mut size);
    if first != ERROR_INSUFFICIENT_BUFFER.0 {
        return Err(format!("TCP table sizing failed ({first})"));
    }
    let size = usize::try_from(size).map_err(|_| "TCP table size overflow")?;
    if !(size_of::<u32>()..=MAX_TABLE_BYTES).contains(&size) {
        return Err(format!("TCP table size out of bounds ({size})"));
    }
    let words = size.div_ceil(size_of::<usize>());
    let mut storage = vec![0usize; words];
    let mut actual = size as u32;
    let status = query(ipv6, Some(&mut storage), &mut actual);
    if status != NO_ERROR.0 || actual as usize > size {
        return Err(format!("TCP table read failed ({status})"));
    }
    // The aligned storage is initialized, and the view is capped by the supplied size.
    let bytes =
        unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), actual as usize) };
    Ok(consume(bytes))
}

fn decode(bytes: &[u8], row_size: usize, state_offset: usize) -> Result<u32, String> {
    if bytes.len() < 4 || row_size == 0 || state_offset.checked_add(4).is_none_or(|n| n > row_size)
    {
        return Err("invalid TCP table layout".into());
    }
    let count = u32::from_ne_bytes(bytes[..4].try_into().unwrap()) as usize;
    let required = 4usize
        .checked_add(
            count
                .checked_mul(row_size)
                .ok_or("TCP row count overflow")?,
        )
        .ok_or("TCP table range overflow")?;
    if required > bytes.len() || required > MAX_TABLE_BYTES {
        return Err("TCP table row count exceeds buffer".into());
    }
    let mut established = 0u32;
    for row in bytes[4..required].chunks_exact(row_size) {
        let state = i32::from_ne_bytes(row[state_offset..state_offset + 4].try_into().unwrap());
        established += u32::from(state == MIB_TCP_STATE_ESTAB.0);
    }
    Ok(established)
}

fn offset_of_state_v4() -> usize {
    let row = std::mem::MaybeUninit::<MIB_TCPROW2>::uninit();
    unsafe { std::ptr::addr_of!((*row.as_ptr()).dwState) as usize - row.as_ptr() as usize }
}

fn offset_of_state_v6() -> usize {
    let row = std::mem::MaybeUninit::<MIB_TCP6ROW2>::uninit();
    unsafe { std::ptr::addr_of!((*row.as_ptr()).State) as usize - row.as_ptr() as usize }
}

const _: () = assert!(align_of::<MIB_TCPROW2>() <= align_of::<usize>());
const _: () = assert!(align_of::<MIB_TCP6ROW2>() <= align_of::<usize>());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_tcp_tables_preserve_native_layout_errors_and_avoid_byte_copies() {
        fn blob(ipv6: bool, states: &[i32]) -> Vec<u8> {
            let (size, offset) = if ipv6 {
                (size_of::<MIB_TCP6ROW2>(), offset_of_state_v6())
            } else {
                (size_of::<MIB_TCPROW2>(), offset_of_state_v4())
            };
            let mut data = vec![0; 4 + states.len() * size];
            data[..4].copy_from_slice(&(states.len() as u32).to_ne_bytes());
            for (i, state) in states.iter().enumerate() {
                let start = 4 + i * size + offset;
                data[start..start + 4].copy_from_slice(&state.to_ne_bytes());
            }
            data
        }

        fn run(
            v4: &[u8],
            v6: &[u8],
            fill_error: Option<bool>,
            oversized: Option<bool>,
        ) -> (Result<u32, String>, usize, usize) {
            let mut calls = 0;
            let mut steps = [None; 4];
            let (result, allocations) = {
                let _window = crate::backends::alloc_count::window();
                let result = sample_with(|ipv6, storage, actual| {
                    let data = if ipv6 { v6 } else { v4 };
                    steps[calls] = Some((ipv6, storage.is_some()));
                    calls += 1;
                    match storage {
                        None => {
                            // Keep the sizing request valid even when a later fill is short.
                            *actual = data.len().max(4) as u32;
                            ERROR_INSUFFICIENT_BUFFER.0
                        }
                        Some(_) if fill_error == Some(ipv6) => 5,
                        Some(_) if oversized == Some(ipv6) => {
                            *actual += 1;
                            NO_ERROR.0
                        }
                        Some(words) => {
                            assert_eq!(words.as_ptr().align_offset(align_of::<usize>()), 0);
                            for (i, chunk) in data.chunks(size_of::<usize>()).enumerate() {
                                let mut bytes = [0; size_of::<usize>()];
                                bytes[..chunk.len()].copy_from_slice(chunk);
                                words[i] = usize::from_ne_bytes(bytes);
                            }
                            *actual = data.len() as u32;
                            NO_ERROR.0
                        }
                    }
                });
                (
                    result.map(|s| s.established),
                    crate::backends::alloc_count::snapshot().4,
                )
            };
            let expected = [
                Some((false, false)),
                Some((false, true)),
                Some((true, false)),
                Some((true, true)),
            ];
            assert_eq!(&steps[..calls], &expected[..calls]);
            (result, allocations, calls)
        }

        let established = MIB_TCP_STATE_ESTAB.0;
        let v4 = blob(false, &[established, 2, established]);
        let v6 = blob(true, &[2, established]);
        let short = [0u8; 3];
        let oversized_count = u32::MAX.to_ne_bytes();
        assert_eq!(run(&v4, &v6, None, None).0.unwrap(), 3);
        assert_eq!(
            run(&short, &v6, Some(true), None).0.unwrap_err(),
            "TCP table read failed (5)"
        );
        assert_eq!(
            run(&v4, &short, None, None).0.unwrap_err(),
            "invalid TCP table layout"
        );
        assert_eq!(
            run(&short, &oversized_count, None, None).0.unwrap_err(),
            "invalid TCP table layout"
        );
        assert_eq!(
            run(&v4, &oversized_count, None, None).0.unwrap_err(),
            "TCP table row count exceeds buffer"
        );
        assert_eq!(
            run(&v4, &v6, None, Some(true)).0.unwrap_err(),
            "TCP table read failed (0)"
        );
        let (first_read, _, calls) = run(&v4, &v6, Some(false), None);
        assert_eq!(first_read.unwrap_err(), "TCP table read failed (5)");
        assert_eq!(calls, 2);
        let (total, allocs, calls) = run(&v4, &v6, None, None);
        assert_eq!((total.unwrap(), calls), (3, 4));
        assert_eq!(allocs, 2, "only the two aligned table allocations");
    }

    #[test]
    fn decoder_is_bounded_and_counts_only_established() {
        let mut bytes = 3u32.to_ne_bytes().to_vec();
        for state in [MIB_TCP_STATE_ESTAB.0, 2, MIB_TCP_STATE_ESTAB.0] {
            bytes.extend_from_slice(&state.to_ne_bytes());
        }
        assert_eq!(decode(&bytes, 4, 0).unwrap(), 2);
        bytes.pop();
        assert!(decode(&bytes, 4, 0).is_err());
        assert!(decode(&u32::MAX.to_ne_bytes(), usize::MAX, 0).is_err());
    }
}
