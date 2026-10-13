//! The guest's firewall: what a job may reach, as rules for the kernel's nftables.
//!
//! A job reaches the internet. It does not reach private networks: the Mac itself (the gateway of the machine's
//! network), other jobs' machines next to it, the Mac's local network, link-local addresses and Tailscale. The one
//! exception is DNS at the name servers the network gave. Nothing reaches the job that the job did not ask for.
//! IPv6 exists only on the loopback device.
//!
//! [`policy`] is the rules; `cf_nft::decide` says what they do with a packet and is what the tests hold against the
//! specification; [`netlink`] is the same rules for the kernel.

use cf_nft::{BLOCKED, Batch, Chain, Family, Hook, Ip, Load, Match, Rule, TCP, Table, UDP, Verdict, network};

/// The whole firewall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// For packets the guest sends. A packet no rule takes passes.
    pub output: Vec<Rule>,
    /// For packets that arrive. A packet no rule takes is dropped.
    pub input: Vec<Rule>,
}

/// The network a lease puts a machine in: its address under its mask, as `(network, prefix length)`. A mask with
/// a hole counts up to the hole, so the network is never smaller than the mask says.
pub fn own_network(address: Ip, netmask: Ip) -> (Ip, u8) {
    let prefix = u32::from_be_bytes(netmask).leading_ones() as u8;
    (network(address, prefix), prefix)
}

/// The most networks a host names for a job to be kept from; what comes after is not taken.
pub const MOST_TOLD: usize = 64;

/// What is next to a job's machine and the Mac it runs on: the network of the machine's lease, its gateway, and
/// the networks the host said the Mac is in (`told`, of which one with no prefix length an address can have is
/// left out). This is what [`policy`] takes as `own`.
pub fn beside(address: Ip, netmask: Ip, gateway: Option<Ip>, told: &[(Ip, u8)]) -> Vec<(Ip, u8)> {
    std::iter::once(own_network(address, netmask))
        .chain(gateway.map(|gateway| (gateway, 32)))
        .chain(
            told.iter()
                .filter(|(_, prefix)| *prefix <= 32)
                .take(MOST_TOLD)
                .map(|(at, prefix)| (network(*at, *prefix), *prefix)),
        )
        .collect()
}

/// The firewall of a job's machine whose network named these name servers. `own` is what the machine's lease
/// says is next to it, the network it is in and its gateway: the Mac and the other jobs' machines, whatever range
/// the Mac was set up to give them.
pub fn policy(name_servers: &[Ip], own: &[(Ip, u8)]) -> Policy {
    let rule = |when: &[Match], then| Rule {
        when: when.to_vec(),
        then,
    };
    let mut output = vec![
        rule(&[Match::OutLoopback], Verdict::Accept),
        rule(&[Match::Ipv6], Verdict::Drop),
    ];
    for server in name_servers {
        let at = Match::DestIn {
            network: *server,
            prefix: 32,
        };
        output.push(rule(
            &[at, Match::Protocol(UDP), Match::DestPort(53)],
            Verdict::Accept,
        ));
        output.push(rule(
            &[at, Match::Protocol(TCP), Match::DestPort(53)],
            Verdict::Accept,
        ));
    }
    for (network, prefix) in BLOCKED.iter().chain(own) {
        output.push(rule(
            &[Match::DestIn {
                network: *network,
                prefix: *prefix,
            }],
            Verdict::Drop,
        ));
    }
    Policy {
        output,
        input: vec![
            rule(&[Match::InLoopback], Verdict::Accept),
            rule(&[Match::Established], Verdict::Accept),
        ],
    }
}

/// The table's name in the kernel.
pub const TABLE: &str = "codefusion";

/// The firewall as one nftables transaction: a table for both IP families, a chain for what the guest sends and
/// one for what arrives. The guest's kernel holds no rules before it.
pub fn netlink(policy: &Policy) -> Result<Batch, cf_nft::Error> {
    let chain = |name: &str, hook, policy, rules: &[Rule]| Chain {
        name: name.into(),
        hook,
        policy,
        rules: rules.to_vec(),
    };
    let table = Table {
        family: Family::Inet,
        name: TABLE.into(),
        chains: vec![
            chain("output", Hook::Output, Verdict::Accept, &policy.output),
            chain("input", Hook::Input, Verdict::Drop, &policy.input),
        ],
    };
    cf_nft::batch(&[table], Load::Add)
}
