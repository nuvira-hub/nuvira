//! How a session ends: the viewer's close of the connection with code 0 is a normal
//! end, and the reason the viewer reads when the connection closes.

use std::time::Duration;

use anyhow::Result;
use nuvira_transport::Connection;

#[cfg(doc)]
use crate::Gate;

/// The viewer closed the session on purpose (QUIC close with code 0): not an error
/// for the session log.
pub(crate) fn closed_normally(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        let closed = |c: &quinn::ConnectionError| {
            matches!(c, quinn::ConnectionError::ApplicationClosed(a) if a.error_code == quinn::VarInt::from_u32(0))
        };
        cause.downcast_ref::<quinn::ConnectionError>().is_some_and(closed)
            || cause.downcast_ref::<quinn::ReadError>().is_some_and(|r| matches!(r, quinn::ReadError::ConnectionLost(c) if closed(c)))
            || cause.downcast_ref::<quinn::WriteError>().is_some_and(|w| matches!(w, quinn::WriteError::ConnectionLost(c) if closed(c)))
            || cause
                .downcast_ref::<std::io::Error>()
                .and_then(|io| io.get_ref())
                .and_then(|inner| inner.downcast_ref::<quinn::ReadError>())
                .is_some_and(|r| matches!(r, quinn::ReadError::ConnectionLost(c) if closed(c)))
            || cause
                .downcast_ref::<quinn::ReadExactError>()
                .is_some_and(|r| matches!(r, quinn::ReadExactError::ReadError(quinn::ReadError::ConnectionLost(c)) if closed(c)))
    })
}

/// How long a viewer that stopped reading the video is waited for to close the
/// connection (it does so at once: see [`video_stopped`]).
const STOP_THEN_CLOSE: Duration = Duration::from_secs(2);

/// The viewer stopped reading a stream with code 0 (quinn's STOP_SENDING when a
/// stream is dropped unread).
fn stopped_by_peer(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause.downcast_ref::<quinn::WriteError>().is_some_and(
            |w| matches!(w, quinn::WriteError::Stopped(code) if *code == quinn::VarInt::from_u32(0)),
        )
    })
}

/// Sending video failed with `e`. At the end of a session the viewer drops its video
/// stream (STOP_SENDING, code 0) just before it closes the connection, so the
/// agent's next write fails with "stopped by peer" before the close arrives: that
/// close, when it comes soon, is how the session ended (normally with code 0, see
/// [`closed_normally`]). Without one in [`STOP_THEN_CLOSE`] the stop stays the
/// error (review AGT-7).
pub(crate) async fn video_stopped(conn: &Connection, e: anyhow::Error) -> anyhow::Error {
    if !stopped_by_peer(&e) {
        return e;
    }
    match tokio::time::timeout(STOP_THEN_CLOSE, conn.closed()).await {
        Ok(closed) => anyhow::Error::new(closed).context("the viewer stopped the video"),
        Err(_) => e,
    }
}

/// A session the viewer closed on purpose ended normally: `Ok`, so it is neither
/// logged as a warning nor reported as an error (`SessionEnded`, session report).
pub(crate) fn normal_end(result: Result<()>) -> Result<()> {
    match result {
        Err(e) if closed_normally(&e) => Ok(()),
        r => r,
    }
}

/// What the viewer reads when the connection closes: the "@key" message of the
/// error that ended the session, if it has one (e.g. the screen of a Linux
/// computer is locked), translated on its side.
pub(crate) fn close_reason(result: &Result<()>) -> Vec<u8> {
    result
        .as_ref()
        .err()
        .and_then(|e| e.chain().map(|c| c.to_string()).find(|m| m.starts_with('@')))
        .map_or_else(|| b"end of session".to_vec(), String::into_bytes)
}

/// The close reason of a relayed session: [`close_reason`], or
/// [`ENDED_AT_COMPUTER`] when the person at the PC ended it and the viewer shows
/// that key (`Hello.end_reason`; older viewers get "end of session" as before).
/// Still a normal end (code 0, `Ok`): the viewer neither reports it as an error
/// nor opens the session again by itself.
pub(crate) fn end_reason(result: &Result<()>, at_computer_told: bool) -> Vec<u8> {
    match (result, at_computer_told) {
        (Ok(()), true) => ENDED_AT_COMPUTER.as_bytes().to_vec(),
        _ => close_reason(result),
    }
}

/// The person at the PC ended the session: the close reason of that end, and the
/// refusal of an automatic reopen after it ([`Gate::refuse_reopen`]).
pub(crate) const ENDED_AT_COMPUTER: &str = "@agent.ended_at_computer";

/// The end of the video stream (review AGT-7): the viewer's stop of the video
/// followed by its close with code 0 is a normal end; a stop without a close, or
/// a close with another code, stays an error.
#[cfg(test)]
mod video_end_tests {
    use super::*;
    use anyhow::anyhow;
    use nuvira_proto::v1::VideoFrame;
    use nuvira_transport::FramedSend;

    use crate::{Duration, Instant};

    /// The agent's and the viewer's sides of a loopback session, and the agent's
    /// video stream as each side holds it.
    async fn pair() -> (Connection, Connection, FramedSend, quinn::RecvStream) {
        let (endpoint, fingerprint) =
            nuvira_transport::server_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = endpoint.local_addr().unwrap();
        let agent = tokio::spawn(async move { endpoint.accept().await.unwrap().await.unwrap() });
        let viewer = nuvira_transport::connect(addr, Some(fingerprint)).await.unwrap();
        let agent = agent.await.unwrap();
        let mut video = FramedSend::new(agent.open_uni().await.unwrap());
        // A stream is seen by the other side only once written on.
        video.send(&VideoFrame::default()).await.unwrap();
        let rx = viewer.accept_uni().await.unwrap();
        (agent, viewer, video, rx)
    }

    /// The viewer stops reading as it does when its session ends (code 0); the
    /// agent's error once its writes fail.
    async fn stopped(video: &mut FramedSend, mut rx: quinn::RecvStream) -> anyhow::Error {
        rx.stop(0u32.into()).unwrap();
        let frame = VideoFrame { data: vec![7; 10_000], ..Default::default() };
        loop {
            if let Err(e) = video.send(&frame).await {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn a_stop_then_the_viewers_close_is_a_normal_end() {
        let (agent, viewer, mut video, rx) = pair().await;
        let e = stopped(&mut video, rx).await;
        assert!(stopped_by_peer(&e), "{e:#}");
        assert!(!closed_normally(&e), "the stop alone is not the close: {e:#}");
        let close = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            viewer.close(0u32.into(), b"end of session");
            viewer
        });
        let e = video_stopped(&agent, e).await;
        assert!(closed_normally(&e), "{e:#}");
        assert!(normal_end(Err(e.context("sending video"))).is_ok());
        drop(close.await);
    }

    #[tokio::test]
    async fn a_stop_without_a_close_stays_an_error() {
        let (agent, _viewer, mut video, rx) = pair().await;
        let e = stopped(&mut video, rx).await;
        let start = Instant::now();
        let e = video_stopped(&agent, e).await;
        assert!(start.elapsed() >= STOP_THEN_CLOSE);
        assert!(stopped_by_peer(&e) && !closed_normally(&e), "{e:#}");
        assert!(normal_end(Err(e)).is_err());
    }

    #[tokio::test]
    async fn a_stop_then_a_close_with_another_code_stays_an_error() {
        let (agent, viewer, mut video, rx) = pair().await;
        let e = stopped(&mut video, rx).await;
        viewer.close(1u32.into(), b"failed");
        let e = video_stopped(&agent, e).await;
        assert!(!closed_normally(&e), "{e:#}");
        assert!(format!("{e:#}").contains("the viewer stopped the video"), "{e:#}");
    }

    #[tokio::test]
    async fn other_errors_are_kept_at_once() {
        let (agent, _viewer, _video, _rx) = pair().await;
        let stop_with_code = |code: u32| anyhow::Error::new(quinn::WriteError::Stopped(code.into()));
        assert!(stopped_by_peer(&stop_with_code(0)));
        assert!(!stopped_by_peer(&stop_with_code(3)));
        let start = Instant::now();
        let e = video_stopped(&agent, stop_with_code(3)).await;
        assert!(format!("{e}").contains("error 3"), "{e:#}");
        let e = video_stopped(&agent, anyhow!("encoder failed")).await;
        assert_eq!(format!("{e}"), "encoder failed");
        assert!(start.elapsed() < STOP_THEN_CLOSE);
    }
}
