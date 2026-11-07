# NuviraHub — Remote printing

[← Index](README.md)

A document of the controlled computer printed on the technician's printer: an invoice the customer needs on paper, a report generated on the remote PC, a page of a program that exists only there.

- **On the controlled computer (Windows, Linux, macOS).** A printer named "NuviraHub" appears among the others. During a session, whatever is printed on it, from any program, goes to the technician as a PDF. Without a session nothing arrives anywhere (see "Without a session").
- **At the technician's (Windows, Linux, macOS).** Every document opens the "Print" window: its title and size, the printer (the default one already chosen), "Print", "Save as PDF…" and "Don't print". With "From now on print on this printer without asking" the next documents go straight to that printer, with a notice in the session window; the app gives the choice back in Settings → Printing → "Ask every time". Closing the window prints none of the documents waiting in it. Printers that would open a dialog of their own (Microsoft Print to PDF, XPS, Fax: ports `PORTPROMPT:` and `SHRFAX:`) and this computer's own NuviraHub printer are not listed: "Save as PDF…" does the same.
- **Not allowing it at the PC.** App settings → "This computer" → "Allow remote printing" (on by default), kept like the other permissions (`app.json` `no_print`, `permissions.json` `print` in the agent's data folder, `AgentCommand::AllowPrint`). Not allowed: the agent does not listen; turned off during a session, the documents printed from then on are not sent.
- **Forbidding it.** On a NuviraHub Pro server, rule "No remote printing" in the web console (Rules) and in the app (Administration), `Policy.no_print`.

## How it works

**The printer.** `nuvira service install` (the setup, the app's "unattended access", automatic updates) adds it; `nuvira service uninstall` removes it; the hidden `nuvira service printer [--remove]` does only that. It is Windows' own "Microsoft Print To PDF" driver (an optional feature present by default in Windows 10 and 11; without it the installation says so and goes on without printer) on a local port that is the named pipe `\\.\pipe\nuvira-print`. No driver of ours, nothing loaded into the spooler: the spooler opens the port for every job and writes the PDF the driver makes into it. The local port monitor accepts a pipe as a port only if it can open it at that moment, so the installation keeps an instance of the pipe open while it adds the port (`XcvData AddPort`, then `AddPrinter`). `client/service/src/printer.rs`.

**The agent.** A session prints when the viewer says it can (`Hello.print` from the viewer) and nothing forbids it (the person at the PC, the server's rules, view-only mode, synthetic image, terminal-only session). The agent then creates the pipe (`first_pipe_instance`: if the agent of another Windows session already listens, this one does not offer printing) with access for SYSTEM, administrators and signed-in users, and answers `Hello.print`. For every job: it reads the PDF to the end (at most 512 MB), takes its title and submission time from the spooler's queue (`EnumJobs`, the job being printed), and sends it on a unidirectional stream with `FileChunk.print` and the title in `name`. The pipe exists only during the session. `client/agent/src/printer.rs`.

**Linux and macOS: a CUPS queue.** The queue "NuviraHub" (device URI `nuvira:/`, not shared, error policy `abort-job`) has a PPD of ours (`client/cups/src/nuvira.ppd`) whose only filter line is `application/vnd.cups-pdf application/pdf 0 -`: CUPS turns whatever is printed into PDF (`pdftopdf` of cups-filters on Linux, `cgpdftopdf` on macOS; text and PostScript first through their own filters) and hands it to our backend, `nuvira-cups` (crate `client/cups`, std and libc only), copied as `nuvira` into CUPS's backend folder (`/usr/lib/cups/backend`, `/usr/libexec/cups/backend` on macOS) with mode 0700, so that CUPS runs it as root. The backend gets the job's user and title on its command line and the PDF on its input, and hands it to the agent of **that user**:
- Linux: TCP on `127.0.0.1:21190`. A Unix socket of a user's program is not reachable from a CUPS backend under SELinux (Fedora: `cupsd_t` may connect to any TCP port, not to sockets of `unconfined_t`). Both sides check the other in `/proc/net/tcp`: the backend connects only if the listener belongs to the user who printed, the agent takes documents only from root's connections. One agent at a time listens (like the Windows pipe): another user's session at the same time does not offer printing.
- macOS: the Unix socket `/private/tmp/nuvira-print-<uid>` (mode 0600), where CUPS's sandbox lets a backend connect (`/private/tmp` and `/private/var/run` only; `/Users` is closed to it). Both sides check the other with `getpeereid`.

On the connection the backend writes `NUVIRA-PRINT 1`, the title and the PDF, and the agent answers `sent` or why it did not send it (not allowed, too large, empty). Without an agent listening (no session, or a session whose viewer does not print) the backend cancels the job at once, with "NuviraHub: no session of this user prints now; document not sent" in the queue: nothing printed outside a session can come out at the next technician's. `client/agent/src/printer.rs` (`cups_impl`).

Installing the queue needs root: `nuvira service printer [--remove]` as root (`client/service/src/cups.rs`) copies the backend (from beside the program, or `/usr/lib/nuvira/nuvira-cups` of the packages), writes the PPD to a temporary file and runs `lpadmin`. Who runs it:
- the .deb and the .rpm (postinstall; preremove only when the package is removed, not updated), if CUPS is installed;
- the app, while installing unattended access, after the system's administrator prompt (`pkexec` on Linux, `do shell script … with administrator privileges` on macOS); without the password unattended access is installed anyway and the app says the printer was not; "Remove unattended access" removes it the same way (not the packages' one);
- by hand: `sudo nuvira service printer`.

**Without a session.** The spooler finds no pipe: the job stays in the queue as "Error" and the spooler tries again every few seconds. As soon as a session starts it would arrive: the agent drops jobs submitted before the session started ("document printed before the session: not sent" in agent.log), so that a document printed hours earlier does not come out of the next technician's printer. The person at the PC sees the job in error in the queue and can delete it.

**The viewer.** `client/viewer/src/printing.rs` and `print.html`. The PDF goes to a temporary file, deleted once printed, saved or dropped.
- Windows: Windows' own PDF renderer (`Windows.Data.Pdf`), page by page, at the printer's resolution up to 300 dpi, sent through GDI (`StartDoc`/`StretchDIBits`); the paper's orientation follows each page; real size when the page fits the paper (centred), otherwise reduced to the printable area.
- Linux and macOS: CUPS, which prints PDF itself (`lp -d PRINTER -t TITLE`); printers from `lpstat -e`, the default from `lpstat -d`. The viewer says it can print only if `lpstat` exists.
- The choice "without asking" is `print.json` in the data folder (`{"printer": …, "automatic": true}`), read also by the app.
- Hidden options for tests: `--print-to PRINTER` (prints without the window), `--print-save DIR` (saves without the window); with `NUVIRA_PRINT_OUTPUT=FILE` the Windows printer's output goes into a file (e.g. "Microsoft Print to PDF" without its dialog). Every document printed or saved writes `@printed` on the output.

Note: on Windows with "Let Windows manage my default printer" the default printer becomes the last one used, by NuviraHub as by any other program.

## Protocol

- `Hello.print`: from the viewer, it prints documents of the agent; from the agent, what is printed on its NuviraHub printer during the session reaches the viewer.
- `FileChunk.print` (first chunk of a unidirectional stream agent → viewer): the stream carries a printed document (PDF), `name` its title, `size` its bytes.
- `Policy.no_print`: rule of the server.

## Tests

2026-11-07, agent on PC-LABORATORIO (Windows 10, service with this build: the installation created the printer), viewer on PC-STUDIO-NUC (Windows 11) and on the Mint VM (X11).
- Pipe port feasibility first, by hand: "Microsoft Print To PDF" on `\\.\pipe\…` delivers the job as `%PDF-1.7`; `Add-PrinterPort` and `XcvData AddPort` refuse the pipe unless an instance exists; without a listener the job goes into error and is delivered 2.7 s after the pipe appears.
- `service printer --remove`, `service install`: printer removed and installed again; installing twice says "already installed".
- `Out-Printer` and a .NET `PrintDocument` ("Prova stampa remota NuviraHub", text, a rectangle and a filled circle) on LAB: the PDF (57–187 KB) reached the viewer with its title (`--print-save`).
- Rendering on Windows, without paper: `--print-to "Microsoft Print to PDF"` with `NUVIRA_PRINT_OUTPUT`: a two-page A4 document came out as two pages identical to the original.
- The "Print" window on the NUC (button pressed through UI Automation) on the Xerox WorkCentre 6515 (IPP class driver): job spooled and printed, one page.
- Linux viewer on Mint: `--print-save` ok; `--print-to` on a CUPS queue for the same Xerox, paused: the job (187 KB, with its title) stood in the queue, then deleted with the queue; the "Print" window shown with the CUPS printer (screenshot).
- A job printed on LAB while no viewer was connected stood in error, arrived at the start of the next session and was dropped as printed before the session.

2026-11-07, Linux as the controlled computer: agent on the Mint VM (X11, AppArmor), viewer on the Fedora VM (Wayland), through the Pro test server; queue also on Fedora (SELinux enforcing).
- `sudo nuvira service printer` on Mint and Fedora: queue and backend (`bin_t` under SELinux) installed; without root it says to use sudo.
- Without a session: a text file printed with `lp` went through `texttopdf`/`pdftopdf` to the backend, which cancelled the job with the message (Mint `error_log`, Fedora journal); no SELinux denial.
- Fedora: a test listener of the user on 127.0.0.1:21190 received `NUVIRA-PRINT 1`, the title and a `%PDF-1.3` of 20 KB from the backend.
- During the session on Mint: a text file ("Fattura di prova 12", 17 KB) and CUPS's test page (PDF, 129 KB) reached the viewer with their titles (`--print-save`); a connection to the port from a program of the user was closed ("not from CUPS"); after the session the port was closed and the next job cancelled.
- .deb on Mint (update from 0.15.0, same version again, remove, install) and .rpm on Fedora (update from 0.14.0, reinstall, remove, install): the queue is there after every install and update, removal takes queue and backend away.
- `pkexec nuvira service printer [--remove]` from the user (with a temporary polkit rule: the password of the VM is not known here); without a polkit agent pkexec ends with 127, taken as "password not given".
- App settings on Mint: "Allow remote printing" shown.

Not tested yet: the whole macOS side (SIP on `/usr/libexec/cups/backend`, the administrator prompt, the sandbox, `cgpdftopdf`), the app's installation with the real polkit window, a Linux agent on Wayland, printing from a GTK or LibreOffice dialog, two users signed in at the same time on Linux, the "Allow remote printing" switch on Linux/macOS during a session, macOS viewer, a document with landscape and portrait pages mixed, two Windows sessions on the same PC, a PC without the "Microsoft Print To PDF" feature, large documents (hundreds of pages).
