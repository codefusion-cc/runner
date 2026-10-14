//! The supervisor's decisions against what they must do and the inputs that break them.

use cf_guest_proto::{Limits, OomEvent, Process as Used, StopReason};
use cf_nft::{BLOCKED, IPV4, IPV6, Packet, Verdict, decide};
use cf_supervisor::dhcp::{self, Lease, Reply};
use cf_supervisor::environment::{find_group, find_user, workload_environment};
use cf_supervisor::firewall::{self, own_network, policy};
use cf_supervisor::oom::{Count, Kill, Log, kills_in_events, message_of};
use cf_supervisor::processes::{Process, Table, parse_stat};
use cf_supervisor::tell::{
    JobFacts, docker_missing, out_of_memory_line, out_of_memory_summary, size, started, stopped,
};
use proptest::prelude::*;

const GATEWAY: [u8; 4] = [192, 168, 64, 1];

fn out(dest: [u8; 4], protocol: u8, dest_port: u16) -> Verdict {
    let packet = Packet {
        ether_type: IPV4,
        in_interface: None,
        loopback: false,
        dest,
        protocol,
        dest_port,
        established: false,
    };
    decide(&policy(&[GATEWAY], &[]).output, Verdict::Accept, &packet)
}

#[test]
fn a_job_reaches_the_internet() {
    for dest in [
        [140, 82, 121, 4],
        [1, 1, 1, 1],
        [104, 16, 0, 1],
        [172, 15, 255, 255],
        [172, 32, 0, 1],
        [100, 63, 255, 255],
        [100, 128, 0, 1],
        [192, 167, 255, 255],
        [192, 169, 0, 1],
        [11, 0, 0, 1],
        [9, 255, 255, 255],
        [169, 253, 255, 255],
        [169, 255, 0, 1],
        [223, 255, 255, 255],
    ] {
        for (protocol, port) in [(6, 443), (6, 22), (17, 443), (1, 0)] {
            assert_eq!(
                out(dest, protocol, port),
                Verdict::Accept,
                "{dest:?} {protocol} {port}"
            );
        }
    }
}

#[test]
fn a_job_reaches_the_gateway_for_dns_and_for_nothing_else() {
    assert_eq!(out(GATEWAY, 17, 53), Verdict::Accept);
    assert_eq!(out(GATEWAY, 6, 53), Verdict::Accept);
    for (protocol, port) in [
        (6, 22),
        (6, 80),
        (6, 443),
        (17, 5353),
        (17, 67),
        (6, 54),
        (17, 52),
        (1, 53),
        (132, 53),
    ] {
        assert_eq!(out(GATEWAY, protocol, port), Verdict::Drop, "{protocol} {port}");
    }
}

#[test]
fn a_job_reaches_no_private_network() {
    for dest in [
        [192, 168, 64, 3],   // another job's machine on the same bridge
        [192, 168, 64, 255], // that network's broadcast
        [192, 168, 1, 10],   // the Mac's local network
        [10, 0, 0, 1],
        [10, 255, 255, 255],
        [172, 16, 0, 1],
        [172, 31, 255, 255],
        [169, 254, 169, 254], // link-local
        [100, 64, 0, 1],
        [100, 100, 100, 100],
        [100, 127, 255, 255], // Tailscale
        [224, 0, 0, 251],     // mDNS
        [239, 255, 255, 250],
        [255, 255, 255, 255],
    ] {
        for (protocol, port) in [(6, 22), (6, 443), (17, 53), (6, 53), (1, 0)] {
            assert_eq!(
                out(dest, protocol, port),
                Verdict::Drop,
                "{dest:?} {protocol} {port}"
            );
        }
    }
}

#[test]
fn ipv6_leaves_only_through_loopback() {
    let rules = policy(&[GATEWAY], &[]);
    let mut packet = Packet {
        ether_type: IPV6,
        in_interface: None,
        loopback: false,
        dest: [0; 4],
        protocol: 6,
        dest_port: 443,
        established: false,
    };
    assert_eq!(decide(&rules.output, Verdict::Accept, &packet), Verdict::Drop);
    packet.loopback = true;
    assert_eq!(decide(&rules.output, Verdict::Accept, &packet), Verdict::Accept);
}

#[test]
fn loopback_carries_everything_even_to_the_guests_own_private_address() {
    let packet = Packet {
        ether_type: IPV4,
        in_interface: None,
        loopback: true,
        dest: [192, 168, 64, 7],
        protocol: 6,
        dest_port: 8080,
        established: false,
    };
    let rules = policy(&[GATEWAY], &[]);
    assert_eq!(decide(&rules.output, Verdict::Accept, &packet), Verdict::Accept);
    assert_eq!(decide(&rules.input, Verdict::Drop, &packet), Verdict::Accept);
}

#[test]
fn nothing_new_comes_in_and_answers_do() {
    let rules = policy(&[GATEWAY], &[]);
    let mut packet = Packet {
        ether_type: IPV4,
        in_interface: None,
        loopback: false,
        dest: [192, 168, 64, 7],
        protocol: 6,
        dest_port: 22,
        established: false,
    };
    assert_eq!(decide(&rules.input, Verdict::Drop, &packet), Verdict::Drop);
    packet.ether_type = IPV6;
    assert_eq!(decide(&rules.input, Verdict::Drop, &packet), Verdict::Drop);
    packet.established = true;
    assert_eq!(decide(&rules.input, Verdict::Drop, &packet), Verdict::Accept);
}

#[test]
fn without_a_name_server_no_private_address_is_reachable_at_all() {
    let rules = policy(&[], &[]);
    let packet = Packet {
        ether_type: IPV4,
        in_interface: None,
        loopback: false,
        dest: GATEWAY,
        protocol: 17,
        dest_port: 53,
        established: false,
    };
    assert_eq!(decide(&rules.output, Verdict::Accept, &packet), Verdict::Drop);
}

proptest! {
    /// Whatever the packet, a blocked destination is reached for DNS at a name server or over loopback only.
    #[test]
    fn a_blocked_destination_is_never_reached_but_for_dns(
        range in 0..BLOCKED.len(), host in any::<u32>(), protocol in any::<u8>(), port in any::<u16>(),
        servers in prop::collection::vec(any::<[u8; 4]>(), 0..3),
    ) {
        let (network, prefix) = BLOCKED[range];
        let host_bits = if prefix == 32 { 0 } else { host >> prefix };
        let dest = (u32::from_be_bytes(network) | host_bits).to_be_bytes();
        let packet = Packet { ether_type: IPV4, in_interface: None, loopback: false, dest, protocol, dest_port: port, established: false };
        let verdict = decide(&policy(&servers, &[]).output, Verdict::Accept, &packet);
        let dns = servers.contains(&dest) && port == 53 && (protocol == 6 || protocol == 17);
        prop_assert_eq!(verdict, if dns { Verdict::Accept } else { Verdict::Drop });
    }
}

/// Walks netlink attributes; panics when one runs past its container.
fn attributes(mut bytes: &[u8], depth: usize, names: &mut Vec<String>) {
    while !bytes.is_empty() {
        let length = u16::from_ne_bytes([bytes[0], bytes[1]]) as usize;
        let kind = u16::from_ne_bytes([bytes[2], bytes[3]]);
        assert!(
            length >= 4 && length <= bytes.len(),
            "an attribute of {length} bytes in {} left",
            bytes.len()
        );
        let payload = &bytes[4..length];
        if kind & 0x8000 != 0 {
            attributes(payload, depth + 1, names);
        } else if depth == 2 && kind == 1 && payload.last() == Some(&0) {
            // NFTA_EXPR_NAME inside a list element inside the rule's expressions.
            names.push(String::from_utf8_lossy(&payload[..payload.len() - 1]).into_owned());
        }
        bytes = &bytes[length.next_multiple_of(4).min(bytes.len())..];
    }
}

#[test]
fn the_netlink_form_is_one_well_formed_transaction_with_every_rule() {
    let rules = policy(&[GATEWAY], &[]);
    let batch = firewall::netlink(&rules).unwrap();
    let mut bytes = batch.bytes.as_slice();
    let (mut kinds, mut sequences, mut expression_names) = (Vec::new(), Vec::new(), Vec::new());
    while !bytes.is_empty() {
        let length = u32::from_ne_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert!(length >= 20 && length <= bytes.len() && bytes.len().is_multiple_of(4));
        kinds.push(u16::from_ne_bytes(bytes[4..6].try_into().unwrap()));
        sequences.push(u32::from_ne_bytes(bytes[8..12].try_into().unwrap()));
        let mut names = Vec::new();
        attributes(&bytes[20..length], 0, &mut names);
        expression_names.push(names);
        bytes = &bytes[length.next_multiple_of(4)..];
    }
    let rule_count = rules.output.len() + rules.input.len();
    // Begin, the table, two chains, the rules, end.
    assert_eq!(kinds.len(), 1 + 1 + 2 + rule_count + 1);
    assert_eq!((kinds[0], *kinds.last().unwrap()), (0x10, 0x11));
    assert_eq!(kinds[1], 0x0a00);
    assert_eq!(kinds.iter().filter(|kind| **kind == 0x0a03).count(), 2);
    assert_eq!(kinds.iter().filter(|kind| **kind == 0x0a06).count(), rule_count);
    assert_eq!(batch.acknowledged as usize, 1 + 2 + rule_count);
    assert!(
        sequences.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "{sequences:?}"
    );
    // Every rule ends in its verdict, and the rule for answers reads the connection's state.
    let rule_names: Vec<&Vec<String>> = expression_names
        .iter()
        .filter(|names| !names.is_empty())
        .collect();
    assert_eq!(rule_names.len(), rule_count);
    assert!(
        rule_names
            .iter()
            .all(|names| names.last().map(String::as_str) == Some("immediate"))
    );
    assert_eq!(
        rule_names.last().unwrap().as_slice(),
        ["ct", "bitwise", "cmp", "immediate"]
    );
    assert_eq!(rule_names[0].as_slice(), ["meta", "cmp", "immediate"]);
}

/// The guest's firewall is the same bytes it was before its encoding moved to cf-nft: `guest-firewall.hex` is
/// what the supervisor's own encoder wrote for this policy. Netlink headers are in the host's byte order, so the
/// file holds for little-endian hosts (the guest's and every host the tests run on).
#[test]
#[cfg(target_endian = "little")]
fn the_guests_firewall_is_the_bytes_it_always_was() {
    use cf_supervisor::firewall::beside;
    let own = beside(
        [192, 168, 64, 9],
        [255, 255, 255, 0],
        Some(GATEWAY),
        &[([145, 97, 20, 0], 22), ([25, 3, 2, 1], 32)],
    );
    let batch = firewall::netlink(&policy(&[GATEWAY, [1, 1, 1, 1]], &own)).unwrap();
    let hex: String = batch.bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let recorded: String = include_str!("guest-firewall.hex").split_whitespace().collect();
    assert_eq!(hex, recorded);
    assert_eq!(batch.acknowledged, 25);
}

const MAC: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];

/// A server's packet for the exchange, with these options.
fn server_packet(xid: u32, mac: [u8; 6], address: [u8; 4], options: &[u8]) -> Vec<u8> {
    let mut packet = vec![0u8; 240];
    packet[0] = 2;
    packet[4..8].copy_from_slice(&xid.to_be_bytes());
    packet[16..20].copy_from_slice(&address);
    packet[28..34].copy_from_slice(&mac);
    packet[236..240].copy_from_slice(&[99, 130, 83, 99]);
    packet.extend_from_slice(options);
    packet
}

const OFFER: &[u8] = &[
    53, 1, 2, 54, 4, 192, 168, 64, 1, 51, 4, 0, 1, 81, 128, 1, 4, 255, 255, 255, 0, 3, 4, 192, 168, 64, 1, 6,
    4, 192, 168, 64, 1, 255,
];

#[test]
fn an_offer_is_read_with_its_mask_gateway_and_name_servers() {
    let lease = Lease {
        address: [192, 168, 64, 9],
        netmask: [255, 255, 255, 0],
        gateway: Some(GATEWAY),
        dns: vec![GATEWAY],
        server: GATEWAY,
    };
    assert_eq!(
        dhcp::parse(&server_packet(7, MAC, [192, 168, 64, 9], OFFER), 7, MAC),
        Some(Reply::Offer(lease.clone()))
    );
    let mut ack = OFFER.to_vec();
    ack[2] = 5;
    assert_eq!(
        dhcp::parse(&server_packet(7, MAC, [192, 168, 64, 9], &ack), 7, MAC),
        Some(Reply::Ack(lease))
    );
    assert_eq!(
        dhcp::parse(&server_packet(7, MAC, [0; 4], &[53, 1, 6, 255]), 7, MAC),
        Some(Reply::Nak)
    );
}

#[test]
fn a_lease_gives_a_job_three_name_servers_at_most() {
    // An answer with 63 servers, as many as one option holds: each would be an address the job may reach.
    let mut options = vec![53, 1, 2, 54, 4, 192, 168, 64, 1, 1, 4, 255, 255, 255, 0, 6, 252];
    options.extend((1..=63u8).flat_map(|n| [10, 0, 0, n]));
    options.push(255);
    let Some(Reply::Offer(lease)) = dhcp::parse(&server_packet(7, MAC, [192, 168, 64, 9], &options), 7, MAC)
    else {
        panic!("the offer was not read");
    };
    assert_eq!(lease.dns, [[10, 0, 0, 1], [10, 0, 0, 2], [10, 0, 0, 3]]);
    assert_eq!(dhcp::NAME_SERVERS, 3);
}

#[test]
fn an_answer_for_someone_else_or_a_broken_one_is_not_ours() {
    let good = server_packet(7, MAC, [192, 168, 64, 9], OFFER);
    assert!(dhcp::parse(&good, 8, MAC).is_none(), "another exchange");
    assert!(
        dhcp::parse(&good, 7, [2, 0, 0, 0, 0, 1]).is_none(),
        "another device"
    );
    let mut request = good.clone();
    request[0] = 1;
    assert!(
        dhcp::parse(&request, 7, MAC).is_none(),
        "a request, not an answer"
    );
    let mut cookie = good.clone();
    cookie[236] = 0;
    assert!(dhcp::parse(&cookie, 7, MAC).is_none(), "no DHCP cookie");
    for cut in 0..good.len() - 1 {
        // Cut anywhere: never a panic, and never a lease without its mask and server.
        if let Some(Reply::Offer(lease)) = dhcp::parse(&good[..cut], 7, MAC) {
            assert_eq!(lease.netmask, [255, 255, 255, 0], "cut at {cut}");
        }
    }
    // An option whose length runs past the packet.
    assert!(
        dhcp::parse(
            &server_packet(7, MAC, [1, 2, 3, 4], &[53, 1, 2, 1, 200, 255]),
            7,
            MAC
        )
        .is_none()
    );
    // No mask, no lease.
    assert!(
        dhcp::parse(
            &server_packet(7, MAC, [1, 2, 3, 4], &[53, 1, 2, 54, 4, 1, 1, 1, 1, 255]),
            7,
            MAC
        )
        .is_none()
    );
}

#[test]
fn the_packets_we_send_name_the_exchange_the_device_and_the_offer() {
    let discover = dhcp::discover(0xdead_beef, MAC);
    assert!(discover.len() >= 300);
    assert_eq!((discover[0], discover[1], discover[2]), (1, 1, 6));
    assert_eq!(discover[4..8], [0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(
        discover[10], 0x80,
        "answers must be broadcast: the guest has no address yet"
    );
    assert_eq!(discover[28..34], MAC);
    assert_eq!(discover[240..243], [53, 1, 1]);
    let lease = Lease {
        address: [192, 168, 64, 9],
        netmask: [255, 255, 255, 0],
        gateway: None,
        dns: vec![],
        server: GATEWAY,
    };
    let request = dhcp::request(1, MAC, &lease);
    assert_eq!(
        request[240..255],
        [53, 1, 3, 50, 4, 192, 168, 64, 9, 54, 4, 192, 168, 64, 1]
    );
}

proptest! {
    #[test]
    fn arbitrary_packets_never_panic_the_dhcp_reader(bytes in prop::collection::vec(any::<u8>(), 0..400), options in prop::collection::vec(any::<u8>(), 0..80)) {
        let _ = dhcp::parse(&bytes, 7, MAC);
        let _ = dhcp::parse(&server_packet(7, MAC, [1, 2, 3, 4], &options), 7, MAC);
    }

    #[test]
    fn arbitrary_kernel_lines_never_panic_the_oom_reader(lines in prop::collection::vec(".{0,120}", 0..20)) {
        let mut log = Log::new(4096);
        for line in &lines {
            let _ = message_of(line);
            let _ = log.line(line);
        }
    }

    #[test]
    fn arbitrary_stat_lines_never_panic(text in ".{0,200}") {
        let _ = parse_stat(&text);
    }
}

/// The kernel's report for a memory control group, as Linux 6.18 prints it.
const REPORT: &str = "\
node invoked oom-killer: gfp_mask=0xcc0(GFP_KERNEL), order=0, oom_score_adj=0
memory: usage 786432kB, limit 786432kB, failcnt 153
Memory cgroup stats for /job:
Tasks state (memory values in pages):
[  pid  ]   uid  tgid total_vm      rss rss_anon rss_file rss_shmem pgtables_bytes swapents oom_score_adj name
[    212]     0   212      609      288       32      256         0    45056        0             0 supervisor
[    215]  1001   215     1100      420      100      320         0    49152        0             0 run.sh
[    230]  1001   230   250000    20000    19000     1000         0   400000        0             0 Runner.Worker
[    260]  1001   260   400000   150000   149000     1000         0  1400000        0             0 node
[    261]  1001   261   300000    30000    29000     1000         0   500000        0             0 Web Content
[    262]  1001   262    90000     9000     8000     1000         0   200000        0             0 esbuild
[    263]  1001   263     1000      100       50       50         0    40000        0             0 sleep
oom-kill:constraint=CONSTRAINT_MEMCG,nodemask=(null),cpuset=/,mems_allowed=0,oom_memcg=/job,task_memcg=/job,task=node,pid=260,uid=1001
Memory cgroup out of memory: Killed process 260 (node) total-vm:1600000kB, anon-rss:596000kB, file-rss:4000kB, shmem-rss:0kB, UID:1001 pgtables:1367kB oom_score_adj:0";

fn feed(log: &mut Log, text: &str) -> Vec<Kill> {
    text.lines().filter_map(|line| log.line(line)).collect()
}

#[test]
fn the_kernels_report_names_the_stopped_process_and_the_five_largest() {
    let kills = feed(&mut Log::new(4096), REPORT);
    assert_eq!(kills.len(), 1);
    assert_eq!(
        kills[0].process,
        Used {
            pid: 260,
            name: "node".into(),
            memory_bytes: 150_000 * 4096
        }
    );
    let largest: Vec<(&str, u64)> = kills[0]
        .largest
        .iter()
        .map(|p| (p.name.as_str(), p.memory_bytes / 4096))
        .collect();
    assert_eq!(
        largest,
        [
            ("node", 150_000),
            ("Web Content", 30_000),
            ("Runner.Worker", 20_000),
            ("esbuild", 9_000),
            ("run.sh", 420)
        ]
    );
}

#[test]
fn a_second_stop_has_its_own_table_and_a_stop_without_one_still_names_the_process() {
    let mut log = Log::new(4096);
    assert_eq!(feed(&mut log, REPORT).len(), 1);
    // The table of the first report must not leak into a later stop that printed none.
    let later = log
        .line(
            "Memory cgroup out of memory: Killed process 300 (python3 (deleted)) total-vm:1kB, anon-rss:1kB",
        )
        .unwrap();
    assert_eq!(
        later.process,
        Used {
            pid: 300,
            name: "python3 (deleted)".into(),
            memory_bytes: 0
        }
    );
    assert!(later.largest.is_empty());
    assert!(
        log.line("Out of memory: Killed process 9 (init) total-vm:1kB")
            .is_some(),
        "a stop for the whole machine counts"
    );
    assert!(
        log.line("Out of memory (oom_kill_allocating_task): Killed process 9 (init) total-vm:1kB")
            .is_some()
    );
    assert!(log.line("usb 1-1: Killed process 9 (x)").is_none());
    // The kernel's words in the middle of a line are a job's: it quotes a mount option it refuses, a name a
    // process gave itself.
    for forged in [
        "tmpfs: Unknown parameter 'Memory cgroup out of memory: Killed process 1 (init) total-vm:1kB'",
        "x Memory cgroup out of memory: Killed process 1 (init)",
        "Out of memory: [77]: segfault at 0 Killed process 1 (init)",
        "out of memory: Killed process 1 (init)",
        "Memory cgroup out of memory: killed process 1 (init)",
    ] {
        assert!(log.line(forged).is_none(), "{forged}");
    }
    assert!(
        log.line("Memory cgroup out of memory: Killed process x (y)")
            .is_none()
    );
}

#[test]
fn a_kernel_log_record_is_its_message_and_continuations_are_none() {
    assert_eq!(
        message_of("6,812,5123456,-;Memory cgroup out of memory: Killed process 1 (a)\n SUBSYSTEM=x\n"),
        Some("Memory cgroup out of memory: Killed process 1 (a)")
    );
    assert_eq!(message_of(" DEVICE=+pci:0000"), None);
    assert_eq!(message_of("no semicolon"), None);
    assert_eq!(message_of("6,1,2,-;"), Some(""));
    // What a process wrote into the log has a facility the kernel's own lines never have, whatever it says.
    assert_eq!(
        message_of("12,813,5123999,-;Memory cgroup out of memory: Killed process 1 (a)\n"),
        None
    );
    assert_eq!(message_of("8,1,2,-;x"), None);
    assert_eq!(message_of("7,1,2,-;x"), Some("x"));
    assert_eq!(message_of("x,1,2,-;x"), None);
    assert_eq!(message_of(";x"), None);
}

#[test]
fn how_a_workload_ended_is_the_launchers_report_and_never_a_success_without_one() {
    use cf_supervisor::outcome::{Outcome, workload_end};
    let report = |outcome: &Outcome| serde_json::to_string(outcome).unwrap();
    let launcher_ok = (Some(0), None);
    // A wait status: exit codes 0, 7 and 255, the signals 9 and 15, a core dump's flag.
    assert_eq!(
        workload_end(&report(&Outcome::Status(0)), launcher_ok),
        Ok((Some(0), None))
    );
    assert_eq!(
        workload_end(&report(&Outcome::Status(7 << 8)), launcher_ok),
        Ok((Some(7), None))
    );
    assert_eq!(
        workload_end(&report(&Outcome::Status(255 << 8)), launcher_ok),
        Ok((Some(255), None))
    );
    assert_eq!(
        workload_end(&report(&Outcome::Status(9)), launcher_ok),
        Ok((None, Some(9)))
    );
    assert_eq!(
        workload_end(&report(&Outcome::Status(15)), launcher_ok),
        Ok((None, Some(15)))
    );
    assert_eq!(
        workload_end(&report(&Outcome::Status(0x80 | 11)), launcher_ok),
        Ok((None, Some(11)))
    );
    assert_eq!(
        workload_end(&report(&Outcome::Error("no root".into())), launcher_ok),
        Err("no root".to_string())
    );

    // No report: the container went before it could say. The launcher's own end is what there is.
    assert_eq!(workload_end("", (Some(137), None)), Ok((Some(137), None)));
    assert_eq!(workload_end("", (None, Some(9))), Ok((None, Some(9))));
    // But a launcher that ended with 0 and left no report, or one something else wrote into, is no workload
    // that ended well.
    let real = report(&Outcome::Status(3 << 8));
    for forged in [
        String::new(),
        "{".to_string(),
        "null".to_string(),
        format!("{}{real}", report(&Outcome::Status(0))),
        format!("{real}{}", report(&Outcome::Status(0))),
        format!("garbage{real}"),
    ] {
        assert_eq!(workload_end(&forged, launcher_ok), Ok((None, None)), "{forged:?}");
    }
}

#[test]
fn a_look_at_the_kernel_log_ends_at_its_end_and_never_reads_without_end() {
    use cf_supervisor::oom::{RECORDS_A_LOOK, read_records};
    use std::io::{Error, ErrorKind, Read};
    /// A log as `/dev/kmsg` gives it: a record a read, then whatever `then` says.
    struct Records(Vec<Result<&'static str, ErrorKind>>, usize, Option<&'static str>);
    impl Read for Records {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.1 += 1;
            assert!(
                self.1 <= RECORDS_A_LOOK,
                "the look goes on after {RECORDS_A_LOOK} records"
            );
            let next = if self.0.is_empty() {
                self.2.ok_or(ErrorKind::WouldBlock)
            } else {
                self.0.remove(0)
            };
            let record = next.map_err(Error::from)?;
            buffer[..record.len()].copy_from_slice(record.as_bytes());
            Ok(record.len())
        }
    }
    let read = |log: &mut Records| {
        let mut messages = Vec::new();
        read_records(log, |message| messages.push(message.to_string()));
        messages
    };

    // To the log's end, past the records it dropped, without those a process wrote or a continuation.
    let mut log = Records(
        vec![
            Ok("6,1,1,-;one\n"),
            Err(ErrorKind::BrokenPipe),
            Ok("12,2,2,-;a process wrote this\n"),
            Ok(" SUBSYSTEM=x\n"),
            Ok("4,3,3,-;two\n KEY=v\n"),
        ],
        0,
        None,
    );
    assert_eq!(read(&mut log), ["one", "two"]);
    assert_eq!(log.1, 6, "five reads and the one that found the end");

    // Another error ends the look; what came before it counts.
    let mut log = Records(
        vec![
            Ok("6,1,1,-;one\n"),
            Err(ErrorKind::InvalidInput),
            Ok("6,2,2,-;two\n"),
        ],
        0,
        None,
    );
    assert_eq!(read(&mut log), ["one"]);

    // A log that never ends: the look does.
    let mut log = Records(Vec::new(), 0, Some("6,1,1,-;again\n"));
    assert_eq!(read(&mut log).len(), RECORDS_A_LOOK);
    assert_eq!(log.1, RECORDS_A_LOOK);
}

#[test]
fn memory_events_give_the_number_of_stops() {
    assert_eq!(
        kills_in_events("low 0\nhigh 0\nmax 153\noom 3\noom_kill 2\noom_group_kill 0\n"),
        2
    );
    assert_eq!(kills_in_events("oom_group_kill 4\n"), 0);
    assert_eq!(kills_in_events(""), 0);
    assert_eq!(kills_in_events("oom_kill many\n"), 0);
}

fn process(pid: u32, parent: u32, name: &str) -> Process {
    Process {
        pid,
        parent,
        name: name.into(),
        started: u64::from(pid) * 10,
    }
}

/// The supervisor (1), the launcher (10), the container's first process (11), then the Actions runner and a job.
fn actions_runner_job() -> Table {
    Table::new([
        process(1, 0, "supervisor"),
        process(10, 1, "supervisor"),
        process(11, 10, "supervisor"),
        process(12, 11, "run.sh"),
        process(13, 12, "run-helper.sh"),
        process(14, 13, "Runner.Listener"),
        process(15, 14, "Runner.Worker"),
        process(20, 15, "bash"),
        process(21, 20, "npm test"),
        process(22, 21, "node"),
        process(23, 22, "node"),
        process(24, 22, "node"),
        process(30, 15, "bash"),
        process(31, 30, "sleep"),
        process(40, 1, "other"),
    ])
}

#[test]
fn the_step_of_a_process_is_its_top_ancestor_below_the_actions_runner() {
    let table = actions_runner_job();
    for victim in [20, 21, 22, 23, 24] {
        assert_eq!(
            table.step_of(victim, 10).map(|step| step.pid),
            Some(20),
            "{victim}"
        );
    }
    assert_eq!(table.step_of(31, 10).map(|step| step.pid), Some(30));
    // The Actions runner's own processes and the container's are nobody's step.
    for own in [10, 11, 12, 13, 14, 15] {
        assert_eq!(table.step_of(own, 10), None, "{own}");
    }
    // A process outside the container, and one that is not in the table.
    assert_eq!(table.step_of(40, 10), None);
    assert_eq!(table.step_of(999, 10), None);
}

#[test]
fn a_plain_command_has_its_children_as_steps() {
    let table = Table::new([
        process(1, 0, "supervisor"),
        process(10, 1, "supervisor"),
        process(11, 10, "supervisor"),
        process(12, 11, "bash"),
        process(13, 12, "tail"),
        process(14, 12, "head"),
    ]);
    assert_eq!(table.step_of(13, 10).map(|step| step.pid), Some(13));
    assert_eq!(table.step_of(12, 10), None, "the workload itself is not a step");
    assert_eq!(table.workload(10).map(|workload| workload.pid), Some(12));
    assert_eq!(table.workload(13), None);
}

#[test]
fn a_tree_lists_children_before_parents_and_survives_a_cycle() {
    let table = actions_runner_job();
    let tree = table.tree(20);
    assert_eq!(tree.len(), 5);
    assert_eq!(*tree.last().unwrap(), 20);
    for (index, pid) in tree.iter().enumerate() {
        let parent = table.get(*pid).unwrap().parent;
        assert!(
            *pid == 20 || tree[index..].contains(&parent),
            "{pid} comes after its parent"
        );
    }
    // Ids reused between two reads can make a table with a cycle; it must end.
    let cycle = Table::new([process(5, 6, "a"), process(6, 5, "b")]);
    assert_eq!(cycle.tree(5).len(), 2);
    assert_eq!(cycle.step_of(5, 10), None);
}

#[test]
fn a_stat_line_is_read_whatever_the_name_holds() {
    let line = |name: &str| {
        format!(
            "4242 ({name}) S 17 4242 4242 0 -1 4194304 100 0 0 0 5 3 0 0 20 0 1 0 987654 1000000 250 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 2 0 0 0 0 0"
        )
    };
    for name in ["node", "Web Content", "a) R 1 (b", "(sd-pam)", ""] {
        assert_eq!(
            parse_stat(&line(name)),
            Some(Process {
                pid: 4242,
                parent: 17,
                name: name.into(),
                started: 987654
            }),
            "{name:?}"
        );
    }
    assert_eq!(parse_stat("4242 (node) S 17"), None);
    assert_eq!(parse_stat(""), None);
}

fn facts() -> JobFacts {
    JobFacts {
        host: "mac-1".into(),
        limits: Limits {
            memory_bytes: 4026531840,
            pids: 4096,
            shm_bytes: 1 << 30,
            cpus: 4,
        },
        disk_bytes: 30 << 30,
        docker: false,
        docker_cache: false,
    }
}

fn oom(limit_bytes: u64) -> OomEvent {
    OomEvent {
        process: Used {
            pid: 260,
            name: "node".into(),
            memory_bytes: 600 << 20,
        },
        limit_bytes,
        largest: vec![
            Used {
                pid: 260,
                name: "node".into(),
                memory_bytes: 600 << 20,
            },
            Used {
                pid: 261,
                name: "Web Content".into(),
                memory_bytes: 3 << 29,
            },
        ],
    }
}

#[test]
fn sizes_read_as_people_write_them() {
    for (bytes, text) in [
        (4026531840, "3.75 GB"),
        (4 << 30, "4 GB"),
        (30 << 30, "30 GB"),
        (3 << 29, "1.5 GB"),
        (805306368, "768 MB"),
        (1 << 30, "1 GB"),
        ((1 << 30) - 1, "1023 MB"),
        (0, "0 MB"),
        (u64::MAX, "17179869183.99 GB"),
    ] {
        assert_eq!(size(bytes), text);
    }
}

#[test]
fn a_job_is_told_its_size_at_the_start() {
    assert_eq!(
        started(&facts()),
        "This job runs on mac-1 with 3.75 GB memory, 4 CPUs, 30 GB disk."
    );
    assert_eq!(
        started(&JobFacts {
            docker: true,
            ..facts()
        }),
        "This job runs on mac-1 with 3.75 GB memory, 4 CPUs, 30 GB disk and Docker."
    );
}

#[test]
fn facts_from_a_supervisor_before_docker_read_as_no_docker() {
    let old =
        r#"{"host":"mac-1","limits":{"memory_bytes":1,"pids":1,"shm_bytes":1,"cpus":1},"disk_bytes":1}"#;
    assert!(!serde_json::from_str::<JobFacts>(old).unwrap().docker);
}

#[test]
fn a_job_without_docker_is_told_why_with_the_end_of_the_daemons_log() {
    let log = "one\n\ntwo\nthree\nfour\nfive\nsix\n\n";
    assert_eq!(
        docker_missing("its daemon did not answer within 60 seconds.", log),
        "Docker is not available in this job: its daemon did not answer within 60 seconds. The daemon's log ends with:\ntwo\nthree\nfour\nfive\nsix"
    );
    assert_eq!(
        docker_missing("the image has no group docker", ""),
        "Docker is not available in this job: the image has no group docker"
    );
    let long = "x".repeat(1000);
    assert_eq!(
        docker_missing("why", &long).len(),
        "Docker is not available in this job: why The daemon's log ends with:\n".len() + 300
    );
}

#[test]
fn a_group_is_found_by_its_whole_name() {
    let group = "root:x:0:\nsudo:x:27:runner\ndocker:x:123:runner\ndockerish:x:5:\nbroken:x\n";
    assert_eq!(find_group(group, "docker"), Some(123));
    assert_eq!(find_group(group, "sudo"), Some(27));
    assert_eq!(find_group(group, "dock"), None);
    assert_eq!(find_group(group, "broken"), None);
    assert_eq!(find_group("", "docker"), None);
    assert_eq!(find_group("docker:x:notanumber:", "docker"), None);
}

#[test]
fn an_out_of_memory_job_is_told_what_was_stopped_and_the_next_size_to_ask_for() {
    assert_eq!(
        out_of_memory_line(&oom(4026531840)),
        "This job used all of its 3.75 GB memory, so node was stopped. Ask for more with `runs-on: cf-6g`, or lower the number of test workers."
    );
    // 2 GB machine asks for 4, 6 for 8, and 8 is the end.
    assert!(out_of_memory_line(&oom((2 << 30) - (1 << 28))).contains("cf-4g"));
    assert!(out_of_memory_line(&oom((6 << 30) - (1 << 28))).contains("cf-8g"));
    let largest = out_of_memory_line(&oom((8 << 30) - (1 << 28)));
    assert!(
        largest.ends_with("This is the largest size. Lower the number of test workers."),
        "{largest}"
    );
    let summary = out_of_memory_summary(&oom(4026531840));
    assert!(summary.contains("`node` (process 260) was stopped"));
    assert!(summary.contains("| `node` | 600 MB |\n| `Web Content` | 1.5 GB |"));
    assert!(summary.contains("cf-6g"));
}

#[test]
fn a_job_the_runner_stopped_is_told_why_and_whose_fault_it_was() {
    assert_eq!(
        stopped(StopReason::EvictedMemory, "mac-1"),
        "Stopped by the CodeFusion Runner: mac-1 ran low on memory because of other work on it. This job did nothing wrong."
    );
    assert!(stopped(StopReason::EvictedDisk, "mac-2").contains("mac-2 ran low on disk space"));
    assert!(!stopped(StopReason::Timeout, "mac-2").contains("nothing wrong"));
}

#[test]
fn the_workload_gets_the_images_environment_and_the_jobs_own_wins() {
    let extra = vec![
        ("PATH".to_string(), "/custom".to_string()),
        ("CI".into(), "true".into()),
        ("BAD=NAME".into(), "x".into()),
        (String::new(), "x".into()),
        ("NUL".into(), "a\0b".into()),
    ];
    let environment = workload_environment("/home/runner", "runner", &extra);
    let get = |name: &str| {
        environment
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(get("PATH"), ["/custom"]);
    assert_eq!(get("HOME"), ["/home/runner"]);
    assert_eq!(get("CI"), ["true"]);
    assert_eq!(get("RUNNER_TOOL_CACHE"), ["/opt/hostedtoolcache"]);
    assert!(get("BAD=NAME").is_empty() && get("").is_empty() && get("NUL").is_empty());
}

#[test]
fn the_runner_user_comes_from_the_images_passwd() {
    let passwd =
        "root:x:0:0:root:/root:/bin/bash\nrunner:x:1001:1001:,,,:/home/runner:/bin/bash\nbad:x:a:b::/x:\n";
    assert_eq!(
        find_user(passwd, "runner"),
        Some((1001, 1001, "/home/runner".into()))
    );
    assert_eq!(find_user(passwd, "root"), Some((0, 0, "/root".into())));
    assert_eq!(find_user(passwd, "bad"), None);
    assert_eq!(find_user(passwd, "run"), None);
    assert_eq!(find_user("runner:x:1001", "runner"), None);
    assert_eq!(find_user("runner:x:1:1::relative:/bin/sh", "runner"), None);
}

#[test]
fn a_user_gets_the_groups_that_list_it_and_its_own_first() {
    let group = "root:x:0:\nsudo:x:27:runner\ndocker:x:123:other, runner\nrunner:x:1001:\nrunners:x:50:runner2,xrunner\nbroken\nbad:x:nan:runner\nsudo2:x:27:runner\n";
    assert_eq!(
        cf_supervisor::environment::groups_of(group, "runner", 1001),
        [1001, 27, 123]
    );
    assert_eq!(cf_supervisor::environment::groups_of("", "runner", 7), [7]);
}

#[test]
fn an_annotation_cannot_be_ended_or_forged_by_a_process_name() {
    use cf_supervisor::tell::annotation;
    assert_eq!(
        annotation("Out of memory", "node was stopped."),
        "::error title=Out of memory::node was stopped."
    );
    assert_eq!(
        annotation("a,b::c", "50% done\n::error::forged\r"),
        "::error title=a%2Cb%3A%3Ac::50%25 done%0A::error::forged%0D"
    );
}

#[test]
fn a_process_nobody_saw_is_of_the_one_step_that_runs() {
    let job = |steps: &[(u32, u32, &str)]| {
        let mut all = vec![
            process(10, 1, "supervisor"),
            process(11, 10, "supervisor"),
            process(12, 11, "run.sh"),
            process(13, 12, "Runner.Listener"),
            process(14, 13, "Runner.Worker"),
            process(40, 1, "other"),
        ];
        all.extend(
            steps
                .iter()
                .map(|(pid, parent, name)| process(*pid, *parent, name)),
        );
        Table::new(all)
    };
    let only = |table: Table| table.only_step(10).map(|step| step.pid);
    // One step with what it started: a process that came and went was its.
    assert_eq!(
        only(job(&[(20, 14, "bash"), (21, 20, "sleep"), (22, 21, "node")])),
        Some(20)
    );
    // Between two steps, and with two at once, nobody can say whose it was.
    assert_eq!(only(job(&[])), None);
    assert_eq!(
        only(job(&[(20, 14, "bash"), (30, 14, "bash"), (31, 30, "sleep")])),
        None
    );
    assert_eq!(only(actions_runner_job()), None);
    // What an earlier step left behind hangs below the container's first process and is nobody's step.
    assert_eq!(only(job(&[(50, 11, "leftover")])), None);
    assert_eq!(only(job(&[(50, 11, "leftover"), (20, 14, "bash")])), Some(20));
}

#[test]
fn a_job_does_not_reach_its_own_network_wherever_the_mac_put_it() {
    // The Mac's network for its machines can be set to any range, also one no list of private ranges has.
    let (address, netmask, gateway) = ([203, 0, 113, 9], [255, 255, 255, 0], [203, 0, 113, 1]);
    let own = [own_network(address, netmask), (gateway, 32)];
    assert_eq!(own[0], ([203, 0, 113, 0], 24));
    let rules = policy(&[gateway], &own).output;
    let to = |dest: [u8; 4], protocol: u8, dest_port: u16| {
        let packet = Packet {
            ether_type: IPV4,
            in_interface: None,
            loopback: false,
            dest,
            protocol,
            dest_port,
            established: false,
        };
        decide(&rules, Verdict::Accept, &packet)
    };
    // The next job's machine, the Mac's services at the gateway, the broadcast address.
    assert_eq!(to([203, 0, 113, 7], 6, 8000), Verdict::Drop);
    assert_eq!(to(gateway, 6, 22), Verdict::Drop);
    assert_eq!(to(gateway, 6, 445), Verdict::Drop);
    assert_eq!(to(gateway, 1, 0), Verdict::Drop);
    assert_eq!(to([203, 0, 113, 255], 17, 137), Verdict::Drop);
    // Names are still resolved, and the world next door is the internet.
    assert_eq!(to(gateway, 17, 53), Verdict::Accept);
    assert_eq!(to(gateway, 6, 53), Verdict::Accept);
    assert_eq!(to([203, 0, 114, 1], 6, 443), Verdict::Accept);
    assert_eq!(to([203, 0, 112, 255], 6, 443), Verdict::Accept);

    // A gateway outside the leased network is no way around it.
    let rules = policy(
        &[[1, 1, 1, 1]],
        &[own_network(address, netmask), ([198, 51, 100, 1], 32)],
    )
    .output;
    let packet = |dest| Packet {
        ether_type: IPV4,
        in_interface: None,
        loopback: false,
        dest,
        protocol: 6,
        dest_port: 22,
        established: false,
    };
    assert_eq!(
        decide(&rules, Verdict::Accept, &packet([198, 51, 100, 1])),
        Verdict::Drop
    );
    assert_eq!(
        decide(&rules, Verdict::Accept, &packet([198, 51, 100, 2])),
        Verdict::Accept
    );

    // The kernel is given the same rules.
    let with = firewall::netlink(&policy(&[gateway], &own)).unwrap();
    let without = firewall::netlink(&policy(&[gateway], &[])).unwrap();
    assert_eq!(with.acknowledged, without.acknowledged + 2);
}

#[test]
fn a_job_does_not_reach_the_networks_its_mac_is_in_whatever_their_range() {
    use cf_supervisor::firewall::{MOST_TOLD, beside};
    let (address, netmask, gateway) = ([192, 168, 64, 9], [255, 255, 255, 0], [192, 168, 64, 1]);
    // The Mac sits in an office network with public addresses and has a VPN address of its own.
    let told = [([145, 97, 20, 0], 22), ([25, 3, 2, 1], 32)];
    let own = beside(address, netmask, Some(gateway), &told);
    assert_eq!(
        own,
        [
            ([192, 168, 64, 0], 24),
            ([192, 168, 64, 1], 32),
            ([145, 97, 20, 0], 22),
            ([25, 3, 2, 1], 32)
        ]
    );
    let rules = policy(&[gateway], &own).output;
    let to = |dest: [u8; 4], protocol: u8, dest_port: u16| {
        let packet = Packet {
            ether_type: IPV4,
            in_interface: None,
            loopback: false,
            dest,
            protocol,
            dest_port,
            established: false,
        };
        decide(&rules, Verdict::Accept, &packet)
    };
    // The Mac at its office address, a colleague's machine, the VPN address; all ports and protocols.
    assert_eq!(to([145, 97, 20, 9], 6, 22), Verdict::Drop);
    assert_eq!(to([145, 97, 23, 255], 6, 445), Verdict::Drop);
    assert_eq!(to([145, 97, 21, 7], 17, 53), Verdict::Drop);
    assert_eq!(to([25, 3, 2, 1], 6, 5900), Verdict::Drop);
    // Next to them is the internet.
    assert_eq!(to([145, 97, 24, 0], 6, 443), Verdict::Accept);
    assert_eq!(to([145, 97, 19, 255], 6, 443), Verdict::Accept);
    assert_eq!(to([25, 3, 2, 2], 6, 443), Verdict::Accept);
    assert_eq!(to(gateway, 17, 53), Verdict::Accept);

    // Without a gateway there is none to add; a host's network is cut to its prefix; one with a prefix no
    // address has is left out, and so is what comes after the first 64.
    assert_eq!(
        beside(
            address,
            netmask,
            None,
            &[([145, 97, 21, 9], 22), ([8, 8, 8, 8], 33)]
        ),
        [([192, 168, 64, 0], 24), ([145, 97, 20, 0], 22)]
    );
    let many: Vec<_> = (0..200u8).map(|n| ([n.max(1), 1, 1, 1], 32)).collect();
    assert_eq!(beside(address, netmask, None, &many).len(), 1 + MOST_TOLD);
}

#[test]
fn a_job_does_not_reach_the_ranges_set_aside_for_inside_a_machine_or_site() {
    let rules = policy(&[], &[]).output;
    let to = |dest: [u8; 4]| {
        let packet = Packet {
            ether_type: IPV4,
            in_interface: None,
            loopback: false,
            dest,
            protocol: 6,
            dest_port: 443,
            established: false,
        };
        decide(&rules, Verdict::Accept, &packet)
    };
    for inside in [
        // A proxy's or VPN's made-up addresses on the Mac, the translated address of a Mac on an IPv6-only
        // network, "this network".
        [198, 18, 0, 1],
        [198, 19, 255, 255],
        [192, 0, 0, 2],
        [192, 0, 0, 255],
        [0, 0, 0, 0],
        [0, 255, 255, 255],
    ] {
        assert_eq!(to(inside), Verdict::Drop, "{inside:?}");
    }
    for outside in [
        [198, 17, 255, 255],
        [198, 20, 0, 0],
        [192, 0, 1, 0],
        [191, 255, 255, 255],
        [1, 0, 0, 0],
    ] {
        assert_eq!(to(outside), Verdict::Accept, "{outside:?}");
    }
}

#[test]
fn the_network_of_a_lease_is_its_address_under_its_mask() {
    for (address, netmask, network) in [
        ([192, 168, 64, 7], [255, 255, 255, 0], ([192, 168, 64, 0], 24)),
        ([10, 211, 55, 130], [255, 255, 255, 128], ([10, 211, 55, 128], 25)),
        ([172, 20, 9, 9], [255, 255, 0, 0], ([172, 20, 0, 0], 16)),
        ([8, 8, 8, 8], [255, 255, 255, 255], ([8, 8, 8, 8], 32)),
        // A mask with a hole counts up to the hole: more is blocked, never less.
        ([10, 1, 2, 3], [255, 0, 255, 0], ([10, 0, 0, 0], 8)),
        // No mask at all blocks everything: a job without a network, not a job with the Mac's.
        ([10, 1, 2, 3], [0, 0, 0, 0], ([0, 0, 0, 0], 0)),
    ] {
        assert_eq!(own_network(address, netmask), network, "{address:?} {netmask:?}");
    }
    let rules = policy(&[], &[own_network([10, 1, 2, 3], [0, 0, 0, 0])]).output;
    let packet = Packet {
        ether_type: IPV4,
        in_interface: None,
        loopback: false,
        dest: [140, 82, 121, 4],
        protocol: 6,
        dest_port: 443,
        established: false,
    };
    assert_eq!(decide(&rules, Verdict::Accept, &packet), Verdict::Drop);
}

#[test]
fn a_dhcp_question_is_asked_again_soon_at_first_and_every_700_ms_from_then_on() {
    use std::time::Duration;
    let waits: Vec<u64> = (0..6)
        .map(|asked| dhcp::ask_again_after(asked).as_millis() as u64)
        .collect();
    assert_eq!(waits, [100, 200, 400, 700, 700, 700]);
    assert_eq!(dhcp::ask_again_after(u32::MAX), Duration::from_millis(700));
    // A first question lost while the device came up costs a tenth of a second, not most of one.
    assert!(dhcp::ask_again_after(0) <= Duration::from_millis(100));
}

#[test]
fn a_stop_the_kernel_counted_but_did_not_log_is_believed_at_the_second_look_or_at_the_last() {
    // The log names every stop: nothing is left to guess.
    let mut count = Count::default();
    assert!(!count.look(0, 0, false));
    assert!(!count.look(1, 1, false));
    assert!(!count.look(2, 3, false), "the log may only be behind");
    assert!(
        !count.look(1, 4, false),
        "the log caught up with one and fell behind by another"
    );

    // The log stays behind for two looks: the counter is believed, once.
    let mut count = Count::default();
    assert!(!count.look(0, 1, false));
    assert!(count.look(0, 1, false));
    assert!(!count.look(0, 1, false));
    assert!(!count.look(0, 1, true));
    // And again for the next stop.
    assert!(!count.look(0, 2, false));
    assert!(count.look(0, 2, false));

    // The log catches up at the second look: its named stop is the report, no second one.
    let mut count = Count::default();
    assert!(!count.look(0, 1, false));
    assert!(!count.look(1, 1, false));
    assert!(!count.look(0, 1, false));

    // The job's last look has no second one: a workload that died of the stop is out of memory all the same.
    let mut count = Count::default();
    assert!(count.look(0, 1, true));
    assert!(!count.look(0, 1, true));
    let mut count = Count::default();
    assert!(!count.look(1, 1, true), "named by the log at the last look");
    let mut count = Count::default();
    assert!(!count.look(0, 0, true), "nothing was stopped");
    // A counter that went backwards (a group made anew) reports nothing.
    let mut count = Count::default();
    assert!(!count.look(3, 3, false));
    assert!(!count.look(0, 1, true));
}

#[test]
fn a_groups_own_limit_is_read_from_its_local_events() {
    use cf_supervisor::oom::limit_reached;
    assert_eq!(
        limit_reached("low 0\nhigh 0\nmax 9\noom 2\noom_kill 5\noom_group_kill 0\n"),
        2
    );
    assert_eq!(limit_reached("oom_kill 5\n"), 0, "oom_kill is not oom");
    assert_eq!(limit_reached(""), 0);
}

#[test]
fn a_docker_jobs_containers_stopped_at_their_own_limit_are_not_the_jobs() {
    use cf_supervisor::oom::OwnLimit;
    let mut own = OwnLimit::default();
    assert_eq!(own.look(0, 0), (false, 0));
    // A container ran into its own limit: two stops, the job's limit untouched.
    assert_eq!(own.look(0, 2), (false, 0));
    assert_eq!(own.look(0, 2), (false, 0));
    // The job reached its limit, and the kernel stopped one process at the same look.
    assert_eq!(own.look(1, 3), (true, 1));
    // The kernel counted the limit at one look and stopped the process at the next: still the job's.
    assert_eq!(own.look(2, 3), (true, 1));
    assert_eq!(own.look(2, 4), (true, 2));
    // Two looks later the container's own stops are theirs again.
    assert_eq!(own.look(2, 6), (false, 2));
    assert_eq!(own.look(2, 6), (false, 2));
}

proptest! {
    #[test]
    fn the_jobs_own_count_never_goes_back_or_past_the_kernels(
        looks in proptest::collection::vec((0u64..4, 0u64..4), 0..40)
    ) {
        use cf_supervisor::oom::OwnLimit;
        let mut own = OwnLimit::default();
        let (mut reached, mut counted, mut last) = (0, 0, 0);
        for (more_reached, more_counted) in looks {
            reached += more_reached;
            counted += more_counted;
            let (ours, count) = own.look(reached, counted);
            prop_assert!(count >= last && count <= counted);
            if !ours {
                prop_assert_eq!(count, last, "no new stop is the job's while its limit was not reached");
            }
            last = count;
        }
    }
}
