# NuviraHub — End sessions without activity

[← Index](README.md)

A session ends by itself when nothing comes from the technician for a while:
keyboard and mouse, files moving, the viewer's own requests (clipboard, chat,
screens, privacy mode…). Its periodic messages (feedback, pings, the window's
size, keyframes) do not count.

- **At the computer**: app, Settings, "End sessions without activity": never,
  5, 10, 15, 30 or 60 minutes (`permissions.json`, `AgentCommand::IdleMinutes`).
- **NuviraHub Pro**: the rule "Minutes without activity before a session ends"
  (`Policy.idle_minutes`, 0 none) in the web console and the app's rules editor.
  With both, the shorter holds. Not among the limits of single accounts (those
  are switches, not numbers).

One minute before the end the agent sends `IdleWarning { seconds }`: the session
window shows it in its bar ("No activity: the session ends in 59 s. Move the
mouse over this window to stay connected") and asks for attention. Moving the
mouse over the window, a key or a click on the bar answers with `Activity`
(also in a view-only session, which sends no input), and the agent sends
`IdleWarning { seconds: 0 }` when the warning is over. At the end the agent
closes the connection with "@agent.idle_ended"; the viewer shows that reason
(it now shows any reason the agent closes a running session with).

Not for guests who joined a session (the first viewer's session counts) nor for
terminal-only sessions. `Hello.idle_limit` tells the viewer the session has a
limit.

## Tests of 2026-11-15

Limit of 2 minutes in the test agent's `permissions.json` (Mint VM, its own data
folder), viewer in view-only mode:

- Mint's viewer, nobody touching it: warning after 60 s, session ended after
  120 s, the viewer showed "session ended after a time without activity" (in
  Italian).
- Mint's viewer, the mouse moved over the window during the warning (`xdotool`):
  `Activity` sent, a new warning 60 s after the movement, the end 60 s later.
- PC-LABORATORIO's viewer: its USB mouse mover moved the pointer over the window
  within a second of each warning, so the session went on (as it would with a
  person there).

The web client shows the warning as a message at the bottom; its mouse over the
screen is input, which counts.

- Pro rule end to end: 2 minutes set in the web console of the Pro test server,
  the Mint's test agent registered there without a setting of its own: the
  session ended after 120 s with the reason, then the computer was locked (the
  rule "lock at the end" was on too).

On the Mac mini M1 (Scaleway, macOS 27.2.1, 2026-11-16): the app's setting
"After 5 minutes" saved for the LaunchAgent's agent (`permissions.json`,
`idle_minutes: 5`; the app gives it again when it connects to the agent, so it
wins over the file); Mint's viewer, view-only, nobody touching it: the warning at
00:19:22 ("ends in 59 s"), the end at 00:20:22, 5 minutes after the start, with
"session ended after a time without activity".

Not tried yet: the web client.
