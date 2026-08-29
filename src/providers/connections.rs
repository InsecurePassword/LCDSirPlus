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
    let v4 = table(false)?;
    let v6 = table(true)?;
    let established = decode(&v4, size_of::<MIB_TCPROW2>(), offset_of_state_v4())?
        .checked_add(decode(
            &v6,
            size_of::<MIB_TCP6ROW2>(),
            offset_of_state_v6(),
        )?)
        .ok_or("established TCP count overflow")?;
    Ok(ConnectionsSample {
        established,
        sampled_at: std::time::SystemTime::now(),
    })
}

fn table(ipv6: bool) -> Result<Vec<u8>, String> {
    unsafe {
        let mut size = 0u32;
        let first = if ipv6 {
            GetTcp6Table2(std::ptr::null_mut(), &mut size, false)
        } else {
            GetTcpTable2(None, &mut size, false)
        };
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
        let status = if ipv6 {
            GetTcp6Table2(
                storage.as_mut_ptr().cast::<MIB_TCP6TABLE2>(),
                &mut actual,
                false,
            )
        } else {
            GetTcpTable2(
                Some(storage.as_mut_ptr().cast::<MIB_TCPTABLE2>()),
                &mut actual,
                false,
            )
        };
        if status != NO_ERROR.0 || actual as usize > size {
            return Err(format!("TCP table read failed ({status})"));
        }
        Ok(std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), actual as usize).to_vec())
    }
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
