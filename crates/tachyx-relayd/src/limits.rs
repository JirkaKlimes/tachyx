//! What stops a machine strangers can reach from being a machine strangers use.
//!
//! Rate limiting is keyed on the IPv6 `/64` rather than the address, because a
//! single subscriber is handed a whole one and limiting per address would limit
//! nothing. An IPv4 address is its own key.
//!
//! The keyed limiter's own map is a denial of service unless something clears
//! it, so [`Limits::forget_idle`] is called on a timer.

use std::hash::Hash;
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;

use governor::{DefaultDirectRateLimiter, DefaultKeyedRateLimiter, Quota, RateLimiter};

/// New connections a prefix may open in a minute.
///
/// A person opens one for the host they run and one for each client. Thirty is
/// a household, and a household reconnecting after its link came back.
const CONNECTIONS_PER_MINUTE: u32 = 30;

/// How many of those may arrive at once.
const CONNECTION_BURST: u32 = 10;

/// Control messages one connection may send in a second.
///
/// A pairing is three flights, a rendezvous is two messages and a splice is one.
/// Ten a second is already far above anything the protocol does.
const MESSAGES_PER_SECOND: u32 = 10;

/// How many of those may arrive at once.
const MESSAGE_BURST: u32 = 20;

/// A source address as it is counted: the IPv6 `/64`, or the IPv4 address.
///
/// Written as sixteen bytes either way, so one map holds both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Prefix([u8; 16]);

impl Prefix {
    /// The prefix `address` is counted under.
    #[must_use]
    pub fn of(address: SocketAddr) -> Self {
        match address.ip().to_canonical() {
            // An IPv4 address is one subscriber, and nothing hands out an IPv4
            // network the way IPv6 hands out a /64.
            IpAddr::V4(four) => {
                let mut bytes = [0u8; 16];
                bytes[..4].copy_from_slice(&four.octets());
                bytes[15] = 4;
                Self(bytes)
            }
            IpAddr::V6(six) => {
                let mut bytes = six.octets();
                bytes[8..].fill(0);
                Self(bytes)
            }
        }
    }
}

/// A quota, built from figures that are known to be non-zero.
fn quota(per: u32, burst: u32, minutes: bool) -> Quota {
    let rate = NonZeroU32::new(per).expect("a rate limit is not zero");
    let burst = NonZeroU32::new(burst).expect("a burst allowance is not zero");
    if minutes {
        Quota::per_minute(rate).allow_burst(burst)
    } else {
        Quota::per_second(rate).allow_burst(burst)
    }
}

/// The relay's rate limits.
#[derive(Debug)]
pub struct Limits {
    connections: DefaultKeyedRateLimiter<Prefix>,
}

impl Default for Limits {
    fn default() -> Self {
        Self::new()
    }
}

impl Limits {
    /// The limits this build ships with.
    #[must_use]
    pub fn new() -> Self {
        Self {
            connections: RateLimiter::keyed(quota(CONNECTIONS_PER_MINUTE, CONNECTION_BURST, true)),
        }
    }

    /// Whether a connection from `address` is within its prefix's budget.
    #[must_use]
    pub fn admits(&self, address: SocketAddr) -> bool {
        self.connections.check_key(&Prefix::of(address)).is_ok()
    }

    /// Drops the prefixes that have not been seen for long enough to have their
    /// whole budget back.
    ///
    /// Without this the map grows one entry per source address forever, and the
    /// thing that was meant to stop a flood becomes the flood.
    pub fn forget_idle(&self) {
        self.connections.retain_recent();
    }

    /// How many prefixes are being counted, which is what says whether
    /// [`Limits::forget_idle`] is keeping up.
    #[must_use]
    pub fn watched(&self) -> usize {
        self.connections.len()
    }
}

/// One connection's own budget for control messages.
#[derive(Debug)]
pub struct Messages(DefaultDirectRateLimiter);

impl Default for Messages {
    fn default() -> Self {
        Self::new()
    }
}

impl Messages {
    /// A fresh budget, for one connection.
    #[must_use]
    pub fn new() -> Self {
        Self(RateLimiter::direct(quota(
            MESSAGES_PER_SECOND,
            MESSAGE_BURST,
            false,
        )))
    }

    /// Whether this connection may say one more thing.
    #[must_use]
    pub fn admits(&self) -> bool {
        self.0.check().is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(address: &str) -> SocketAddr {
        address.parse().expect("an address")
    }

    #[test]
    fn a_whole_ipv6_subscriber_is_counted_as_one() {
        assert_eq!(
            Prefix::of(at("[2001:db8:1:2::1]:47990")),
            Prefix::of(at("[2001:db8:1:2:ffff:ffff:ffff:ffff]:9")),
            "a /64 is handed to one subscriber, so it is one budget"
        );
        assert_ne!(
            Prefix::of(at("[2001:db8:1:2::1]:47990")),
            Prefix::of(at("[2001:db8:1:3::1]:47990")),
        );
    }

    #[test]
    fn an_ipv4_address_is_its_own_budget_however_it_is_written() {
        assert_eq!(
            Prefix::of(at("198.51.100.4:47990")),
            Prefix::of(at("[::ffff:198.51.100.4]:47990")),
            "the same address in two notations is one address"
        );
        assert_ne!(
            Prefix::of(at("198.51.100.4:47990")),
            Prefix::of(at("198.51.100.5:47990")),
        );
    }

    #[test]
    fn a_burst_of_connections_is_admitted_and_then_it_is_not() {
        let limits = Limits::new();
        let address = at("198.51.100.9:1");
        for _ in 0..CONNECTION_BURST {
            assert!(limits.admits(address));
        }
        assert!(
            !limits.admits(address),
            "past the burst the prefix waits for its budget to refill"
        );
        assert!(
            limits.admits(at("198.51.100.10:1")),
            "another subscriber is not held back by this one"
        );
    }

    #[test]
    fn a_connection_that_shouts_is_stopped_and_a_quiet_one_is_not() {
        let messages = Messages::new();
        for _ in 0..MESSAGE_BURST {
            assert!(messages.admits());
        }
        assert!(!messages.admits());
        assert!(Messages::new().admits(), "each connection has its own");
    }
}
