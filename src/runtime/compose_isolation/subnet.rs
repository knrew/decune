use std::{fmt, net::Ipv4Addr};

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ipv4Cidr {
    network: u32,
    broadcast: u32,
    prefix: u8,
}

impl Ipv4Cidr {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let (addr, prefix) = value.split_once('/')?;
        let addr = addr.parse::<Ipv4Addr>().ok()?;
        let prefix = prefix.parse::<u8>().ok()?;
        if prefix > 32 {
            return None;
        }
        let addr = u32::from(addr);
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - u32::from(prefix))
        };
        let network = addr & mask;
        let broadcast = network | !mask;
        Some(Self {
            network,
            broadcast,
            prefix,
        })
    }

    pub(crate) const fn prefix(self) -> u8 {
        self.prefix
    }

    pub(crate) const fn overlaps(self, other: Self) -> bool {
        self.network <= other.broadcast && other.network <= self.broadcast
    }

    pub(crate) const fn contains(self, other: Self) -> bool {
        self.network <= other.network && other.broadcast <= self.broadcast
    }

    pub(crate) const fn network(self) -> u32 {
        self.network
    }

    pub(crate) const fn contains_address(self, address: Ipv4Addr) -> bool {
        let address = u32::from_be_bytes(address.octets());
        self.network <= address && address <= self.broadcast
    }

    pub(crate) fn address_at_offset(self, offset: u32) -> Option<Ipv4Addr> {
        let address = self.network.checked_add(offset)?;
        (address <= self.broadcast).then_some(Ipv4Addr::from(address))
    }
}

impl fmt::Display for Ipv4Cidr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}/{}",
            Ipv4Addr::from(self.network),
            self.prefix
        )
    }
}

const SUBNET_HASH_VERSION: &str = "decune-clone-isolation-subnet-v1";

pub(crate) fn allocate_ipv4_subnet_slot(
    pool: Ipv4Cidr,
    subnet_prefix: u8,
    workspace_id: &str,
    compose_network: &str,
    unavailable: &[Ipv4Cidr],
) -> Option<Ipv4Cidr> {
    if subnet_prefix < pool.prefix || subnet_prefix > 32 {
        return None;
    }
    // Valid CIDR prefixes and the normalized pool keep these calculations within IPv4 space;
    // checked arithmetic keeps the allocator total if those invariants change.
    let prefix_difference = u32::from(subnet_prefix - pool.prefix);
    let slot_count = 1_u64.checked_shl(prefix_difference)?;
    let slot_size = 1_u64.checked_shl(32 - u32::from(subnet_prefix))?;
    let input = format!("{SUBNET_HASH_VERSION}:{workspace_id}:{compose_network}");
    let digest = Sha256::digest(input.as_bytes());
    let initial_slot = u64::from_be_bytes([
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
    ]) % slot_count;
    let occupied = occupied_slot_intervals(pool, slot_size, unavailable);
    let slot = first_free_slot(&occupied, initial_slot, slot_count)
        .or_else(|| first_free_slot(&occupied, 0, initial_slot))?;
    let network = u64::from(pool.network).checked_add(slot.checked_mul(slot_size)?)?;
    let network = u32::try_from(network).ok()?;
    let broadcast = network.checked_add(u32::try_from(slot_size - 1).ok()?)?;
    Some(Ipv4Cidr {
        network,
        broadcast,
        prefix: subnet_prefix,
    })
}

fn occupied_slot_intervals(
    pool: Ipv4Cidr,
    slot_size: u64,
    unavailable: &[Ipv4Cidr],
) -> Vec<(u64, u64)> {
    let pool_network = u64::from(pool.network);
    let mut intervals = unavailable
        .iter()
        .filter(|existing| pool.overlaps(**existing))
        .map(|existing| {
            let overlap_start = u64::from(pool.network.max(existing.network));
            let overlap_end = u64::from(pool.broadcast.min(existing.broadcast));
            (
                (overlap_start - pool_network) / slot_size,
                (overlap_end - pool_network) / slot_size,
            )
        })
        .collect::<Vec<_>>();
    intervals.sort_unstable_by_key(|interval| interval.0);

    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(intervals.len());
    for interval in intervals {
        if let Some(previous) = merged.last_mut()
            && interval.0 <= previous.1 + 1
        {
            previous.1 = previous.1.max(interval.1);
        } else {
            merged.push(interval);
        }
    }
    merged
}

fn first_free_slot(intervals: &[(u64, u64)], start: u64, end: u64) -> Option<u64> {
    if start >= end {
        return None;
    }
    let mut candidate = start;
    for &(occupied_start, occupied_end) in intervals {
        if occupied_end < candidate {
            continue;
        }
        if occupied_start >= end {
            break;
        }
        if occupied_start > candidate {
            return Some(candidate);
        }
        candidate = occupied_end.checked_add(1)?;
        if candidate >= end {
            return None;
        }
    }
    Some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipv4_cidr_and_rejects_ipv6_or_invalid_prefix() {
        assert!(Ipv4Cidr::parse("172.20.0.0/16").is_some());
        assert!(Ipv4Cidr::parse("::1/128").is_none());
        assert!(Ipv4Cidr::parse("172.20.0.0/33").is_none());
        assert!(Ipv4Cidr::parse("172.20.0.0").is_none());
    }

    #[test]
    fn detects_equal_contained_and_adjacent_subnets() {
        let base = Ipv4Cidr::parse("172.20.0.0/16").unwrap();
        assert!(base.overlaps(Ipv4Cidr::parse("172.20.0.0/16").unwrap()));
        assert!(base.overlaps(Ipv4Cidr::parse("172.20.10.0/24").unwrap()));
        assert!(!base.overlaps(Ipv4Cidr::parse("172.21.0.0/16").unwrap()));
        assert!(
            !Ipv4Cidr::parse("10.0.0.0/25")
                .unwrap()
                .overlaps(Ipv4Cidr::parse("10.0.0.128/25").unwrap())
        );
    }

    #[test]
    fn slot_allocation_is_deterministic_and_skips_unavailable_subnets() {
        let pool = Ipv4Cidr::parse("10.200.0.0/16").unwrap();
        let first = allocate_ipv4_subnet_slot(pool, 24, "workspace-a", "grpc", &[]).unwrap();
        assert_eq!(
            allocate_ipv4_subnet_slot(pool, 24, "workspace-a", "grpc", &[]),
            Some(first)
        );

        let next = allocate_ipv4_subnet_slot(pool, 24, "workspace-a", "grpc", &[first]).unwrap();
        assert_ne!(next, first);
        assert!(pool.contains(next));
    }

    #[test]
    fn slot_allocation_reports_pool_exhaustion() {
        let pool = Ipv4Cidr::parse("10.200.0.0/30").unwrap();
        let occupied = [
            Ipv4Cidr::parse("10.200.0.0/31").unwrap(),
            Ipv4Cidr::parse("10.200.0.2/31").unwrap(),
        ];

        assert_eq!(
            allocate_ipv4_subnet_slot(pool, 31, "workspace-a", "grpc", &occupied),
            None
        );
    }

    #[test]
    fn slot_allocation_skips_large_occupied_interval_and_wraps() {
        let pool = Ipv4Cidr::parse("0.0.0.0/0").unwrap();
        let occupied = [Ipv4Cidr::parse("128.0.0.0/1").unwrap()];

        assert_eq!(
            allocate_ipv4_subnet_slot(pool, 32, "workspace-a", "grpc", &occupied),
            Ipv4Cidr::parse("0.0.0.0/32")
        );
    }

    #[test]
    fn slot_allocation_reports_large_pool_exhaustion_without_scanning_slots() {
        let pool = Ipv4Cidr::parse("0.0.0.0/0").unwrap();

        assert_eq!(
            allocate_ipv4_subnet_slot(pool, 32, "workspace-a", "grpc", &[pool]),
            None
        );
    }
}
