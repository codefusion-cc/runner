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

#[test]
fn the_daemon_listens_on_the_shared_socket_only() {
    let command = dockerd_command();
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
