//! The remote computer's sound, played here: its player and, when the video is on its
//! stream, the datagrams that carry it.

use anyhow::Result;
use tracing::info;

/// The sound when the video is on its stream: every datagram is a packet of it.
pub(super) async fn receive_audio(
    conn: &nuvira_transport::Connection,
    (player, tap): (&nuvira_audio::player::Player, &crate::recorder::AudioTap),
) -> Result<()> {
    info!("receiving the sound");
    loop {
        let packet = match conn.read_datagram().await {
            Ok(p) => p,
            Err(quinn::ConnectionError::ApplicationClosed(_) | quinn::ConnectionError::LocallyClosed) => {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        match nuvira_audio::packet::decode(&packet) {
            Ok((seq, data)) => {
                player.push(seq, data);
                tap.push(seq, data);
            }
            Err(e) => tracing::debug!("datagram dropped: {e:#}"),
        }
    }
}

/// The player of the remote computer's sound: none for terminals, with
/// NUVIRA_NO_AUDIO, or without an output device. NUVIRA_AUDIO_DUMP=file.wav also
/// writes what is received (automated tests).
pub(super) fn start_player(terminal_only: bool) -> Option<nuvira_audio::player::Player> {
    if terminal_only || std::env::var_os("NUVIRA_NO_AUDIO").is_some() {
        return None;
    }
    let dump = std::env::var_os("NUVIRA_AUDIO_DUMP").map(std::path::PathBuf::from);
    match nuvira_audio::player::Player::start(dump) {
        Ok(p) => Some(p),
        Err(e) => {
            info!("no sound: {e:#}");
            None
        }
    }
}
