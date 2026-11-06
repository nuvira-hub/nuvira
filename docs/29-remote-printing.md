# NuviraHub — Remote printing

[← Index](README.md)

A document of the controlled computer printed on the technician's printer: an invoice the customer needs on paper, a report generated on the remote PC, a page of a program that exists only there.

- **On the controlled computer (Windows).** A printer named "NuviraHub" appears among the others. During a session, whatever is printed on it, from any program, goes to the technician as a PDF. Without a session nothing arrives anywhere (see "Without a session").
- **At the technician's (Windows, Linux, macOS).** Every document opens the "Print" window: its title and size, the printer (the default one already chosen), "Print", "Save as PDF…" and "Don't print". With "From now on print on this printer without asking" the next documents go straight to that printer, with a notice in the session window; the app gives the choice back in Settings → Printing → "Ask every time". Closing the window prints none of the documents waiting in it. Printers that would open a dialog of their own (Microsoft Print to PDF, XPS, Fax: ports `PORTPROMPT:` and `SHRFAX:`) and this computer's own NuviraHub printer are not listed: "Save as PDF…" does the same.
- **Not allowing it at the PC.** App settings → "This computer" → "Allow remote printing" (Windows, on by default), kept like the other permissions (`app.json` `no_print`, `permissions.json` `print` in the agent's data folder, `AgentCommand::AllowPrint`). Not allowed: the agent does not listen; turned off during a session, the documents printed from then on are not sent.
- **Forbidding it.** On a NuviraHub Pro server, rule "No remote printing" in the web console (Rules) and in the app (Administration), `Policy.no_print`.

## How it works

**The printer.** `nuvira service install` (the setup, the app's "unattended access", automatic updates) adds it; `nuvira service uninstall` removes it; the hidden `nuvira service printer [--remove]` does only that. It is Windows' own "Microsoft Print To PDF" driver (an optional feature present by default in Windows 10 and 11; without it the installation says so and goes on without printer) on a local port that is the named pipe `\\.\pipe\nuvira-print`. No driver of ours, nothing loaded into the spooler: the spooler opens the port for every job and writes the PDF the driver makes into it. The local port monitor accepts a pipe as a port only if it can open it at that moment, so the installation keeps an instance of the pipe open while it adds the port (`XcvData AddPort`, then `AddPrinter`). `client/service/src/printer.rs`.

**The agent.** A session prints when the viewer says it can (`Hello.print` from the viewer) and nothing forbids it (the person at the PC, the server's rules, view-only mode, synthetic image, terminal-only session). The agent then creates the pipe (`first_pipe_instance`: if the agent of another Windows session already listens, this one does not offer printing) with access for SYSTEM, administrators and signed-in users, and answers `Hello.print`. For every job: it reads the PDF to the end (at most 512 MB), takes its title and submission time from the spooler's queue (`EnumJobs`, the job being printed), and sends it on a unidirectional stream with `FileChunk.print` and the title in `name`. The pipe exists only during the session. `client/agent/src/printer.rs`.

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

Not tested yet: macOS viewer, a document with landscape and portrait pages mixed, two Windows sessions on the same PC, a PC without the "Microsoft Print To PDF" feature, large documents (hundreds of pages).
