# NexoDesk — Clipboard, chat and files

[← Index](README.md)

Last part of phase 2 (Windows MVP).

## Shared clipboard (text and images)

During the session text and images copied on one of the two PCs can be pasted on the other, in both directions and with no commands.

- `client/clipboard` (`nexodesk-clipboard`, arboard library): one thread per side checks the clipboard every 400 ms. On Windows it detects a change from the sequence number (`GetClipboardSequenceNumber`) without reading the clipboard; elsewhere it compares the text.
- New text travels on the control channel (`ControlMessage.clipboard`, at most 1 MB) and the other side writes it to its clipboard. Received text is not sent back.
- Only what is copied during the session counts: content already present at start-up is not sent.
- The agent shares it only if it can control the PC (not with `--view-only`). As SYSTEM, in the user's session, it uses the user's clipboard (it belongs to the `winsta0` window station).

Test of 2026-10-27: NUC ↔ PC-LABORATORIO (service agent), copy and paste in both directions, tested by the user.

Images (2026-10-27): copied images travel as PNG (fast compression, at most 8192x8192 pixels and 32 MB) on their own unidirectional stream, like files (`FileChunk.clipboard_image`), so a large image does not hold up the control channel; echo is avoided with a hash of the pixels. Not yet tested by the user between two PCs.

## Chat

Text chat between the person in control and the person using the PC.

- Protocol: `ControlMessage.chat` on the control channel, in both directions.
- Viewer: speech-bubble button in the bar; the panel (`chat.html`) is a second WebView on the right, under the bar. It opens by itself when a message arrives. While it is open the keyboard is used for typing; a click on the image gives it back to the session.
- On the controlled PC the chat is in the NexoDesk app, in the chat section next to the session strip: an incoming message brings the app to the front with the cursor in the reply field. It works with the app's agent and with the service's (`chat` events and `chat` command on the local channel, see [14-phase3-service.md](14-phase3-service.md)).
- The chat stays visible in the app after the session closes, until the next session. It is not saved.
- The command-line agent, without the app, writes received messages to the log.

Test of 2026-10-27: a message from the NUC arrived in the app on PC-LABORATORIO (service), the reply from there arrived in the viewer's panel.

## Sending files to the remote PC

First version, chosen with the user: the person in control sends files to the remote PC. The full two-way file manager will come later.

- **Viewer**: drag files onto the session window, or use the bar's "Send file" button (file dialog, rfd library, on its own thread). Files are sent one at a time. In the bar, in place of fps and latency, the progress appears, then the outcome, which also stays in the chat panel. For tests: `--send-file FILE` sends as soon as connected.
- **Protocol**: each file has its own unidirectional QUIC stream, opened by the viewer after the input stream. The first `FileChunk` carries name and size, the following ones the data (256 KB each); the end of the stream closes the file. The agent replies with `ControlMessage.file_received` (outcome and folder, or the reason for the error).
- **Agent** (`client/agent/src/files.rs`): saves to the logged-in user's Downloads folder. As SYSTEM (service) it gets it from the session user's token (`WTSQueryUserToken` + `SHGetKnownFolderPath`); with no user logged in it refuses the file. It first writes `name.part` and renames it only if the size matches; if the name exists it adds " (1)", " (2)"…; it keeps only the last part of the name, without folders or invalid characters. With `--view-only` it accepts no files.
- **On the controlled PC** the app notifies in the chat panel ("You received the file … in the Downloads folder") and comes to the front.

Tests of 2026-10-27, NUC → PC-LABORATORIO (service agent):

- random 20 MB file in about 0.7 s (about 230 Mbit/s in a LAN), identical SHA-256 on both PCs;
- a second send of the same file saved as "name (1).bin"; the file written by SYSTEM inherits the permissions of the Downloads folder, so the user has full control over it;
- drag and drop and the "Send file" button tested by the user.

## File manager (both directions, resumable)

The "File" button of the toolbar (shown when the agent says it supports it, `Hello.file_manager`) opens a second window (`client/viewer/src/filemanager.rs` and `filemanager.html`): this PC on the left, the remote one on the right, starting from known folders (Desktop, Documents, Downloads) and drives. Select files (Ctrl+click for several), open a folder on the other side, then "Send →" or "← Download". Transfers are listed at the bottom with their progress; an interrupted one has "Resume".

- **Protocol** (`session.proto`): `FsRequest` on the control channel (`list`, `download` from an offset, `upload_offset` = how much of a file an interrupted upload left), answered by `FsResponse`. Data travels as for "Send files", on unidirectional streams of `FileChunk`, now carrying `transfer_id`, destination `dir` and `offset`; downloads use streams from the agent.
- **Resume**: the receiving side writes `name.part` and renames it only when complete, so an interrupted transfer leaves the part; "Resume" asks how much is there (upload) or measures it (download) and sends only the rest. A part that does not have exactly the expected size is refused.
- **As the user**: on the agent of the service (SYSTEM) every file operation runs on a thread that impersonates the user signed in on the PC (`ImpersonateLoggedOnUser` with the session token): the person in control sees and writes only what that user could. Without a user signed in it is not available.
- Automated tests: `nexodesk viewer --download REMOTE_FILE --download-dir DIR` and `--upload FILE --upload-dir REMOTE_DIR`, with `--resume`; the viewer exits when the transfers are over.

Tests of 2026-10-27, NUC → PC-LABORATORIO (service) through the NAS server: 30 MB upload (same SHA-256; the file is owned by the user, not SYSTEM), upload resumed after 10 MB already there, 25 MB download, download resumed after 8 MB, the service configuration (readable only by SYSTEM) refused with "access denied"; window with both panes opened from the toolbar.

To do: sending whole folders, creating, renaming and deleting files and folders.
