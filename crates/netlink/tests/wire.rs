//! Netlink's wire form, from its spec: a message is a 16-byte header and a body, an attribute a 4-byte header and a
//! payload, each padded to 4 bytes, and every length counts its own header but not the padding.

use cf_netlink::{
    LONGEST_PAYLOAD, Message, NLA_F_NESTED, attribute, attribute_str, attributes, message, messages, nested,
};
use proptest::prelude::*;

#[test]
fn an_attribute_is_its_length_kind_payload_and_padding() {
    let mut bytes = Vec::new();
    attribute(&mut bytes, 3, b"abcde");
    assert_eq!(bytes.len(), 12);
    assert_eq!(u16::from_ne_bytes([bytes[0], bytes[1]]), 9);
    assert_eq!(u16::from_ne_bytes([bytes[2], bytes[3]]), 3);
    assert_eq!(&bytes[4..9], b"abcde");
    assert_eq!(&bytes[9..], [0, 0, 0]);
    let mut empty = Vec::new();
    attribute(&mut empty, 1, b"");
    assert_eq!(empty, [4u16.to_ne_bytes(), 1u16.to_ne_bytes()].concat());
}

#[test]
fn a_text_attribute_ends_in_a_nul_inside_its_length() {
    let mut bytes = Vec::new();
    attribute_str(&mut bytes, 3, "br0");
    assert_eq!(
        bytes,
        [8u16.to_ne_bytes(), 3u16.to_ne_bytes()]
            .concat()
            .into_iter()
            .chain(*b"br0\0")
            .collect::<Vec<_>>()
    );
    let mut empty = Vec::new();
    attribute_str(&mut empty, 1, "");
    assert_eq!(
        empty,
        [5u16.to_ne_bytes(), 1u16.to_ne_bytes(), [0, 0], [0, 0]].concat()
    );
    // Ten bytes of UTF-8 and the NUL: the length counts 15, the padding makes 16.
    let mut unicode = Vec::new();
    attribute_str(&mut unicode, 1, "zażółć");
    assert_eq!(u16::from_ne_bytes([unicode[0], unicode[1]]), 15);
    assert_eq!(&unicode[4..15], "zażółć\0".as_bytes());
    assert_eq!(unicode.len(), 16);
}

#[test]
fn the_longest_payload_fits_and_one_more_byte_does_not() {
    let mut bytes = Vec::new();
    attribute(&mut bytes, 1, &vec![7; LONGEST_PAYLOAD]);
    assert_eq!(u16::from_ne_bytes([bytes[0], bytes[1]]), u16::MAX);
    let over = std::panic::catch_unwind(|| attribute(&mut Vec::new(), 1, &vec![7; LONGEST_PAYLOAD + 1]));
    assert!(over.is_err());
}

#[test]
fn a_nested_attribute_says_so_and_reads_back_without_the_flag() {
    let mut bytes = Vec::new();
    nested(&mut bytes, 18, |inner| {
        attribute(inner, 1, b"bridge");
        attribute(inner, 2, &7u32.to_ne_bytes());
    });
    assert_eq!(u16::from_ne_bytes([bytes[2], bytes[3]]), 18 | NLA_F_NESTED);
    let outer: Vec<_> = attributes(&bytes).collect::<Result<_, _>>().unwrap();
    assert_eq!(outer.len(), 1);
    assert_eq!(outer[0].0, 18);
    let inner: Vec<_> = attributes(outer[0].1).collect::<Result<_, _>>().unwrap();
    assert_eq!(inner, [(1, &b"bridge"[..]), (2, &7u32.to_ne_bytes()[..])]);
}

#[test]
fn a_message_is_its_header_and_body_padded() {
    let mut bytes = Vec::new();
    message(&mut bytes, 16, 0x405, 9, b"xyz");
    assert_eq!(bytes.len(), 20);
    assert_eq!(u32::from_ne_bytes(bytes[..4].try_into().unwrap()), 19);
    assert_eq!(&bytes[12..16], [0; 4], "the port is the kernel's to fill");
    let read: Vec<Message<'_>> = messages(&bytes).collect::<Result<_, _>>().unwrap();
    assert_eq!(
        read,
        [Message {
            kind: 16,
            flags: 0x405,
            sequence: 9,
            body: b"xyz"
        }]
    );
}

#[test]
fn lengths_that_do_not_fit_are_errors_and_end_the_reading() {
    let mut bytes = Vec::new();
    message(&mut bytes, 1, 0, 1, b"ok");
    let good = bytes.len();
    // A length beyond what is there, a length shorter than the header, and a header cut short.
    for broken in [
        [40u32.to_ne_bytes().to_vec(), vec![0; 12]].concat(),
        [8u32.to_ne_bytes().to_vec(), vec![0; 12]].concat(),
        vec![20, 0, 0],
    ] {
        let mut all = bytes.clone();
        all.extend_from_slice(&broken);
        let read: Vec<_> = messages(&all).collect();
        assert_eq!(read.len(), 2, "{broken:?}");
        assert!(read[0].is_ok());
        assert!(read[1].is_err());
    }
    assert_eq!(messages(&bytes[..good]).count(), 1);
    assert_eq!(messages(&[]).count(), 0);
    let mut cut = Vec::new();
    attribute(&mut cut, 1, b"abcdef");
    cut.truncate(6);
    let read: Vec<_> = attributes(&cut).collect();
    assert!(read.len() == 1 && read[0].is_err());
}

#[test]
fn the_last_piece_may_go_without_its_padding() {
    let mut bytes = Vec::new();
    attribute(&mut bytes, 1, b"a");
    bytes.truncate(5);
    let read: Vec<_> = attributes(&bytes).collect::<Result<_, _>>().unwrap();
    assert_eq!(read, [(1, &b"a"[..])]);
}

proptest! {
    #[test]
    fn messages_and_attributes_read_back_as_written(
        written in proptest::collection::vec(
            (any::<u16>(), any::<u16>(), any::<u32>(), proptest::collection::vec((0u16..0x4000, proptest::collection::vec(any::<u8>(), 0..40)), 0..5)),
            0..6,
        )
    ) {
        let mut bytes = Vec::new();
        let mut bodies = Vec::new();
        for (kind, flags, sequence, attrs) in &written {
            let mut body = Vec::new();
            for (attr_kind, payload) in attrs {
                attribute(&mut body, *attr_kind, payload);
            }
            message(&mut bytes, *kind, *flags, *sequence, &body);
            bodies.push(body);
        }
        prop_assert_eq!(bytes.len() % 4, 0);
        let read: Vec<Message<'_>> = messages(&bytes).collect::<Result<_, _>>().unwrap();
        prop_assert_eq!(read.len(), written.len());
        for ((message, (kind, flags, sequence, attrs)), body) in read.iter().zip(&written).zip(&bodies) {
            prop_assert_eq!((message.kind, message.flags, message.sequence), (*kind, *flags, *sequence));
            prop_assert_eq!(message.body, &body[..]);
            let back: Vec<(u16, Vec<u8>)> = attributes(message.body)
                .map(|attribute| attribute.map(|(kind, payload)| (kind, payload.to_vec())))
                .collect::<Result<_, _>>()
                .unwrap();
            prop_assert_eq!(&back, attrs);
        }
    }

    #[test]
    fn any_bytes_are_read_without_a_panic_and_stop_at_the_first_error(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
        let read: Vec<_> = messages(&bytes).collect();
        prop_assert!(read.iter().rev().skip(1).all(Result::is_ok));
        let read: Vec<_> = attributes(&bytes).collect();
        prop_assert!(read.iter().rev().skip(1).all(Result::is_ok));
    }
}
