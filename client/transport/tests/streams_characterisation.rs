//! Characterisation tests (Phase 2 of the full review) of the readers of framed
//! messages on QUIC streams, over a real QUIC connection on the loopback:
//! `recv_one`, `FramedRecv`, `read_stream`, `send_stream` and the nested-QUIC
//! frames (`nested::read_frame` / `write_frame`). They pin the CURRENT behaviour.

use std::net::SocketAddr;
use std::time::Duration;

use nuvira_proto::framing::MAX_MESSAGE_LEN;
use nuvira_proto::v1::{FileChunk, Ping};
use nuvira_transport::{Connection, FramedRecv, nested, read_stream, recv_one, send_stream};
use prost::Message;

/// A QUIC connection on the loopback: (client side, server side).
async fn pair() -> (Connection, Connection, quinn::Endpoint) {
    let (server, fp) =
        nuvira_transport::server_endpoint("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let addr = server.local_addr().unwrap();
    let accept = {
        let server = server.clone();
        tokio::spawn(async move { server.accept().await.unwrap().await.unwrap() })
    };
    let client = nuvira_transport::connect(addr, Some(fp)).await.unwrap();
    let server_conn = accept.await.unwrap();
    (client, server_conn, server)
}

/// Sends `bytes` on a new uni stream and finishes it; returns the receiving side.
async fn uni(client: &Connection, server: &Connection, bytes: &[u8]) -> quinn::RecvStream {
    let mut send = client.open_uni().await.unwrap();
    send.write_all(bytes).await.unwrap();
    send.finish().unwrap();
    tokio::time::timeout(Duration::from_secs(10), server.accept_uni()).await.unwrap().unwrap()
}

fn framed(msg: &impl Message) -> Vec<u8> {
    nuvira_proto::framing::encode(msg)
}

fn varint(n: u64) -> Vec<u8> {
    let mut v = Vec::new();
    prost::encoding::encode_varint(n, &mut v);
    v
}

#[tokio::test]
async fn recv_one_reads_one_message_and_leaves_the_rest() {
    let (c, s, _e) = pair().await;
    let mut wire = framed(&Ping { sent_us: 42 });
    wire.extend_from_slice(b"raw tunnel bytes");
    let mut rx = uni(&c, &s, &wire).await;
    assert_eq!(recv_one::<Ping>(&mut rx).await.unwrap(), Some(Ping { sent_us: 42 }));
    assert_eq!(rx.read_to_end(1000).await.unwrap(), b"raw tunnel bytes");
}

#[tokio::test]
async fn recv_one_end_of_stream_cases() {
    let (c, s, _e) = pair().await;
    // Nothing at all: None.
    let mut rx = uni(&c, &s, &[]).await;
    assert_eq!(recv_one::<Ping>(&mut rx).await.unwrap(), None);
    // Inside the length.
    let mut rx = uni(&c, &s, &[0x80]).await;
    let err = recv_one::<Ping>(&mut rx).await.unwrap_err().to_string();
    assert_eq!(err, "stream ended inside a message");
    // Inside the body.
    let mut rx = uni(&c, &s, &[5, 8, 1]).await;
    assert!(recv_one::<Ping>(&mut rx).await.is_err());
    // A body that does not decode.
    let mut rx = uni(&c, &s, &[1, 0x0f]).await;
    assert!(recv_one::<Ping>(&mut rx).await.is_err());
}

#[tokio::test]
async fn recv_one_size_limit() {
    let (c, s, _e) = pair().await;
    let mut rx = uni(&c, &s, &varint(MAX_MESSAGE_LEN as u64 + 1)).await;
    let err = recv_one::<Ping>(&mut rx).await.unwrap_err().to_string();
    assert_eq!(err, format!("message too large: {}", MAX_MESSAGE_LEN + 1));
    // Exactly the limit: accepted, then the body is missing.
    let mut rx = uni(&c, &s, &varint(MAX_MESSAGE_LEN as u64)).await;
    let err = recv_one::<Ping>(&mut rx).await.unwrap_err().to_string();
    assert!(!err.contains("too large"), "{err}");
}

#[tokio::test]
async fn characterises_bug_recv_one_accepts_varints_the_decoder_refuses() {
    // KNOWN BUG (Phase 2, lead transport/src/lib.rs:336-349): `recv_one` reads at
    // most 10 length bytes and never checks the 10th: ten bytes with the
    // continuation bit give length 0 (the 11th byte is left on the stream), and the
    // bits of the 10th byte above the 64th are shifted out (0x02 << 63 = 0).
    // `framing::Decoder` refuses both. Expected after fix: the same varint rules
    // (and the same error) as the Decoder / prost.
    let (c, s, _e) = pair().await;
    let mut wire = vec![0x80; 10];
    wire.push(0xAA);
    let mut rx = uni(&c, &s, &wire).await;
    assert_eq!(recv_one::<Ping>(&mut rx).await.unwrap(), Some(Ping::default()));
    assert_eq!(rx.read_to_end(10).await.unwrap(), [0xAA]);

    let mut wire = vec![0x80; 9];
    wire.push(0x02);
    let mut rx = uni(&c, &s, &wire).await;
    assert_eq!(recv_one::<Ping>(&mut rx).await.unwrap(), Some(Ping::default()));

    // The Decoder on the same bytes: errors.
    for bytes in [vec![0x80u8; 10], [vec![0x80u8; 9], vec![0x02]].concat()] {
        let mut dec = nuvira_proto::framing::Decoder::default();
        dec.push(&bytes);
        assert!(dec.next_message::<Ping>().is_err());
    }
}

#[tokio::test]
async fn characterises_framed_recv_drops_a_truncated_last_message() {
    // KNOWN BUG (Phase 2): `FramedRecv::recv` returns `Ok(None)` (a clean end) when
    // the stream ends inside a message: the partial message is dropped without an
    // error. Callers that count bytes (read_stream, files) notice it from the size;
    // others do not. Expected after fix: an error ("stream ended inside a message").
    let (c, s, _e) = pair().await;
    let mut wire = framed(&Ping { sent_us: 1 });
    wire.extend_from_slice(&framed(&Ping { sent_us: 2 })[..2]);
    let mut rx = FramedRecv::new(uni(&c, &s, &wire).await);
    assert_eq!(rx.recv::<Ping>().await.unwrap(), Some(Ping { sent_us: 1 }));
    assert_eq!(rx.recv::<Ping>().await.unwrap(), None);
    // A length above the limit is an error, though.
    let mut rx = FramedRecv::new(uni(&c, &s, &varint(MAX_MESSAGE_LEN as u64 + 1)).await);
    assert!(rx.recv::<Ping>().await.is_err());
}

#[tokio::test]
async fn framed_recv_many_messages() {
    let (c, s, _e) = pair().await;
    let wire: Vec<u8> = (0..5000u64).flat_map(|i| framed(&Ping { sent_us: i })).collect();
    let mut rx = FramedRecv::new(uni(&c, &s, &wire).await);
    for i in 0..5000u64 {
        assert_eq!(rx.recv::<Ping>().await.unwrap(), Some(Ping { sent_us: i }));
    }
    assert_eq!(rx.recv::<Ping>().await.unwrap(), None);
}

/// Sends a header and then data chunks as given; returns what `read_stream` says.
async fn read_with(header: FileChunk, chunks: &[&[u8]], limit: u64) -> anyhow::Result<Vec<u8>> {
    let (c, s, _e) = pair().await;
    let mut wire = framed(&header);
    for chunk in chunks {
        wire.extend(framed(&FileChunk { data: chunk.to_vec(), ..Default::default() }));
    }
    let mut rx = FramedRecv::new(uni(&c, &s, &wire).await);
    let first: FileChunk = rx.recv().await.unwrap().unwrap();
    read_stream(&mut rx, &first, limit).await
}

#[tokio::test]
async fn read_stream_checks_size_and_limit() {
    let header = |size: u64, data: &[u8]| FileChunk { size, data: data.to_vec(), ..Default::default() };
    assert_eq!(read_with(header(6, b"ab"), &[b"cd", b"ef"], 6).await.unwrap(), b"abcdef");
    assert_eq!(read_with(header(0, b""), &[], 0).await.unwrap(), b"");
    let err = read_with(header(7, b""), &[], 6).await.unwrap_err().to_string();
    assert_eq!(err, "7 bytes, more than the 6 accepted");
    let err = read_with(header(3, b""), &[b"ab", b"cd"], 10).await.unwrap_err().to_string();
    assert_eq!(err, "more data than announced");
    let err = read_with(header(5, b""), &[b"ab"], 10).await.unwrap_err().to_string();
    assert_eq!(err, "incomplete: received 2 bytes out of 5");
    // Data in the header beyond the announced size: noticed only at the end.
    let err = read_with(header(1, b"abc"), &[], 10).await.unwrap_err().to_string();
    assert_eq!(err, "incomplete: received 3 bytes out of 1");
}

#[tokio::test]
async fn send_stream_and_read_stream_round_trip() {
    let (c, s, _e) = pair().await;
    let data: Vec<u8> = (0..1_000_000u32).map(|i| (i % 253) as u8).collect();
    let header = FileChunk { name: "x.png".into(), ..Default::default() };
    let sender = {
        let c = c.clone();
        let data = data.clone();
        tokio::spawn(async move { send_stream(&c, header, &data).await })
    };
    let rx = s.accept_uni().await.unwrap();
    let mut rx = FramedRecv::new(rx);
    let first: FileChunk = rx.recv().await.unwrap().unwrap();
    assert_eq!((first.name.as_str(), first.size, first.data.len()), ("x.png", 1_000_000, 0));
    assert_eq!(read_stream(&mut rx, &first, 1_000_000).await.unwrap(), data);
    sender.await.unwrap().unwrap();
}

#[tokio::test]
async fn nested_frames() {
    let (c, s, _e) = pair().await;
    let (mut send, _r) = c.open_bi().await.unwrap();
    nested::write_frame(&mut send, b"one").await.unwrap();
    nested::write_frame(&mut send, b"").await.unwrap();
    nested::write_frame(&mut send, &vec![7u8; 65535]).await.unwrap();
    let err = nested::write_frame(&mut send, &vec![0u8; 65536]).await.unwrap_err().to_string();
    assert_eq!(err, "datagram too large");
    send.finish().unwrap();
    let (_s2, mut recv) = s.accept_bi().await.unwrap();
    assert_eq!(nested::read_frame(&mut recv).await.unwrap().as_deref(), Some(&b"one"[..]));
    assert_eq!(nested::read_frame(&mut recv).await.unwrap().as_deref(), Some(&b""[..]));
    assert_eq!(nested::read_frame(&mut recv).await.unwrap().map(|f| f.len()), Some(65535));
    assert_eq!(nested::read_frame(&mut recv).await.unwrap(), None);
}

#[tokio::test]
async fn nested_frames_truncated() {
    let (c, s, _e) = pair().await;
    // One byte of the length, then the end: an error, not the end.
    for wire in [&[0u8][..], &[0, 5, 1, 2][..]] {
        let (mut send, _r) = c.open_bi().await.unwrap();
        send.write_all(wire).await.unwrap();
        send.finish().unwrap();
        let (_s2, mut recv) = s.accept_bi().await.unwrap();
        assert!(nested::read_frame(&mut recv).await.is_err(), "{wire:?}");
    }
}
