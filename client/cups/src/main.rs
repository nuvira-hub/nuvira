//! CUPS backend of the "NuviraHub" printer (see the library's documentation): CUPS
//! runs it as root for every job, as
//! `nuvira job-id user title copies options [file]`, with the PDF on its input
//! when there is no file. It hands the document to the agent of `user`, if a session
//! of theirs prints; otherwise the job is cancelled with a message (nothing printed
//! without a session may come out at the next technician's).

#[cfg(unix)]
fn main() {
    std::process::exit(backend::run());
}

#[cfg(not(unix))]
fn main() {
    eprintln!("nuvira-cups is a CUPS backend: Linux and macOS only");
    std::process::exit(1);
}

#[cfg(unix)]
mod backend {
    use std::ffi::CString;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::time::Duration;

    // Exit codes of CUPS backends (cups/backend.h).
    const OK: i32 = 0;
    const FAILED: i32 = 1;
    /// The job is cancelled; the queue goes on with the next one.
    const CANCEL: i32 = 5;

    pub fn run() -> i32 {
        let args: Vec<String> = std::env::args().collect();
        match args.len() {
            // Device discovery (`lpinfo -v`): nothing to offer, the queue is made by
            // `nuvira service printer`.
            1 => OK,
            6 | 7 => match send(&args) {
                Ok(()) => OK,
                Err(Refused(message)) => {
                    // "ERROR:" lines become the job's and the printer's state message.
                    eprintln!("ERROR: {message}");
                    CANCEL
                }
            },
            _ => {
                eprintln!("usage: nuvira job-id user title copies options [file]");
                FAILED
            }
        }
    }

    struct Refused(String);

    impl<E: std::fmt::Display> From<E> for Refused {
        fn from(e: E) -> Self {
            Refused(e.to_string())
        }
    }

    fn send(args: &[String]) -> Result<(), Refused> {
        let (user, title) = (&args[2], &args[3]);
        let uid = uid_of(user).ok_or_else(|| Refused(format!("NuviraHub: unknown user {user:?}")))?;
        let mut agent = connect(uid)?;
        let title: String = title.chars().filter(|c| !c.is_control()).collect();
        let title = if title.trim().is_empty() { "document".to_string() } else { title };
        let title = truncate(&title, nuvira_cups::MAX_TITLE);
        eprintln!("INFO: Sending the document to the NuviraHub session");
        agent.write_all(format!("{}\n{title}\n", nuvira_cups::HEADER).as_bytes())?;
        match args.get(6) {
            Some(file) => std::io::copy(&mut std::fs::File::open(file)?, &mut agent)?,
            None => std::io::copy(&mut std::io::stdin().lock(), &mut agent)?,
        };
        agent.done_writing()?;
        let mut answer = String::new();
        BufReader::new(agent.reader()).take(4096).read_line(&mut answer)?;
        let answer = answer.trim();
        if answer == nuvira_cups::SENT {
            eprintln!("INFO: Sent to the NuviraHub session");
            Ok(())
        } else if answer.is_empty() {
            Err(Refused("NuviraHub: the session ended before the document was sent".into()))
        } else {
            Err(Refused(format!("NuviraHub: document not sent ({answer})")))
        }
    }

    fn truncate(text: &str, max: usize) -> &str {
        let mut end = text.len().min(max);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    }

    fn uid_of(user: &str) -> Option<u32> {
        let name = CString::new(user).ok()?;
        // SAFETY: getpwnam returns null or a pointer to a static record, read at once.
        let entry = unsafe { libc::getpwnam(name.as_ptr()) };
        (!entry.is_null()).then(|| unsafe { (*entry).pw_uid })
    }

    fn no_session() -> Refused {
        Refused("NuviraHub: no session of this user prints now; document not sent".into())
    }

    const TIMEOUT: Duration = Duration::from_secs(30);

    #[cfg(target_os = "linux")]
    struct Agent(std::net::TcpStream);

    #[cfg(target_os = "linux")]
    fn connect(uid: u32) -> Result<Agent, Refused> {
        use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};

        let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, nuvira_cups::PORT);
        // Who listens must be the user who printed, not another program on the port.
        match nuvira_cups::tcp_owner(address, None) {
            None => return Err(no_session()),
            Some(owner) if owner != uid => return Err(no_session()),
            Some(_) => {}
        }
        let stream =
            TcpStream::connect_timeout(&address.into(), Duration::from_secs(5)).map_err(|_| no_session())?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        // The listener may have changed between the check and the connection: the
        // other end of this very connection must be the user's too.
        let mine = match stream.local_addr()? {
            std::net::SocketAddr::V4(a) => a,
            _ => return Err(no_session()),
        };
        if nuvira_cups::tcp_owner(address, Some(mine)) != Some(uid) {
            return Err(no_session());
        }
        Ok(Agent(stream))
    }

    #[cfg(target_os = "macos")]
    struct Agent(std::os::unix::net::UnixStream);

    #[cfg(target_os = "macos")]
    fn connect(uid: u32) -> Result<Agent, Refused> {
        use std::os::fd::AsRawFd;

        let stream = std::os::unix::net::UnixStream::connect(nuvira_cups::socket_path(uid))
            .map_err(|_| no_session())?;
        let (mut peer_uid, mut peer_gid) = (0, 0);
        // SAFETY: valid socket and out pointers.
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut peer_uid, &mut peer_gid) } != 0
            || peer_uid != uid
        {
            return Err(no_session());
        }
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        Ok(Agent(stream))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    struct Agent(std::fs::File);

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn connect(_uid: u32) -> Result<Agent, Refused> {
        Err(no_session())
    }

    impl Write for Agent {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }

    impl Agent {
        /// The whole document is written: the agent reads to the end.
        fn done_writing(&self) -> std::io::Result<()> {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            return self.0.shutdown(std::net::Shutdown::Write);
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            Ok(())
        }

        fn reader(&self) -> impl Read + '_ {
            &self.0
        }
    }
}
