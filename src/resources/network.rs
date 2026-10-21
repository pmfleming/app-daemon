use std::{
    collections::{HashMap, HashSet},
    os::fd::OwnedFd,
    sync::Arc,
    time::{Duration, Instant},
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
const NLM_F_DUMP_INTR: u16 = 0x10;
const NLMSG_NOOP: u16 = 0x01;
const NLMSG_DONE: u16 = 0x03;
const DUMP_TIMEOUT: Duration = Duration::from_millis(500);
const INET_DIAG_INFO: u16 = 2;
const INET_DIAG_MESSAGE_LENGTH: usize = 72;
const TCP_INFO_BYTES_ACKED_OFFSET: usize = 120;
const TCP_INFO_BYTES_RECEIVED_OFFSET: usize = 128;

use super::provider::{NetworkCounters, ProcessStat, ResourceProvider};
use crate::process::Identity;

type SocketOwners = HashMap<Identity, Arc<HashSet<u64>>>;

/// Own socket continuity independently of CPU sampling and file-cache TTLs.
#[derive(Debug, Default)]
pub(super) struct Sampler {
    counters: HashMap<u64, NetworkCounters>,
    sockets: SocketOwners,
    available: bool,
}

impl Sampler {
    pub(super) fn sockets(&self, identity: Identity) -> Option<Arc<HashSet<u64>>> {
        self.sockets.get(&identity).cloned()
    }

    pub(super) fn sample(
        &mut self,
        provider: &dyn ResourceProvider,
        active: &HashSet<Identity>,
        processes: &HashMap<u32, ProcessStat>,
    ) -> (HashMap<u64, NetworkCounters>, bool) {
        self.sockets.retain(|&(pid, start), _| {
            processes
                .get(&pid)
                .is_some_and(|process| process.start_ticks == start)
        });
        let previous_sockets = socket_inodes(&self.sockets);
        // A recycled PID/inode without a continuing observer is a new baseline.
        self.counters
            .retain(|inode, _| previous_sockets.contains(inode));
        let sockets = active
            .iter()
            .filter_map(|&identity| {
                Some((identity, Arc::new(provider.process_sockets(identity.0)?)))
            })
            .collect::<SocketOwners>();
        let newly_opened = sockets
            .iter()
            .filter(|(identity, _)| self.sockets.contains_key(identity))
            .flat_map(|(_, sockets)| sockets.iter().copied())
            .filter(|inode| !previous_sockets.contains(inode))
            .collect::<HashSet<_>>();
        let sampled = provider.network_counters(&socket_inodes(&sockets));
        let available = sampled.is_some();
        let current = sampled.unwrap_or_default();
        let deltas = current
            .iter()
            .map(|(&inode, &counters)| {
                // Count initial bytes only for newly opened sockets in an observed process.
                let previous = self.counters.get(&inode).copied().unwrap_or_else(|| {
                    if self.available && newly_opened.contains(&inode) {
                        NetworkCounters::default()
                    } else {
                        counters
                    }
                });
                (inode, counters.delta(previous))
            })
            .collect();
        self.sockets = sockets;
        self.available = available;
        self.counters = current;
        (deltas, available)
    }
}

fn socket_inodes(owners: &SocketOwners) -> HashSet<u64> {
    owners
        .values()
        .flat_map(|sockets| sockets.iter().copied())
        .collect()
}

impl crate::resources::provider::NetworkCounters {
    fn delta(self, previous: Self) -> Self {
        Self {
            received_bytes: self.received_bytes.saturating_sub(previous.received_bytes),
            transmitted_bytes: self
                .transmitted_bytes
                .saturating_sub(previous.transmitted_bytes),
        }
    }
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
    let descriptor = socket_with(
        AddressFamily::NETLINK,
        SocketType::RAW,
        SocketFlags::CLOEXEC,
        Some(netlink::SOCK_DIAG),
    )
    .ok()?;
    let mut counters = HashMap::new();
    if !dump_family(&descriptor, IPV4_FAMILY, 1, requested_inodes, &mut counters)
        || !dump_family(&descriptor, IPV6_FAMILY, 2, requested_inodes, &mut counters)
    {
        return None;
    }
    Some(counters)
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
    let deadline = Instant::now() + DUMP_TIMEOUT;
    while let Some(packet) = receive_packet(descriptor, &mut response, deadline) {
        match process_dump_messages(packet, sequence, requested_inodes, counters) {
            DumpStatus::Pending => {}
            DumpStatus::Complete => return true,
            DumpStatus::Failed => return false,
        }
    }
    false
}

fn receive_packet<'a>(
    descriptor: &OwnedFd,
    buffer: &'a mut [u8],
    deadline: Instant,
) -> Option<&'a [u8]> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())?;
    set_socket_timeout(descriptor, Timeout::Recv, Some(remaining)).ok()?;
    // MSG_TRUNC reports the original datagram length, including bytes not copied.
    let (_, received) = recv(descriptor, &mut *buffer, RecvFlags::TRUNC).ok()?;
    buffer.get(..received).filter(|packet| !packet.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    if response.is_empty() {
        return DumpStatus::Failed;
    }
    let mut offset = 0;
    while offset < response.len() {
        let Some(message) = netlink_message(response, offset) else {
            return DumpStatus::Failed;
        };
        offset = message.next_offset;
        if message.sequence != sequence {
            continue;
        }
        match message.apply(requested_inodes, counters) {
            DumpStatus::Pending => {}
            terminal => return terminal,
        }
    }
    DumpStatus::Pending
}

struct NetlinkMessage<'a> {
    kind: u16,
    flags: u16,
    sequence: u32,
    payload: &'a [u8],
    next_offset: usize,
}

impl NetlinkMessage<'_> {
    fn apply(
        self,
        requested: &HashSet<u64>,
        counters: &mut HashMap<u64, NetworkCounters>,
    ) -> DumpStatus {
        if self.flags & NLM_F_DUMP_INTR != 0 {
            return DumpStatus::Failed;
        }
        match self.kind {
            NLMSG_NOOP => DumpStatus::Pending,
            NLMSG_DONE if self.payload.is_empty() || read_u32(self.payload, 0) == Some(0) => {
                DumpStatus::Complete
            }
            SOCK_DIAG_BY_FAMILY
                if parse_diag_message(self.payload, requested, counters).is_some() =>
            {
                DumpStatus::Pending
            }
            // Includes error/overrun, failed DONE status and malformed observations.
            _ => DumpStatus::Failed,
        }
    }
}

fn netlink_message(response: &[u8], offset: usize) -> Option<NetlinkMessage<'_>> {
    let length = read_u32(response, offset)? as usize;
    let (payload, next_offset) = frame(response, offset, length, 16)?;
    Some(NetlinkMessage {
        kind: read_u16(response, offset + 4)?,
        flags: read_u16(response, offset + 6)?,
        sequence: read_u32(response, offset + 8)?,
        payload,
        next_offset,
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
) -> Option<()> {
    let inode = read_u32(message, 68)? as u64;
    if !requested_inodes.contains(&inode) {
        return Some(());
    }
    let mut offset = INET_DIAG_MESSAGE_LENGTH;
    while offset < message.len() {
        let (attribute_type, payload, next_offset) = diag_attribute(message, offset)?;
        // Older kernels can expose shorter tcp_info; that is unsupported, not zero.
        if attribute_type == INET_DIAG_INFO
            && let Some(usage) = tcp_counters(payload)
        {
            counters.insert(inode, usage);
        }
        offset = next_offset;
    }
    Some(())
}

fn diag_attribute(message: &[u8], offset: usize) -> Option<(u16, &[u8], usize)> {
    let length = read_u16(message, offset)? as usize;
    let (payload, next_offset) = frame(message, offset, length, 4)?;
    Some((read_u16(message, offset + 2)?, payload, next_offset))
}

fn tcp_counters(payload: &[u8]) -> Option<NetworkCounters> {
    Some(NetworkCounters {
        transmitted_bytes: u64::from_ne_bytes(read_array(payload, TCP_INFO_BYTES_ACKED_OFFSET)?),
        received_bytes: u64::from_ne_bytes(read_array(payload, TCP_INFO_BYTES_RECEIVED_OFFSET)?),
    })
}

// Both netlink messages and attributes use length-prefixed, four-byte-aligned frames.
fn frame(bytes: &[u8], offset: usize, length: usize, header: usize) -> Option<(&[u8], usize)> {
    let end = offset.checked_add(length)?;
    let payload = bytes.get(offset.checked_add(header)?..end)?;
    let next = offset.checked_add(length.checked_add(3)? & !3)?;
    // An unpadded final frame is valid; a partial frame or inter-frame gap is not.
    (next <= bytes.len() || end == bytes.len()).then_some((payload, next))
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    bytes.get(offset..offset.checked_add(N)?)?.try_into().ok()
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    read_array(bytes, offset).map(u16::from_ne_bytes)
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    read_array(bytes, offset).map(u32::from_ne_bytes)
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
        DumpStatus, HashMap, HashSet, INET_DIAG_INFO, NLM_F_DUMP_INTR, NLMSG_DONE,
        SOCK_DIAG_BY_FAMILY, process_dump_messages, receive_packet, write_u16, write_u32,
    };

    fn message(kind: u16, flags: u16, sequence: u32, payload: &[u8]) -> Vec<u8> {
        let length = 16 + payload.len();
        let mut bytes = vec![0; (length + 3) & !3];
        write_u32(&mut bytes, 0, length as u32);
        write_u16(&mut bytes, 4, kind);
        write_u16(&mut bytes, 6, flags);
        write_u32(&mut bytes, 8, sequence);
        bytes[16..length].copy_from_slice(payload);
        bytes
    }

    #[test]
    fn dumps_reject_interruptions_truncation_and_malformed_frames() {
        let requested = HashSet::from([7]);
        let mut counters = HashMap::new();
        let mut diag = vec![0; 72 + 4 + 136];
        write_u32(&mut diag, 68, 7);
        write_u16(&mut diag, 72, 140);
        write_u16(&mut diag, 74, INET_DIAG_INFO);
        diag[76 + 120..76 + 128].copy_from_slice(&200_u64.to_ne_bytes());
        diag[76 + 128..].copy_from_slice(&100_u64.to_ne_bytes());
        let mut complete = message(SOCK_DIAG_BY_FAMILY, 0, 1, &diag);
        let observation_length = complete.len();
        let done = message(NLMSG_DONE, 0, 1, &0_i32.to_ne_bytes());
        complete.extend(&done);
        let observation = &complete[..observation_length];
        assert_eq!(
            process_dump_messages(&complete, 1, &requested, &mut counters),
            DumpStatus::Complete
        );
        assert_eq!(
            (counters[&7].received_bytes, counters[&7].transmitted_bytes),
            (100, 200)
        );
        for end in 0..observation.len() {
            assert_eq!(
                process_dump_messages(&observation[..end], 1, &requested, &mut counters),
                DumpStatus::Failed
            );
        }
        for packet in [
            message(SOCK_DIAG_BY_FAMILY, NLM_F_DUMP_INTR, 1, &diag),
            message(NLMSG_DONE, NLM_F_DUMP_INTR, 1, &[]),
            message(NLMSG_DONE, 0, 1, &(-5_i32).to_ne_bytes()),
            message(NLMSG_DONE, 0, 1, &[0]),
            message(2, 0, 1, &[]), // NLMSG_ERROR
            message(4, 0, 1, &[]), // NLMSG_OVERRUN
        ] {
            assert_eq!(
                process_dump_messages(&packet, 1, &requested, &mut counters),
                DumpStatus::Failed
            );
        }
        assert_eq!(
            process_dump_messages(&done, 2, &requested, &mut counters),
            DumpStatus::Pending
        );
        for length in [0, 2, u16::MAX] {
            write_u16(&mut diag, 72, length);
            let packet = message(SOCK_DIAG_BY_FAMILY, 0, 1, &diag);
            assert_eq!(
                process_dump_messages(&packet, 1, &requested, &mut counters),
                DumpStatus::Failed
            );
        }
        // Short but well-framed tcp_info is a supported older-kernel layout.
        diag.truncate(72 + 4 + 16);
        write_u16(&mut diag, 72, 20);
        counters.clear();
        let packet = message(SOCK_DIAG_BY_FAMILY, 0, 1, &diag);
        assert_eq!(
            process_dump_messages(&packet, 1, &requested, &mut counters),
            DumpStatus::Pending
        );
        assert!(counters.is_empty());
    }

    #[test]
    fn receiving_rejects_truncated_empty_and_expired_datagrams() {
        use rustix::net::{AddressFamily, SendFlags, SocketFlags, SocketType, send, socketpair};
        use std::time::{Duration, Instant};
        let (sender, receiver) = socketpair(
            AddressFamily::UNIX,
            SocketType::DGRAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let mut buffer = [0; 8];
        send(&sender, &[0; 32], SendFlags::empty()).unwrap();
        assert!(
            receive_packet(
                &receiver,
                &mut buffer,
                Instant::now() + Duration::from_secs(1)
            )
            .is_none()
        );
        send(&sender, &[], SendFlags::empty()).unwrap();
        assert!(
            receive_packet(
                &receiver,
                &mut buffer,
                Instant::now() + Duration::from_secs(1)
            )
            .is_none()
        );
        assert!(receive_packet(&receiver, &mut buffer, Instant::now()).is_none());
    }
}
