//! The reply parser against any bytes: it never panics, keeps no more than it may, and reads back what a server
//! wrote.

use cf_a2s::{Malformed, NAME_CHARS, Reply, parse, request};
use proptest::prelude::*;

fn info(name: &[u8], strings: [&[u8]; 3], players: u8, max: u8, tail: &[u8]) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFF, 0xFF, 0xFF, 0x49, 17];
    out.extend_from_slice(name);
    out.push(0);
    for s in strings {
        out.extend_from_slice(s);
        out.push(0);
    }
    out.extend_from_slice(&[0, 0, players, max]);
    out.extend_from_slice(tail);
    out
}

fn no_nul() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(1u8..=255, 0..400)
}

fn kept(name: &str) {
    assert!(name.chars().count() <= NAME_CHARS, "{name:?}");
    assert!(!name.chars().any(char::is_control), "{name:?}");
    assert!(
        !name
            .chars()
            .any(|c| matches!(c, '\u{202a}'..='\u{202e}' | '\u{200b}'..='\u{200f}')),
        "{name:?}"
    );
    assert_eq!(name, name.trim());
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

    #[test]
    fn any_bytes_read_without_a_panic(bytes in proptest::collection::vec(any::<u8>(), 0..1600)) {
        if let Ok(Reply::Info(i)) = parse(&bytes) {
            kept(&i.name);
        }
    }

    #[test]
    fn any_bytes_after_the_header_read_without_a_panic(kind in any::<u8>(), rest in proptest::collection::vec(any::<u8>(), 0..300)) {
        let bytes = [&[0xFF, 0xFF, 0xFF, 0xFF, kind][..], &rest].concat();
        match parse(&bytes) {
            Ok(Reply::Info(i)) => { prop_assert_eq!(kind, 0x49); kept(&i.name); }
            Ok(Reply::Challenge(c)) => { prop_assert_eq!(kind, 0x41); prop_assert_eq!(&c[..], &rest[..4]); }
            Err(Malformed::Kind(k)) => prop_assert!(k == kind && k != 0x41 && k != 0x49),
            Err(_) => prop_assert!(kind == 0x41 || kind == 0x49),
        }
    }

    #[test]
    fn a_written_reply_reads_back(
        name in no_nul(), map in no_nul(), folder in no_nul(), game in no_nul(),
        players in any::<u8>(), max in any::<u8>(), tail in proptest::collection::vec(any::<u8>(), 0..40),
    ) {
        let bytes = info(&name, [&map, &folder, &game], players, max, &tail);
        let Ok(Reply::Info(i)) = parse(&bytes) else { panic!("not read: {bytes:02x?}") };
        prop_assert_eq!((i.players, i.max), (players, max));
        kept(&i.name);
        // A plain name comes back as it was sent, cut to its most.
        if name.iter().all(|b| b.is_ascii_alphanumeric()) {
            let want: String = String::from_utf8_lossy(&name).chars().take(NAME_CHARS).collect();
            prop_assert_eq!(i.name, want);
        }
    }

    #[test]
    fn a_reply_cut_short_is_never_read(name in no_nul(), players in any::<u8>(), max in any::<u8>(), cut in any::<prop::sample::Index>()) {
        let bytes = info(&name, [b"m", b"f", b"g"], players, max, &[]);
        let at = cut.index(bytes.len());
        prop_assert!(parse(&bytes[..at]).is_err(), "read {} of {} bytes", at, bytes.len());
    }

    #[test]
    fn a_challenge_reads_back(c in any::<[u8; 4]>(), tail in proptest::collection::vec(any::<u8>(), 0..8)) {
        let bytes = [&[0xFF, 0xFF, 0xFF, 0xFF, 0x41][..], &c, &tail].concat();
        prop_assert_eq!(parse(&bytes), Ok(Reply::Challenge(c)));
        // And the query carries it back as sent, after the plain query.
        let again = request(Some(c));
        prop_assert_eq!(&again[..again.len() - 4], &request(None)[..]);
        prop_assert_eq!(&again[again.len() - 4..], &c[..]);
    }
}
