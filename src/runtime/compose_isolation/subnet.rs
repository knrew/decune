use std::net::Ipv4Addr;

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
}
