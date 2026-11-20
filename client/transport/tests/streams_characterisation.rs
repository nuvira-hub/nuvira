//! Characterisation tests (Phase 2 of the full review) of the readers of framed
//! messages on QUIC streams, over a real QUIC connection on the loopback:
//! `recv_one`, `FramedRecv`, `read_stream`, `send_stream` and the nested-QUIC
//! frames (`nested::read_frame` / `write_frame`). They pin the CURRENT behaviour.

use std::net::SocketAddr;
use std::time::Duration;

use nuvira_proto::framing::MAX_MESSAGE_LEN;
use nuvira_proto::v1::{FileChunk, Ping};
use nuvira_transport::{
    Connection, FramedRecv, nested, read_stream, recv_one, recv_one_with_limit, send_stream,
};
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
    // With a limit of its own (review NET-3): refused above it, before reading the body.
    let limit = nuvira_transport::STREAM_HEADER_LIMIT;
    let mut rx = uni(&c, &s, &varint(limit as u64 + 1)).await;
    let err = recv_one_with_limit::<Ping>(&mut rx, limit).await.unwrap_err().to_string();
    assert_eq!(err, format!("message too large: {}", limit + 1));
    let mut rx = uni(&c, &s, &framed(&Ping { sent_us: 5 })).await;
    assert_eq!(recv_one_with_limit::<Ping>(&mut rx, limit).await.unwrap(), Some(Ping { sent_us: 5 }));
}

#[tokio::test]
async fn framed_recv_limit_can_be_raised() {
    // Review NET-3: a stream read with a small limit (before authentication),
    // raised afterwards.
    let (c, s, _e) = pair().await;
    let big = FileChunk { data: vec![1; 70 * 1024], ..Default::default() };
    let mut wire = framed(&Ping { sent_us: 1 });
    wire.extend(framed(&big));
    let mut rx = FramedRecv::with_limit(uni(&c, &s, &wire).await, 64 * 1024);
    assert_eq!(rx.recv::<Ping>().await.unwrap(), Some(Ping { sent_us: 1 }));
    rx.set_limit(MAX_MESSAGE_LEN);
    assert_eq!(rx.recv::<FileChunk>().await.unwrap().map(|m| m.data.len()), Some(70 * 1024));
    // Not raised: refused.
    let mut rx = FramedRecv::with_limit(uni(&c, &s, &framed(&big)).await, 64 * 1024);
    let err = rx.recv::<FileChunk>().await.unwrap_err().to_string();
    assert!(err.contains("message too large"), "{err}");
}

#[tokio::test]
async fn recv_one_follows_the_decoder_varint_rules() {
    // Was K5 (Phase 2): `recv_one` read at most 10 length bytes and never checked
    // the 10th: ten bytes with the continuation bit gave length 0 (the 11th byte
    // left on the stream), and the bits of the 10th byte above the 64th were
    // shifted out (0x02 << 63 = 0). Fixed in review NET-5: both are refused with
    // the error `framing::Decoder` gives on the same bytes.
    let (c, s, _e) = pair().await;
    for bytes in [[vec![0x80u8; 10], vec![0xAA]].concat(), [vec![0x80u8; 9], vec![0x02]].concat()] {
        let mut dec = nuvira_proto::framing::Decoder::default();
        dec.push(&bytes);
        let decoder_err = dec.next_message::<Ping>().unwrap_err().to_string();
        let mut rx = uni(&c, &s, &bytes).await;
        let err = recv_one::<Ping>(&mut rx).await.unwrap_err().to_string();
        assert_eq!(err, decoder_err, "{bytes:x?}");
    }
    // The longest valid prefix (10 bytes, u64::MAX) is a length, refused by the limit.
    let mut rx = uni(&c, &s, &varint(u64::MAX)).await;
    let err = recv_one::<Ping>(&mut rx).await.unwrap_err().to_string();
    assert_eq!(err, format!("message too large: {}", u64::MAX));
    // A non-minimal but valid prefix (0x82 0x00 = 2) still reads as the Decoder reads it.
    let mut wire = vec![0x82, 0x00];
    wire.extend(Ping { sent_us: 7 }.encode_to_vec());
    assert_eq!(wire.len(), 4);
    let mut dec = nuvira_proto::framing::Decoder::default();
    dec.push(&wire);
    assert_eq!(dec.next_message::<Ping>().unwrap(), Some(Ping { sent_us: 7 }));
    wire.extend_from_slice(b"rest");
    let mut rx = uni(&c, &s, &wire).await;
    assert_eq!(recv_one::<Ping>(&mut rx).await.unwrap(), Some(Ping { sent_us: 7 }));
    assert_eq!(rx.read_to_end(10).await.unwrap(), b"rest");
}

#[tokio::test]
async fn framed_recv_refuses_a_truncated_last_message() {
    // Was K6 (Phase 2): `FramedRecv::recv` returned `Ok(None)`, a clean end, when
    // the stream ended inside a message. Fixed in review NET-4: an error, as
    // `recv_one` gives.
    let (c, s, _e) = pair().await;
    let mut wire = framed(&Ping { sent_us: 1 });
    wire.extend_from_slice(&framed(&Ping { sent_us: 2 })[..2]);
    let mut rx = FramedRecv::new(uni(&c, &s, &wire).await);
    assert_eq!(rx.recv::<Ping>().await.unwrap(), Some(Ping { sent_us: 1 }));
    let err = rx.recv::<Ping>().await.unwrap_err().to_string();
    assert_eq!(err, "stream ended inside a message");
    // Inside the length prefix too.
    let mut rx = FramedRecv::new(uni(&c, &s, &[0x80]).await);
    let err = rx.recv::<Ping>().await.unwrap_err().to_string();
    assert_eq!(err, "stream ended inside a message");
    // A clean end after whole messages is still `None`.
    let mut rx = FramedRecv::new(uni(&c, &s, &framed(&Ping { sent_us: 3 })).await);
    assert_eq!(rx.recv::<Ping>().await.unwrap(), Some(Ping { sent_us: 3 }));
    assert_eq!(rx.recv::<Ping>().await.unwrap(), None);
    // A length above the limit is an error, as before.
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
