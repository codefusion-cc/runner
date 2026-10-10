//! The protocol as its two ends meet it: what one writes the other reads, and nothing a peer sends breaks a reader.

use std::io::Cursor;

use cf_guest_proto::{
    Decoder, GuestEnd, GuestMessage, HostMessage, Job, Limits, MAX_FRAME, OomEvent, PROTOCOL, Process,
    ProtoError, Secret, Start, Stop, StopReason, Summary, Task, Usage, Workload, encode, read_message,
    write_message,
};
use proptest::prelude::*;

fn job(secret: &str) -> Job {
    Job {
        name: "cf-air-1".into(),
        host: "air".into(),
        workload: Workload::ActionsRunner {
            jit_config: Secret::new(secret),
        },
        limits: Limits {
            memory_bytes: 4_026_531_840,
            pids: 4096,
            shm_bytes: 1 << 30,
            cpus: 4,
        },
        env: vec![("CI".into(), "true".into())],
        oom_grace_ms: 15_000,
    }
}

fn usage() -> impl Strategy<Value = Usage> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u32>(),
    )
        .prop_map(
            |(memory_bytes, memory_peak_bytes, cpu_usec, disk_bytes, pids)| Usage {
                memory_bytes,
                memory_peak_bytes,
                cpu_usec,
                disk_bytes,
                pids,
            },
        )
}

fn process() -> impl Strategy<Value = Process> {
    (any::<u32>(), ".{0,20}", any::<u64>()).prop_map(|(pid, name, memory_bytes)| Process {
        pid,
        name,
        memory_bytes,
    })
}

fn stop_reason() -> impl Strategy<Value = StopReason> {
    prop_oneof![
        Just(StopReason::Cancelled),
        Just(StopReason::Timeout),
        Just(StopReason::EvictedMemory),
        Just(StopReason::EvictedDisk),
        Just(StopReason::HostSlept),
    ]
}

fn oom() -> impl Strategy<Value = OomEvent> {
    (process(), any::<u64>(), prop::collection::vec(process(), 0..6)).prop_map(
        |(process, limit_bytes, largest)| OomEvent {
            process,
            limit_bytes,
            largest,
        },
    )
}

fn guest_message() -> impl Strategy<Value = GuestMessage> {
    prop_oneof![
        (any::<u32>(), ".{0,40}").prop_map(|(protocol, kernel)| GuestMessage::Hello { protocol, kernel }),
        any::<u64>().prop_map(|boot_ms| GuestMessage::Ready { boot_ms }),
        usage().prop_map(GuestMessage::Heartbeat),
        oom().prop_map(GuestMessage::Oom),
        (".{0,12}", ".{0,200}").prop_map(|(stage, message)| GuestMessage::Failed { stage, message }),
        (
            prop_oneof![Just(GuestEnd::Exited), stop_reason().prop_map(GuestEnd::Stopped)],
            any::<Option<i32>>(),
            any::<Option<i32>>(),
            usage(),
            any::<u64>(),
            prop::option::of(oom()),
            any::<u64>()
        )
            .prop_map(|(end, exit_code, signal, usage, disk_peak_bytes, oom, wall_ms)| {
                GuestMessage::Exited(Summary {
                    end,
                    exit_code,
                    signal,
                    usage,
                    disk_peak_bytes,
                    oom,
                    wall_ms,
                })
            }),
    ]
}

fn host_message() -> impl Strategy<Value = HostMessage> {
    let task = prop_oneof![
        Just(Task::Probe),
        Just(Task::FormatScratch),
        ".{0,300}".prop_map(|secret| Task::Job(job(&secret))),
        prop::collection::vec(".{0,30}", 0..5).prop_map(|argv| {
            let mut job = job("");
            job.workload = Workload::Command { argv };
            Task::Job(job)
        }),
    ];
    prop_oneof![
        (any::<u32>(), any::<u64>(), task).prop_map(|(protocol, time_ns, task)| HostMessage::Start(Start {
            protocol,
            time_ns,
            task
        })),
        (stop_reason(), any::<u64>())
            .prop_map(|(reason, grace_ms)| HostMessage::Stop(Stop { reason, grace_ms })),
    ]
}

proptest! {
    #[test]
    fn every_guest_message_reads_back_as_written(messages in prop::collection::vec(guest_message(), 0..8)) {
        let mut wire = Vec::new();
        for message in &messages {
            write_message(&mut wire, message).unwrap();
        }
        let mut reader = Cursor::new(wire);
        for message in &messages {
            let read = read_message::<GuestMessage>(&mut reader).unwrap();
            prop_assert_eq!(read.as_ref(), Some(message));
        }
        prop_assert!(read_message::<GuestMessage>(&mut reader).unwrap().is_none());
    }

    #[test]
    fn every_host_message_reads_back_as_written(message in host_message()) {
        let mut reader = Cursor::new(encode(&message).unwrap());
        prop_assert_eq!(read_message::<HostMessage>(&mut reader).unwrap(), Some(message));
    }

    /// However the bytes are cut on their way, the decoder gives the same messages in the same order.
    #[test]
    fn the_decoder_gives_the_same_messages_however_the_bytes_are_cut(
        messages in prop::collection::vec(guest_message(), 1..6),
        cuts in prop::collection::vec(1usize..40, 1..40),
    ) {
        let wire: Vec<u8> = messages.iter().flat_map(|message| encode(message).unwrap()).collect();
        let mut decoder = Decoder::new();
        let mut seen = Vec::new();
        let mut rest = wire.as_slice();
        let mut cuts = cuts.iter().cycle();
        while !rest.is_empty() {
            let take = (*cuts.next().unwrap()).min(rest.len());
            decoder.push(&rest[..take]);
            rest = &rest[take..];
            while let Some(message) = decoder.take::<GuestMessage>().unwrap() {
                seen.push(message);
            }
        }
        prop_assert_eq!(seen, messages);
        prop_assert!(!decoder.mid_frame());
    }

    /// A hostile guest's bytes end in a message or an error, never in a panic or an endless buffer.
    #[test]
    fn arbitrary_bytes_never_panic_a_reader(bytes in prop::collection::vec(any::<u8>(), 0..600)) {
        let mut decoder = Decoder::new();
        decoder.push(&bytes);
        while let Ok(Some(_)) = decoder.take::<GuestMessage>() {}
        let mut reader = Cursor::new(bytes);
        while let Ok(Some(_)) = read_message::<HostMessage>(&mut reader) {}
    }
}

#[test]
fn a_frame_that_claims_more_than_the_limit_is_refused_before_it_is_buffered() {
    let prefix = ((MAX_FRAME + 1) as u32).to_be_bytes();
    let mut decoder = Decoder::new();
    decoder.push(&prefix);
    assert!(
        matches!(decoder.take::<GuestMessage>(), Err(ProtoError::TooLarge(size)) if size == MAX_FRAME + 1)
    );
    assert!(matches!(
        read_message::<GuestMessage>(&mut Cursor::new(prefix.to_vec())),
        Err(ProtoError::TooLarge(_))
    ));
}

#[test]
fn a_frame_of_exactly_the_limit_is_read_and_one_byte_more_is_not_written() {
    // A JSON string of MAX_FRAME bytes with its two quotes is two bytes over; without them it fits exactly.
    let fits = "x".repeat(MAX_FRAME - 2);
    let frame = encode(&fits).unwrap();
    assert_eq!(frame.len(), 4 + MAX_FRAME);
    assert_eq!(
        read_message::<String>(&mut Cursor::new(frame))
            .unwrap()
            .unwrap()
            .len(),
        MAX_FRAME - 2
    );
    assert!(matches!(
        encode(&"x".repeat(MAX_FRAME - 1)),
        Err(ProtoError::TooLarge(_))
    ));
}

#[test]
fn a_connection_cut_inside_a_frame_is_an_error_and_between_frames_is_the_end() {
    let frame = encode(&GuestMessage::Ready { boot_ms: 700 }).unwrap();
    for cut in 1..frame.len() {
        let result = read_message::<GuestMessage>(&mut Cursor::new(frame[..cut].to_vec()));
        assert!(
            matches!(result, Err(ProtoError::Truncated)),
            "cut at {cut}: {result:?}"
        );
        let mut decoder = Decoder::new();
        decoder.push(&frame[..cut]);
        assert!(decoder.take::<GuestMessage>().unwrap().is_none());
        assert!(decoder.mid_frame());
    }
    assert!(
        read_message::<GuestMessage>(&mut Cursor::new(Vec::new()))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_frame_that_is_no_message_is_refused_and_the_next_frame_is_still_read() {
    let mut wire = Vec::new();
    for body in [&b"{\"type\":\"reboot-the-host\"}"[..], b"\xff\xfe", b"[]", b""] {
        wire.extend_from_slice(&(body.len() as u32).to_be_bytes());
        wire.extend_from_slice(body);
    }
    wire.extend(encode(&GuestMessage::Ready { boot_ms: 1 }).unwrap());
    let mut decoder = Decoder::new();
    decoder.push(&wire);
    for _ in 0..4 {
        assert!(matches!(
            decoder.take::<GuestMessage>(),
            Err(ProtoError::Malformed(_))
        ));
    }
    assert_eq!(
        decoder.take::<GuestMessage>().unwrap(),
        Some(GuestMessage::Ready { boot_ms: 1 })
    );
}

#[test]
fn a_secret_never_shows_in_debug_output_and_arrives_whole() {
    let secret = "eyJ-one-job-configuration-0123456789";
    let start = HostMessage::Start(Start {
        protocol: PROTOCOL,
        time_ns: 1,
        task: Task::Job(job(secret)),
    });
    let shown = format!("{start:?} {:#?}", start);
    assert!(
        !shown.contains(secret) && !shown.contains("0123456789"),
        "{shown}"
    );
    let Some(HostMessage::Start(read)) =
        read_message::<HostMessage>(&mut Cursor::new(encode(&start).unwrap())).unwrap()
    else {
        panic!("not a start");
    };
    let Task::Job(Job {
        workload: Workload::ActionsRunner { jit_config },
        ..
    }) = read.task
    else {
        panic!("not a job");
    };
    assert_eq!(jit_config.expose(), secret);
}

/// The names on the wire are part of the protocol; a rename here is a protocol change.
#[test]
fn the_wire_names_are_the_documented_ones() {
    let stop = serde_json::to_string(&HostMessage::Stop(Stop {
        reason: StopReason::EvictedMemory,
        grace_ms: 20_000,
    }))
    .unwrap();
    assert_eq!(
        stop,
        r#"{"type":"stop","reason":"evicted-memory","grace_ms":20000}"#
    );
    assert_eq!(
        serde_json::to_string(&GuestEnd::Stopped(StopReason::HostSlept)).unwrap(),
        r#"{"kind":"stopped","reason":"host-slept"}"#
    );
    assert_eq!(
        serde_json::to_string(&Task::FormatScratch).unwrap(),
        r#"{"kind":"format-scratch"}"#
    );
}
