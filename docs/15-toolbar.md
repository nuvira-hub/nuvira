# NexoDesk — Session toolbar

[← Index](README.md)

## How it is built

Dark bar at the top centre of the session window (`client/viewer/src/toolbar.rs` and `toolbar.html`):

- it is a WebView (WebView2 on Windows, the wry library, the same one Tauri uses) that is a child of the session window. It works with both of the viewer's drawing modes (wgpu and CPU), and the HTML can be reused in the future web client;
- clicks reach the viewer as messages (`window.ipc.postMessage`), and the state goes back to the page with `nexodesk.update({...})`;
- after every click the keyboard goes back to the session; the mouse over the bar does not reach the remote PC;
- the labels come from `client/i18n` in the interface language (D15);
- WebView2 data is stored in `%LOCALAPPDATA%\NexoDesk\WebView2` (next to the program, for example in Program Files, it often cannot write).

## Content

| Item | Function | Where it acts |
|---|---|---|
| Dot, name, fps and latency | Green: direct connection; orange: through the server's relay | viewer |
| 1, 2, …, All | Screen to transmit (only if the agent has more than one); "All" streams every screen at once, laid out as in the remote virtual desktop (gaps black). Composed in RAM: on the NUC two 1080p screens make 3840x1080 at ~19 fps with software encoding; tested by the user from the laptop | agent: `SelectDisplay` (65535 = all) |
| Quality | Balanced → Sharp → Smooth at each click | agent: `SetQuality` |
| Ctrl+Alt+Del | Same as Ctrl+Alt+End (see [14-phase3-service.md](14-phase3-service.md)) | service |
| Eye | View only: mouse and keyboard are not sent | viewer |
| Chat, Send file | See [16-clipboard-chat-files.md](16-clipboard-chat-files.md) | viewer, agent |
| Real size | 1:1 instead of fitting the window; the image follows the mouse, so moving it from one edge of the window to the other crosses the whole remote screen. `--actual-size` starts in this mode | viewer |
| Full screen | Enters and leaves | viewer |
| Arrow | Shrinks the bar to a tab; a click opens it again | viewer |
| X | Closes the session | viewer |

## New control messages (`session.proto`)

- `Displays` (agent → viewer): available screens and the one being transmitted, at the start and after every change.
- `SelectDisplay` (viewer → agent): the agent stops capture and encoding, restarts them on the new screen with a keyframe, sends updated `VideoFormat` and `Displays`, and updates the input area. If the new screen cannot be opened it stays on the previous one.
- `SetQuality` (viewer → agent): changes the ceiling of the adaptive bitrate and the frame rate, without restarting anything.

| Quality | Maximum bitrate | Frames per second |
|---|---|---|
| Balanced (default) | the agent's (8 Mbit/s) | the agent's (60) |
| Sharp | double | at most 30 |
| Smooth | half | the agent's |

The bitrate stays adaptive: the quality only moves its ceiling.

## Shrinking

When the remote screen is larger than the window each window pixel averages several source samples (GPU: up to 4x4 in the shader; CPU: the brightness at the corners of the area it covers). A single sample per pixel skipped strokes and broke up thin text: the case that showed it was a 1280x1024 screen on a 1366x768 laptop, shrunk to 75%. Cost on the CPU path: 6 ms per 1080p frame on PC-LABORATORIO, as before.

## Tests of 2026-10-27

- Bar, Ctrl+Alt+Del, view only, full screen, shrink and close towards PC-LABORATORIO: tested by the user.
- Screen and quality change: session from the NUC (two screens) to the NUC itself in view-only mode, tested by the user.

## Related

A viewer killed abruptly left the agent "busy" until the QUIC timeout (20 s). Now the timeout is 10 s with a keep-alive packet every 2 s (`transport/src/lib.rs`).

## To do

- Drop-down menus (for example for quality) require temporarily enlarging the WebView: for now quality changes at each click.
