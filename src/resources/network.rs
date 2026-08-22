use std::{
    collections::{HashMap, HashSet},
    mem::size_of,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    time::Duration,
};

const NETLINK_SOCK_DIAG: libc::c_int = 4;
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct NetworkCounters {
    pub received_bytes: u64,
    pub transmitted_bytes: u64,
}

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
    if !dump_family(
        descriptor.as_raw_fd(),
        libc::AF_INET as u8,
        1,
        requested_inodes,
        &mut counters,
    ) || !dump_family(
        descriptor.as_raw_fd(),
        libc::AF_INET6 as u8,
        2,
        requested_inodes,
        &mut counters,
    ) {
        return None;
    }
    Some(counters)
}

fn open_diag_socket() -> Option<OwnedFd> {
    // SAFETY: socket returns a new descriptor and receives constant Linux ABI values.
    let raw = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            NETLINK_SOCK_DIAG,
        )
    };
    if raw < 0 {
        return None;
    }
    // SAFETY: ownership of the successful socket descriptor is transferred exactly once.
    let descriptor = unsafe { OwnedFd::from_raw_fd(raw) };
    let timeout = libc::timeval {
        tv_sec: Duration::from_millis(500).as_secs() as libc::time_t,
        tv_usec: 500_000,
    };
    // SAFETY: the timeout points to an initialized timeval with the correct length.
    let configured = unsafe {
        libc::setsockopt(
            descriptor.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw const timeout).cast(),
            size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    (configured == 0).then_some(descriptor)
}

fn dump_family(
    descriptor: libc::c_int,
    family: u8,
    sequence: u32,
    requested_inodes: &HashSet<u64>,
    counters: &mut HashMap<u64, NetworkCounters>,
) -> bool {
    let request = diag_request(family, sequence);
    // SAFETY: request is a live contiguous buffer for the duration of send.
    if unsafe { libc::send(descriptor, request.as_ptr().cast(), request.len(), 0) } < 0 {
        return false;
    }
    let mut response = vec![0_u8; 64 * 1024];
    loop {
        // SAFETY: response exposes its full initialized allocation as a mutable receive buffer.
        let received =
            unsafe { libc::recv(descriptor, response.as_mut_ptr().cast(), response.len(), 0) };
        if received <= 0 {
            return false;
        }
        let mut offset = 0;
        let received = received as usize;
        while offset + 16 <= received {
            let length = read_u32(&response, offset) as usize;
            if length < 16 || offset + length > received {
                return false;
            }
            let message_type = read_u16(&response, offset + 4);
            let message_sequence = read_u32(&response, offset + 8);
            if message_sequence == sequence {
                if message_type == NLMSG_DONE {
                    return true;
                }
                if message_type == NLMSG_ERROR {
                    return false;
                }
                parse_diag_message(
                    &response[offset + 16..offset + length],
                    requested_inodes,
                    counters,
                );
            }
            offset += align4(length);
        }
    }
}

fn diag_request(family: u8, sequence: u32) -> [u8; 72] {
    let mut request = [0_u8; 72];
    write_u32(&mut request, 0, 72);
    write_u16(&mut request, 4, SOCK_DIAG_BY_FAMILY);
    write_u16(&mut request, 6, NLM_F_REQUEST | NLM_F_DUMP);
    write_u32(&mut request, 8, sequence);
    request[16] = family;
    request[17] = libc::IPPROTO_TCP as u8;
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
        let length = read_u16(message, offset) as usize;
        let attribute_type = read_u16(message, offset + 2);
        if length < 4 || offset + length > message.len() {
            return;
        }
        let payload = &message[offset + 4..offset + length];
        if attribute_type == INET_DIAG_INFO && payload.len() >= TCP_INFO_COUNTERS_LENGTH {
            counters.insert(
                inode,
                NetworkCounters {
                    transmitted_bytes: read_u64(payload, TCP_INFO_BYTES_ACKED_OFFSET),
                    received_bytes: read_u64(payload, TCP_INFO_BYTES_RECEIVED_OFFSET),
                },
            );
            return;
        }
        offset += align4(length);
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
    use super::*;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
    };

    #[test]
    fn reads_counters_for_a_live_tcp_connection() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback listener");
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).expect("connect");
        let (mut server, _) = listener.accept().expect("accept");
        client.write_all(b"request").unwrap();
        let mut request = [0_u8; 7];
        server.read_exact(&mut request).unwrap();
        server.write_all(b"response").unwrap();
        let mut response = [0_u8; 8];
        client.read_exact(&mut response).unwrap();
        let link = std::fs::read_link(format!("/proc/self/fd/{}", client.as_raw_fd())).unwrap();
        let inode = link
            .to_string_lossy()
            .strip_prefix("socket:[")
            .and_then(|value| value.strip_suffix(']'))
            .and_then(|value| value.parse::<u64>().ok())
            .expect("socket inode");
        let counters = read_network_counters(&HashSet::from([inode])).expect("INET_DIAG dump");
        let counter = counters.get(&inode).expect("TCP socket counter");
        assert!(counter.transmitted_bytes >= 7);
        assert!(counter.received_bytes >= 8);
    }

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
