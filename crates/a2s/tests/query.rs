//! `query` against a fake game server on 127.0.0.1, which answers each datagram as a test says and keeps what it
//! was sent.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cf_a2s::{Info, Malformed, NAME_CHARS, QueryError, query, request};
use tokio::net::UdpSocket;

/// An info reply as a Source server sends it, with `tail` after the most players (bots, type and so on).
fn info_reply(name: &[u8], players: u8, max: u8) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFF, 0xFF, 0xFF, 0x49, 17];
    for field in [name, b"Meadows", b"valheim", b"Valheim"] {
        out.extend_from_slice(field);
        out.push(0);
    }
    out.extend_from_slice(&[0x9a, 0x03, players, max, 0, b'd', b'l', 0, 0]);
    out
}

/// A fake server: each datagram it gets is kept and answered with what `answer` makes of it, or nothing.
struct Fake {
    port: u16,
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Fake {
    async fn start(answer: impl Fn(&[u8]) -> Vec<Vec<u8>> + Send + 'static) -> Fake {
        let socket = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        let port = socket.local_addr().expect("address").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let kept = seen.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = socket.recv_from(&mut buf).await else {
                    return;
                };
                kept.lock().expect("seen").push(buf[..n].to_vec());
                for reply in answer(&buf[..n]) {
                    let _ = socket.send_to(&reply, from).await;
                }
            }
        });
        Fake { port, seen }
    }

    fn seen(&self) -> Vec<Vec<u8>> {
        self.seen.lock().expect("seen").clone()
    }
}

const QUICK: Duration = Duration::from_millis(400);

#[tokio::test]
async fn a_direct_info_reply_gives_the_name_and_the_players() {
    let fake = Fake::start(|_| vec![info_reply(b"Heimforge NA", 3, 10)]).await;
    let answer = query("127.0.0.1", fake.port, QUICK).await.expect("an answer");
    assert_eq!(
        answer.info,
        Info {
            name: "Heimforge NA".into(),
            players: 3,
            max: 10
        }
    );
    assert!(answer.latency < QUICK, "{:?}", answer.latency);
    assert_eq!(fake.seen(), vec![request(None)], "one query, and no other kind");
}

#[tokio::test]
async fn a_challenge_is_sent_back_once_and_then_answered() {
    let challenge = [0x12, 0x34, 0x56, 0x78];
    let fake = Fake::start(move |got| {
        if got == request(Some(challenge)).as_slice() {
            vec![info_reply(b"Heimforge EU", 0, 64)]
        } else if got == request(None).as_slice() {
            vec![[&[0xFF, 0xFF, 0xFF, 0xFF, 0x41][..], &challenge].concat()]
        } else {
            vec![]
        }
    })
    .await;
    let answer = query("127.0.0.1", fake.port, QUICK).await.expect("an answer");
    assert_eq!(
        answer.info,
        Info {
            name: "Heimforge EU".into(),
            players: 0,
            max: 64
        }
    );
    // Exactly the query and the query with the challenge: never A2S_PLAYER (0x55) or anything else.
    assert_eq!(fake.seen(), vec![request(None), request(Some(challenge))]);
    assert!(fake.seen().iter().all(|d| d[4] == 0x54));
}

#[tokio::test]
async fn a_second_challenge_ends_the_query() {
    let fake = Fake::start(|_| vec![vec![0xFF, 0xFF, 0xFF, 0xFF, 0x41, 1, 2, 3, 4]]).await;
    assert_eq!(
        query("127.0.0.1", fake.port, QUICK).await,
        Err(QueryError::ChallengedTwice)
    );
    assert_eq!(fake.seen().len(), 2, "no storm of retries");
}

#[tokio::test]
async fn a_server_that_says_nothing_times_out_on_time() {
    let fake = Fake::start(|_| vec![]).await;
    let started = Instant::now();
    assert_eq!(
        query("127.0.0.1", fake.port, QUICK).await,
        Err(QueryError::Timeout(QUICK))
    );
    let took = started.elapsed();
    assert!(took >= QUICK && took < QUICK * 3, "{took:?}");
    assert_eq!(fake.seen(), vec![request(None)], "asked once, never again");
}

#[tokio::test]
async fn malformed_replies_are_errors() {
    let cases: [(Vec<u8>, Malformed); 6] = [
        (vec![0xFF, 0xFF, 0xFF], Malformed::Short),
        (vec![0xFF, 0xFF, 0xFF, 0xFE, 0x49, 17, b'x', 0], Malformed::Header),
        (vec![0xFF, 0xFF, 0xFF, 0xFF, 0x44, 1], Malformed::Kind(0x44)),
        (
            vec![0xFF, 0xFF, 0xFF, 0xFF, 0x49, 17, b'H', b'e', b'i', b'm'],
            Malformed::Unended("name"),
        ),
        (vec![0xFF, 0xFF, 0xFF, 0xFF, 0x41, 1, 2], Malformed::Short),
        // Every string, then the id and players, but not the most players.
        (
            info_reply(b"Heimforge NA", 3, 10)[..6 + 13 + 8 + 8 + 8 + 3].to_vec(),
            Malformed::Short,
        ),
    ];
    for (reply, why) in cases {
        let sent = reply.clone();
        let fake = Fake::start(move |_| vec![sent.clone()]).await;
        assert_eq!(
            query("127.0.0.1", fake.port, QUICK).await,
            Err(QueryError::Malformed(why)),
            "{reply:02x?}"
        );
    }
}

#[tokio::test]
async fn an_oversize_name_is_clipped() {
    let long = "N".repeat(1200);
    let fake = Fake::start(move |_| vec![info_reply(long.as_bytes(), 1, 2)]).await;
    let answer = query("127.0.0.1", fake.port, QUICK).await.expect("an answer");
    assert_eq!(answer.info.name, "N".repeat(NAME_CHARS));
    assert_eq!((answer.info.players, answer.info.max), (1, 2));
}

#[tokio::test]
async fn control_and_direction_characters_never_reach_the_name() {
    let fake = Fake::start(|_| {
        vec![info_reply(
            "\u{1b}[31mHeim\u{7}forge\u{202e} NA\r\n".as_bytes(),
            2,
            10,
        )]
    })
    .await;
    let answer = query("127.0.0.1", fake.port, QUICK).await.expect("an answer");
    assert_eq!(answer.info.name, "[31mHeim forge  NA");
}

#[tokio::test]
async fn bad_targets_are_refused_unsent() {
    assert_eq!(
        query("a b", 2457, QUICK).await,
        Err(QueryError::Target("the host is not a name or an address"))
    );
    assert_eq!(
        query("-x", 2457, QUICK).await,
        Err(QueryError::Target("the host is not a name or an address"))
    );
    assert_eq!(
        query("", 2457, QUICK).await,
        Err(QueryError::Target("the host is not a name or an address"))
    );
    assert_eq!(
        query("127.0.0.1", 0, QUICK).await,
        Err(QueryError::Target("port 0"))
    );
}

#[tokio::test]
async fn a_port_nothing_listens_on_is_no_answer() {
    // A port just let go: the host answers the datagram with port unreachable, or says nothing.
    let free: SocketAddr = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind")
        .local_addr()
        .expect("address");
    match query("127.0.0.1", free.port(), QUICK).await {
        Err(QueryError::Io(_) | QueryError::Timeout(_)) => {}
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_name_with_no_address_ends_in_time() {
    let started = Instant::now();
    match query("no-such-host.invalid", 2457, QUICK).await {
        Err(QueryError::Resolve(_) | QueryError::Timeout(_)) => {}
        other => panic!("{other:?}"),
    }
    assert!(started.elapsed() < QUICK * 3);
}
