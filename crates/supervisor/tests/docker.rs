//! The Docker sandbox's ids and commands, held against what they must be.

use cf_supervisor::docker::*;

use proptest::prelude::*;

fn outside(extents: &[Extent], id: u32) -> Option<u32> {
    extents
        .iter()
        .find(|(inside, _, count)| (*inside..inside + count).contains(&id))
        .map(|(inside, outside, _)| outside + (id - inside))
}

#[test]
fn a_job_user_keeps_its_id_and_root_is_shifted() {
    let map = extents(&[1001, 123]);
    assert_eq!(
        map,
        vec![
            (0, SHIFT, 123),
            (123, 123, 1),
            (124, SHIFT + 124, 1001 - 124),
            (1001, 1001, 1),
            (1002, SHIFT + 1002, IDS - 1002),
        ]
    );
    assert_eq!(map_text(&map[..2]), format!("0 {SHIFT} 123\n123 123 1\n"));
}

#[test]
fn root_and_ids_out_of_range_never_keep_their_own() {
    for same in [vec![0], vec![IDS], vec![u32::MAX], vec![0, IDS, 70000]] {
        assert_eq!(extents(&same), vec![(0, SHIFT, IDS)], "{same:?}");
    }
}

#[test]
fn neighbours_and_the_ends() {
    assert_eq!(
        extents(&[1, 2, IDS - 1]),
        vec![
            (0, SHIFT, 1),
            (1, 1, 1),
            (2, 2, 1),
            (3, SHIFT + 3, IDS - 4),
            (IDS - 1, IDS - 1, 1)
        ]
    );
}

proptest! {
    #[test]
    fn every_id_once_none_is_root_no_overlap(same in proptest::collection::vec(prop_oneof![0u32..4, 0u32..2048, IDS - 2..IDS + 2, any::<u32>()], 0..400)) {
        let map = extents(&same);
        prop_assert!(map.len() <= MOST_EXTENTS);
        // Inside: 0..IDS, each once, in order.
        let mut expected = 0;
        for (inside, _, count) in &map {
            prop_assert_eq!(*inside, expected);
            prop_assert!(*count > 0);
            expected += count;
        }
        prop_assert_eq!(expected, IDS);
        // Outside: no range meets another, and none holds the machine's root.
        let mut ranges: Vec<(u32, u32)> = map.iter().map(|(_, outside, count)| (*outside, outside + count)).collect();
        ranges.sort_unstable();
        for pair in ranges.windows(2) {
            prop_assert!(pair[0].1 <= pair[1].0);
        }
        prop_assert!(ranges.iter().all(|(start, _)| *start > 0));
        prop_assert_eq!(outside(&map, 0), Some(SHIFT));
        // An id asked for keeps its own unless it is 0 or out of range, or there were too many to keep.
        let mut wanted: Vec<u32> = same.iter().copied().filter(|id| (1..IDS).contains(id)).collect();
        wanted.sort_unstable();
        wanted.dedup();
        for id in wanted.iter().take(MOST_EXTENTS / 2 - 1) {
            prop_assert_eq!(outside(&map, *id), Some(*id));
        }
        for id in [1u32, 500, 1001, IDS - 1] {
            let to = outside(&map, id).unwrap();
            prop_assert!(to == id || to == SHIFT + id);
        }
    }
}

#[test]
fn pasta_keeps_the_gateway_and_forwards_nothing_inwards() {
    let command = pasta_command("/proc/7/ns/net");
    let text = command.join(" ");
    assert!(text.contains("--no-map-gw"), "{text}");
    assert!(text.contains("--tcp-ns none --udp-ns none"), "{text}");
    assert!(text.ends_with("--netns-only --netns /proc/7/ns/net"), "{text}");
    assert!(text.contains("--ipv4-only"), "{text}");
}

/// Nothing next to the machine but its own network and gateway, as on a Mac's NAT.
fn alone() -> Networks {
    networks(&[([192, 168, 64, 0], 24), ([192, 168, 64, 1], 32)]).expect("networks")
}

#[test]
fn the_daemon_listens_on_the_shared_socket_only() {
    let command = dockerd_command(&alone());
    let hosts: Vec<&String> = command
        .iter()
        .zip(command.iter().skip(1))
        .filter(|(flag, _)| *flag == "--host")
        .map(|(_, value)| value)
        .collect();
    assert_eq!(hosts, vec![&format!("unix://{SOCKET}")]);
    assert!(command.contains(&"--ip6tables=false".to_string()));
}

#[test]
fn only_tcp_segments_without_any_flag_are_dropped_before_the_daemons_nat() {
    assert_eq!(
        flagless_tcp_rule().join(" "),
        "iptables --wait -t raw -I PREROUTING -p tcp --tcp-flags ALL NONE -j DROP"
    );
}

#[test]
fn the_socket_and_the_log_are_in_the_shared_folder() {
    assert_eq!(SOCKET, format!("{SOCKET_DIR}/{SOCKET_NAME}"));
    assert_eq!(LOG, format!("{SOCKET_DIR}/{LOG_NAME}"));
}

#[test]
fn with_nothing_near_the_daemon_gets_dockers_own_networks() {
    assert_eq!(
        alone(),
        Networks {
            bridge: ([172, 17, 0, 0], 16),
            pools: vec![
                ([172, 18, 0, 0], 16),
                ([172, 19, 0, 0], 16),
                ([172, 20, 0, 0], 16),
                ([172, 21, 0, 0], 16),
            ],
        }
    );
    let text = dockerd_command(&alone()).join(" ");
    assert!(text.contains("--bip 172.17.0.1/16"), "{text}");
    assert!(
        text.contains("--default-address-pool base=172.18.0.0/16,size=24")
            && text.contains("--default-address-pool base=172.21.0.0/16,size=24"),
        "{text}"
    );
}

#[test]
fn a_host_on_dockers_own_bridge_moves_the_daemons() {
    // A Linux host with Docker of its own: docker0 is 172.17.0.1/16, which the host tells as one of its networks.
    let got = networks(&[([10, 0, 2, 0], 24), ([172, 17, 0, 0], 16)]).unwrap();
    assert_eq!(got.bridge, ([172, 18, 0, 0], 16));
    assert_eq!(got.pools.first(), Some(&([172, 19, 0, 0], 16)));
    // Only the host's address, with no network told: the block that holds it still goes.
    let got = networks(&[([172, 17, 0, 1], 32)]).unwrap();
    assert_eq!(got.bridge, ([172, 18, 0, 0], 16));
    // The last address of a block and the first of the next.
    let got = networks(&[([172, 17, 255, 255], 32), ([172, 18, 0, 0], 32)]).unwrap();
    assert_eq!(got.bridge, ([172, 19, 0, 0], 16));
}

#[test]
fn a_host_in_all_of_172_16_gets_networks_of_10() {
    let got = networks(&[([172, 16, 0, 0], 12)]).unwrap();
    assert_eq!(got.bridge, ([10, 0, 0, 0], 16));
    assert_eq!(got.pools.len(), POOLS);
    let got = networks(&[([172, 16, 0, 0], 12), ([10, 0, 0, 0], 8)]).unwrap();
    assert_eq!(got.bridge, ([198, 18, 0, 0], 16));
    assert_eq!(got.pools, vec![([198, 19, 0, 0], 16)]);
}

#[test]
fn with_no_room_for_a_bridge_and_a_pool_there_is_no_daemon() {
    let private = [([172, 16, 0, 0], 12), ([10, 0, 0, 0], 8)];
    assert_eq!(networks(&[private[0], private[1], ([198, 18, 0, 0], 15)]), None);
    assert_eq!(
        networks(&[private[0], private[1], ([198, 18, 0, 0], 16)]),
        None,
        "one block is a bridge alone"
    );
    assert_eq!(networks(&[([0, 0, 0, 0], 0)]), None, "a host told everything");
}

#[test]
fn networks_overlap_when_they_share_an_address() {
    assert!(overlap(([172, 17, 0, 0], 16), ([172, 17, 255, 255], 32)));
    assert!(!overlap(([172, 17, 0, 0], 16), ([172, 18, 0, 0], 32)));
    assert!(overlap(([172, 17, 0, 0], 16), ([172, 16, 0, 0], 12)));
    assert!(overlap(([1, 2, 3, 4], 32), ([0, 0, 0, 0], 0)));
    assert!(!overlap(([10, 0, 0, 0], 8), ([11, 0, 0, 0], 8)));
}

fn near() -> impl Strategy<Value = (Ip, u8)> {
    // Networks of every size, mostly in the ranges the daemon takes from, so the cases meet.
    let address = prop_oneof![
        any::<[u8; 4]>(),
        (16u8..32, any::<u8>(), any::<u8>()).prop_map(|(b, c, d)| [172, b, c, d]),
        (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(b, c, d)| [10, b, c, d]),
    ];
    (address, 0u8..=32)
}

proptest! {
    #[test]
    fn the_daemons_networks_never_share_an_address_with_what_is_near(beside in prop::collection::vec(near(), 0..80)) {
        if let Some(got) = networks(&beside) {
            let all: Vec<(Ip, u8)> = std::iter::once(got.bridge).chain(got.pools.iter().copied()).collect();
            prop_assert!(!got.pools.is_empty() && got.pools.len() <= POOLS);
            for (index, own) in all.iter().enumerate() {
                prop_assert_eq!(own.1, 16);
                for other in &beside {
                    prop_assert!(!overlap(*own, *other), "{:?} overlaps {:?}", own, other);
                }
                for later in &all[index + 1..] {
                    prop_assert!(!overlap(*own, *later), "{:?} twice", own);
                }
            }
        }
    }
}

#[test]
fn the_sandboxs_reason_is_among_the_last_words() {
    let log = b"time=\"x\" level=info msg=\"Starting up\"\n\ncf-sandbox: pasta ended with exit status: 1: the daemon has no network\n";
    assert_eq!(
        last_words(log),
        vec![
            "time=\"x\" level=info msg=\"Starting up\"".to_string(),
            "cf-sandbox: pasta ended with exit status: 1: the daemon has no network".to_string(),
        ]
    );
}

#[test]
fn last_words_are_the_last_lines_in_order() {
    let log: String = (1..=12).map(|n| format!("line {n}\n")).collect();
    let words = last_words(log.as_bytes());
    assert_eq!(words.len(), LAST_WORDS_LINES);
    assert_eq!(words.first().map(String::as_str), Some("line 8"));
    assert_eq!(words.last().map(String::as_str), Some("line 12"));
}

#[test]
fn an_empty_or_blank_log_has_no_last_words() {
    assert!(last_words(b"").is_empty());
    assert!(last_words(b"\n \n\r\n").is_empty());
}

#[test]
fn last_words_cannot_steer_the_console() {
    let hostile = format!(
        "\x1b[2Jcf-supervisor: power off\u{202e}\r{}",
        "x".repeat(LAST_WORDS_CHARS * 2)
    );
    let mut bytes = hostile.into_bytes();
    bytes.extend([0xff, b'\n']);
    let words = last_words(&bytes);
    assert_eq!(words.len(), 1);
    let line = &words[0];
    assert!(line.chars().count() <= LAST_WORDS_CHARS);
    assert!(!line.chars().any(|c| c.is_control() || c == '\u{202e}'));
    assert!(line.starts_with("[2Jcf-supervisor: power off"));
}

/// What an image built before Docker jobs has: the Actions runner image's engine, nothing more.
fn before_docker_jobs(path: &str) -> bool {
    path == "/usr/bin/dockerd"
}

#[test]
fn an_image_from_before_docker_jobs_gets_no_daemon_and_says_why() {
    let problem = missing_programs(before_docker_jobs).expect("a problem");
    assert!(
        problem.starts_with("the image has no iptables or pasta: "),
        "{problem}"
    );
}

#[test]
fn an_image_with_every_program_on_the_path_is_fine() {
    let image = ["/usr/bin/dockerd", "/usr/sbin/iptables", "/usr/local/bin/pasta"];
    assert_eq!(missing_programs(|path| image.contains(&path)), None);
}

#[test]
fn a_program_off_the_path_counts_as_missing() {
    let image = [
        "/usr/bin/dockerd",
        "/usr/sbin/iptables",
        "/opt/passt/pasta",
        "/usr/bin/pasta/",
    ];
    let problem = missing_programs(|path| image.contains(&path)).expect("a problem");
    assert!(problem.starts_with("the image has no pasta: "), "{problem}");
    let problem = missing_programs(|_| false).expect("a problem");
    assert!(
        problem.starts_with("the image has no dockerd or iptables or pasta: "),
        "{problem}"
    );
}

#[test]
fn the_sandbox_runs_only_programs_it_checks_for() {
    for command in [
        dockerd_command(&alone()),
        pasta_command("/proc/2/ns/net"),
        flagless_tcp_rule(),
    ] {
        assert!(PROGRAMS.contains(&command[0].as_str()), "{}", command[0]);
    }
}

proptest! {
    #[test]
    fn last_words_are_few_short_and_harmless_for_any_log(log in prop::collection::vec(any::<u8>(), 0..4096)) {
        let words = last_words(&log);
        prop_assert!(words.len() <= LAST_WORDS_LINES);
        for line in &words {
            prop_assert!(!line.is_empty() && line.chars().count() <= LAST_WORDS_CHARS);
            prop_assert!(!line.chars().any(char::is_control), "{:?}", line);
            prop_assert_eq!(line.trim(), line.as_str());
        }
    }
}

#[test]
fn a_docker_jobs_cache_is_tidy_only_when_its_daemon_says_no_container_or_volume_is_left() {
    use cf_supervisor::docker::left_nothing;
    let ok = |body: &str| format!("HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{body}\n");
    let none = (ok("[]"), ok(r#"{"Volumes":[],"Warnings":null}"#));
    assert!(left_nothing(&none.0, &none.1));
    assert!(left_nothing(&none.0, &ok(r#"{"Volumes":null,"Warnings":null}"#)));
    // What the end hook could not remove: a container that would start again, a volume.
    assert!(!left_nothing(&ok(r#"[{"Id":"abc","State":"exited"}]"#), &none.1));
    assert!(!left_nothing(
        &none.0,
        &ok(r#"{"Volumes":[{"Name":"data"}],"Warnings":null}"#)
    ));
    // A daemon that did not answer, answered an error or something this cannot read says nothing is tidy.
    for broken in [
        String::new(),
        "HTTP/1.0 500 Internal Server Error\r\n\r\n[]".to_string(),
        ok("not json"),
        ok("{}"),
        "[]".to_string(),
    ] {
        assert!(!left_nothing(&broken, &none.1), "containers: {broken:?}");
        assert!(!left_nothing(&none.0, &broken), "volumes: {broken:?}");
    }
}
