use std::{
    collections::{HashMap, HashSet},
    mem::size_of,
    os::fd::OwnedFd,
    time::Duration,
};

use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, SocketFlags, SocketType, netlink, recv, send, socket_with,
    sockopt::{Timeout, set_socket_timeout},
};

const IPV4_FAMILY: u8 = 2;
const IPV6_FAMILY: u8 = 10;
const TCP_PROTOCOL: u8 = 6;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const NLM_F_REQUEST: u16 = 0x01;
const NLM_F_DUMP: u16 = 0x300;
const NLMSG_ERROR: u16 = 0x02;
const NLMSG_DONE: u16 = 0x03;
const INET_DIAG_INFO: u16 = 2;
const INET_DIAG_MESSAGE_LENGTH: usize = 72;
const TCP_INFO_BYTES_ACKED_OFFSET: usize = 120;
const TCP_INFO_BYTES_RECEIVED_OFFSET: usize = 128;
const TCP_INFO_COUNTERS_LENGTH: usize = TCP_INFO_BYTES_RECEIVED_OFFSET + size_of::<u64>();

use super::provider::NetworkCounters;

/// Reads cumulative TCP counters for the requested socket inodes through INET_DIAG.
/// UDP and Unix sockets do not expose equivalent lifetime byte counters, so callers
/// retain capability metadata when no requested TCP socket is present.
pub(super) fn read_network_counters(
    requested_inodes: &HashSet<u64>,
) -> Option<HashMap<u64, NetworkCounters>> {
    if requested_inodes.is_empty() {
        return Some(HashMap::new());
    }
    let descriptor = open_diag_socket()?;
    let mut counters = HashMap::new();
    if !dump_family(&descriptor, IPV4_FAMILY, 1, requested_inodes, &mut counters)
        || !dump_family(&descriptor, IPV6_FAMILY, 2, requested_inodes, &mut counters)
    {
        return None;
    }
    Some(counters)
}

fn open_diag_socket() -> Option<OwnedFd> {
    let descriptor = socket_with(
        AddressFamily::NETLINK,
        SocketType::RAW,
        SocketFlags::CLOEXEC,
        Some(netlink::SOCK_DIAG),
    )
    .ok()?;
    set_socket_timeout(&descriptor, Timeout::Recv, Some(Duration::from_millis(500))).ok()?;
    Some(descriptor)
}

fn dump_family(
    descriptor: &OwnedFd,
    family: u8,
    sequence: u32,
    requested_inodes: &HashSet<u64>,
    counters: &mut HashMap<u64, NetworkCounters>,
) -> bool {
    let request = diag_request(family, sequence);
    if send(descriptor, &request, SendFlags::empty()).is_err() {
        return false;
    }
    let mut response = vec![0_u8; 64 * 1024];
    loop {
        let Ok((_, received)) = recv(descriptor, &mut response, RecvFlags::empty()) else {
            return false;
        };
        match process_dump_messages(&response[..received], sequence, requested_inodes, counters) {
            DumpStatus::Pending => {}
            DumpStatus::Complete => return true,
            DumpStatus::Failed => return false,
        }
    }
}

#[derive(Clone, Copy)]
enum DumpStatus {
    Pending,
    Complete,
    Failed,
}

fn process_dump_messages(
    response: &[u8],
    sequence: u32,
    requested_inodes: &HashSet<u64>,
    counters: &mut HashMap<u64, NetworkCounters>,
) -> DumpStatus {
    let mut offset = 0;
    while offset + 16 <= response.len() {
        let Some(message) = netlink_message(response, offset) else {
            return DumpStatus::Failed;
        };
        offset = message.next_offset;
        if message.sequence != sequence {
            continue;
        }
        match message.kind {
            NLMSG_DONE => return DumpStatus::Complete,
            NLMSG_ERROR => return DumpStatus::Failed,
            _ => parse_diag_message(message.payload, requested_inodes, counters),
        }
    }
    DumpStatus::Pending
}

struct NetlinkMessage<'a> {
    kind: u16,
    sequence: u32,
    payload: &'a [u8],
    next_offset: usize,
}

fn netlink_message(response: &[u8], offset: usize) -> Option<NetlinkMessage<'_>> {
    let length = read_u32(response, offset) as usize;
    let end = offset.checked_add(length)?;
    (length >= 16 && end <= response.len()).then(|| NetlinkMessage {
        kind: read_u16(response, offset + 4),
        sequence: read_u32(response, offset + 8),
        payload: &response[offset + 16..end],
        next_offset: offset + align4(length),
    })
}

fn diag_request(family: u8, sequence: u32) -> [u8; 72] {
    let mut request = [0_u8; 72];
    write_u32(&mut request, 0, 72);
    write_u16(&mut request, 4, SOCK_DIAG_BY_FAMILY);
    write_u16(&mut request, 6, NLM_F_REQUEST | NLM_F_DUMP);
    write_u32(&mut request, 8, sequence);
    request[16] = family;
    request[17] = TCP_PROTOCOL;
    request[18] = 1 << (INET_DIAG_INFO - 1);
    write_u32(&mut request, 20, u32::MAX);
    // inet_diag_no_cookie asks the kernel not to filter by a specific socket cookie.
    write_u32(&mut request, 64, u32::MAX);
    write_u32(&mut request, 68, u32::MAX);
    request
}

fn parse_diag_message(
    message: &[u8],
    requested_inodes: &HashSet<u64>,
    counters: &mut HashMap<u64, NetworkCounters>,
) {
    if message.len() < INET_DIAG_MESSAGE_LENGTH {
        return;
    }
    let inode = read_u32(message, 68) as u64;
    if !requested_inodes.contains(&inode) {
        return;
    }
    let mut offset = INET_DIAG_MESSAGE_LENGTH;
    while offset + 4 <= message.len() {
        let Some((attribute_type, payload, next_offset)) = diag_attribute(message, offset) else {
            return;
        };
        if attribute_type == INET_DIAG_INFO && payload.len() >= TCP_INFO_COUNTERS_LENGTH {
            counters.insert(inode, tcp_counters(payload));
            return;
        }
        offset = next_offset;
    }
}

fn diag_attribute(message: &[u8], offset: usize) -> Option<(u16, &[u8], usize)> {
    let length = read_u16(message, offset) as usize;
    let end = offset.checked_add(length)?;
    (length >= 4 && end <= message.len()).then(|| {
        (
            read_u16(message, offset + 2),
            &message[offset + 4..end],
            offset + align4(length),
        )
    })
}

fn tcp_counters(payload: &[u8]) -> NetworkCounters {
    NetworkCounters {
        transmitted_bytes: read_u64(payload, TCP_INFO_BYTES_ACKED_OFFSET),
        received_bytes: read_u64(payload, TCP_INFO_BYTES_RECEIVED_OFFSET),
    }
}

fn align4(value: usize) -> usize {
    (value + 3) & !3
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_ne_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap_or_default())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_ne_bytes(bytes[offset..offset + 8].try_into().unwrap_or_default())
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

#[cfg(test)]
mod tests {
    use super::{
        HashMap, HashSet, INET_DIAG_INFO, INET_DIAG_MESSAGE_LENGTH, NetworkCounters,
        TCP_INFO_BYTES_ACKED_OFFSET, TCP_INFO_BYTES_RECEIVED_OFFSET, TCP_INFO_COUNTERS_LENGTH,
        parse_diag_message, write_u16, write_u32,
    };

    #[test]
    fn parses_tcp_info_counters_for_requested_inode() {
        let inode = 42_u64;
        let mut message = vec![0_u8; INET_DIAG_MESSAGE_LENGTH + 4 + TCP_INFO_COUNTERS_LENGTH];
        write_u32(&mut message, 68, inode as u32);
        write_u16(
            &mut message,
            INET_DIAG_MESSAGE_LENGTH,
            (4 + TCP_INFO_COUNTERS_LENGTH) as u16,
        );
        write_u16(&mut message, INET_DIAG_MESSAGE_LENGTH + 2, INET_DIAG_INFO);
        let payload = INET_DIAG_MESSAGE_LENGTH + 4;
        message[payload + TCP_INFO_BYTES_ACKED_OFFSET..payload + TCP_INFO_BYTES_ACKED_OFFSET + 8]
            .copy_from_slice(&1234_u64.to_ne_bytes());
        message[payload + TCP_INFO_BYTES_RECEIVED_OFFSET
            ..payload + TCP_INFO_BYTES_RECEIVED_OFFSET + 8]
            .copy_from_slice(&5678_u64.to_ne_bytes());
        let mut counters = HashMap::new();
        parse_diag_message(&message, &HashSet::from([inode]), &mut counters);
        assert_eq!(
            counters[&inode],
            NetworkCounters {
                transmitted_bytes: 1234,
                received_bytes: 5678,
            }
        );
    }
}
