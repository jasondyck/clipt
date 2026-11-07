//! One recording at a time, and the commands other clipt processes send it:
//! stop, pause and resume (`clipt record --stop` and friends, the tray, the
//! overlay). Unix uses a locked pidfile and a FIFO (on macOS, in the user's
//! own temporary directory); Windows a named mutex and a named pipe.

use anyhow::{Result, bail};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[cfg(unix)]
use unix as imp;
#[cfg(windows)]
use win as imp;

pub use imp::{Lock, free_bytes, handle_signals, status};

pub static STOP: AtomicBool = AtomicBool::new(false);
pub static PAUSED: AtomicBool = AtomicBool::new(false);
/// When the last pause, resume and stop signals arrived (monotonic clock,
/// ns). The writer may be busy handing ffmpeg a frame when one arrives, so
/// it uses these rather than the moment it notices.
pub static PAUSED_AT: AtomicU64 = AtomicU64::new(0);
pub static RESUMED_AT: AtomicU64 = AtomicU64::new(0);
pub static STOPPED_AT: AtomicU64 = AtomicU64::new(0);

pub const CMD_STOP: u8 = b's';
pub const CMD_PAUSE: u8 = b'p';
pub const CMD_RESUME: u8 = b'r';

/// The Instant matching a monotonic_ns() reading (0 = now).
pub fn instant_at(ns: u64) -> Instant {
    let ago = imp::monotonic_ns().saturating_sub(ns);
    if ns == 0 { Instant::now() } else { Instant::now().checked_sub(Duration::from_nanos(ago)).unwrap_or_else(Instant::now) }
}

/// Act on a command from the control FIFO or pipe, or a signal.
/// Async-signal-safe.
pub fn apply(cmd: u8) {
    // The flag to set, its new value, and where to note when it changed.
    let (flag, value, at) = match cmd {
        CMD_PAUSE => (&PAUSED, true, &PAUSED_AT),
        CMD_RESUME => (&PAUSED, false, &RESUMED_AT),
        CMD_STOP => (&STOP, true, &STOPPED_AT),
        _ => return,
    };
    if flag.swap(value, Ordering::SeqCst) != value {
        at.store(imp::monotonic_ns(), Ordering::SeqCst);
    }
}

/// Fail early, before the overlay opens, if a recording is already running.
pub fn ensure_idle() -> Result<()> {
    match status() {
        Some(_) => bail!("already recording; stop it with 'clipt record --stop'"),
        None => Ok(()),
    }
}

/// `clipt record --stop`: tell the running recording to stop and wait for it
/// to finish writing.
pub fn stop() -> Result<()> {
    imp::send(CMD_STOP)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while status().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::ensure!(status().is_none(), "the recording didn't stop within 30 s");
    Ok(())
}

pub enum PauseCmd {
    Pause,
    Resume,
    Toggle,
}

/// `clipt record --pause / --resume / --toggle-pause`.
pub fn pause(cmd: PauseCmd) -> Result<()> {
    let Some(paused) = status() else { bail!("no recording is running") };
    let pause = match cmd {
        PauseCmd::Pause => true,
        PauseCmd::Resume => false,
        PauseCmd::Toggle => !paused,
    };
    imp::send(if pause { CMD_PAUSE } else { CMD_RESUME })
}

#[cfg(unix)]
mod unix {
    use super::{CMD_PAUSE, CMD_RESUME, CMD_STOP, apply};
    use anyhow::{Context, Result, bail};
    use std::path::{Path, PathBuf};

    pub fn monotonic_ns() -> u64 {
        // clock_gettime is async-signal-safe, so the handler may call this.
        let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
    }

    extern "C" fn on_signal(sig: libc::c_int) {
        apply(match sig {
            libc::SIGUSR1 => CMD_PAUSE,
            libc::SIGUSR2 => CMD_RESUME,
            _ => CMD_STOP,
        });
    }

    /// Ctrl-C and SIGTERM stop the recording; SIGUSR1 and SIGUSR2 pause and
    /// resume it.
    pub fn handle_signals() {
        unsafe {
            for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGUSR1, libc::SIGUSR2] {
                libc::signal(sig, on_signal as *const () as libc::sighandler_t);
            }
        }
    }

    // Recording control works through files in the runtime dir rather than
    // pids: each Flatpak instance has its own PID namespace (where the first
    // process is always pid 2), so a pid in the pidfile means nothing to another
    // `flatpak run`, and signalling it would hit the wrong process.
    //   clipt-record.pid  held under an exclusive flock while recording (locks
    //                     cross PID namespaces); says "recording" or "paused"
    //   clipt-record.ctl  a FIFO the recorder reads one-byte commands from

    fn runtime_file(name: &str) -> PathBuf {
        #[cfg(target_os = "macos")]
        let dir = darwin_user_temp_dir();
        #[cfg(not(target_os = "macos"))]
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        dir.unwrap_or_else(std::env::temp_dir).join(name)
    }

    /// The user's own temporary directory (`/var/folders/…/T/`), from the
    /// system rather than `$TMPDIR`, which sudo and some launchd contexts don't
    /// set: they'd fall back to the shared `/tmp`, and the tray and a terminal
    /// would look for the recording in different places.
    #[cfg(target_os = "macos")]
    fn darwin_user_temp_dir() -> Option<PathBuf> {
        use std::os::unix::ffi::OsStringExt;
        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr().cast(), buf.len()) };
        // n counts the terminating NUL; 0 is failure, more than the buffer too long.
        (n > 1 && n <= buf.len()).then(|| {
            buf.truncate(n - 1);
            PathBuf::from(std::ffi::OsString::from_vec(buf))
        })
    }

    fn pidfile() -> PathBuf {
        runtime_file("clipt-record.pid")
    }

    fn ctlfile() -> PathBuf {
        runtime_file("clipt-record.ctl")
    }

    /// The running recording, if any: whether it's paused. The pidfile holds the
    /// recorder's pid (for people reading it) on the first line and "paused" or
    /// "recording" on the second.
    pub fn status() -> Option<bool> {
        use rustix::fs::{FlockOperation, flock};
        let f = std::fs::File::open(pidfile()).ok()?;
        match flock(&f, FlockOperation::NonBlockingLockShared) {
            Ok(()) => None, // nobody holds it: stale
            Err(_) => {
                let s = std::io::read_to_string(&f).unwrap_or_default();
                Some(s.lines().nth(1) == Some("paused"))
            }
        }
    }

    /// Holds the pidfile (locked) and the control FIFO for as long as the
    /// recording runs.
    pub struct Lock {
        file: std::fs::File,
    }

    impl Lock {
        pub fn take() -> Result<Self> {
            use rustix::fs::{FlockOperation, flock};
            let p = pidfile();
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&p)
                .with_context(|| format!("opening {}", p.display()))?;
            // `status()`, which a tray polls, holds a shared lock for a moment:
            // a recording starting just then mustn't be told one is running.
            let mut tries = 0;
            while flock(&file, FlockOperation::NonBlockingLockExclusive).is_err() {
                tries += 1;
                if tries == 10 {
                    bail!("already recording; stop it with 'clipt record --stop'");
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let lock = Self { file };
            lock.set(false)?;
            let ctl = ctlfile();
            let _ = std::fs::remove_file(&ctl); // stale
            // rustix's mkfifoat is not built for Apple.
            #[cfg(target_os = "macos")]
            {
                use std::os::unix::ffi::OsStrExt;
                let mut bytes = ctl.as_os_str().as_bytes().to_vec();
                bytes.push(0);
                if unsafe { libc::mkfifo(bytes.as_ptr().cast(), 0o600) } != 0 {
                    return Err(std::io::Error::last_os_error()).with_context(|| format!("creating {}", ctl.display()));
                }
            }
            #[cfg(not(target_os = "macos"))]
            rustix::fs::mkfifoat(rustix::fs::CWD, &ctl, rustix::fs::Mode::from_raw_mode(0o600))
                .with_context(|| format!("creating {}", ctl.display()))?;
            // Read and write: with a writer of its own, reads wait for the next
            // command instead of seeing end-of-file whenever a sender closes.
            let mut fifo =
                std::fs::OpenOptions::new().read(true).write(true).open(&ctl).with_context(|| format!("opening {}", ctl.display()))?;
            std::thread::spawn(move || {
                let mut b = [0u8; 16];
                while let Ok(n) = std::io::Read::read(&mut fifo, &mut b) {
                    b[..n].iter().for_each(|&c| apply(c));
                }
            });
            Ok(lock)
        }

        pub fn set(&self, paused: bool) -> Result<()> {
            use std::os::unix::fs::FileExt;
            // Rewritten in place: replacing the file would drop the lock.
            let s = format!("{}\n{}\n", std::process::id(), if paused { "paused" } else { "recording" });
            self.file.set_len(0)?;
            self.file.write_all_at(s.as_bytes(), 0)?;
            Ok(())
        }
    }

    impl Drop for Lock {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(ctlfile());
            let _ = std::fs::remove_file(pidfile());
        }
    }

    /// Hand the running recording a command.
    pub fn send(cmd: u8) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if status().is_none() {
            bail!("no recording is running");
        }
        // Non-blocking, so a FIFO nobody reads fails at once instead of hanging.
        let mut fifo = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(ctlfile())
            .context("no recording is running (its control FIFO isn't open)")?;
        fifo.write_all(&[cmd]).context("sending the command to the recording")?;
        Ok(())
    }

    pub fn free_bytes(path: &Path) -> Option<u64> {
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let s = rustix::fs::statvfs(dir).ok()?;
        Some(s.f_bavail * s.f_frsize)
    }
}

/// Windows: a named mutex in the session's namespace says a recording is
/// running, a file beside it in %TEMP% whether it's paused, and commands
/// arrive on a named pipe, one byte per connection.
#[cfg(windows)]
mod win {
    use super::{CMD_STOP, apply};
    use anyhow::{Context, Result, bail};
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, PIPE_ACCESS_INBOUND, ReadFile, SYNCHRONIZE};
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_TYPE_BYTE, PIPE_WAIT};
    use windows_sys::Win32::System::Threading::{CreateMutexW, OpenMutexW};

    const MUTEX: &str = r"Local\clipt-record";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// Pipes are machine-wide, unlike `Local\` mutexes: the session id keeps
    /// two people's recordings on one machine apart.
    fn pipe_name() -> String {
        let mut session = 0u32;
        unsafe { windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId(std::process::id(), &mut session) };
        format!(r"\\.\pipe\clipt-record-{session}")
    }

    fn state_file() -> PathBuf {
        std::env::temp_dir().join("clipt-record.state")
    }

    pub fn monotonic_ns() -> u64 {
        static BASE: OnceLock<Instant> = OnceLock::new();
        // Never 0, which instant_at reads as "now".
        BASE.get_or_init(Instant::now).elapsed().as_nanos() as u64 + 1
    }

    unsafe extern "system" fn on_ctrl(event: u32) -> i32 {
        const CTRL_BREAK_EVENT: u32 = 1;
        apply(CMD_STOP);
        // Closing the console, logging off and shutting down end the
        // process as soon as this returns (and after 5 s regardless), so
        // hold on while the recording finishes its file and exits.
        if event > CTRL_BREAK_EVENT {
            std::thread::sleep(std::time::Duration::from_secs(10));
        }
        // Handled: the recording ends itself, and with it the process.
        1
    }

    /// Ctrl-C, Ctrl-Break and closing the console stop the recording.
    pub fn handle_signals() {
        unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) };
    }

    /// The running recording, if any: whether it's paused.
    pub fn status() -> Option<bool> {
        let name = wide(MUTEX);
        let h = unsafe { OpenMutexW(SYNCHRONIZE, 0, name.as_ptr()) };
        if h.is_null() {
            return None;
        }
        unsafe { CloseHandle(h) };
        Some(std::fs::read_to_string(state_file()).is_ok_and(|s| s.trim() == "paused"))
    }

    /// Holds the mutex for as long as the recording runs.
    pub struct Lock {
        mutex: HANDLE,
    }

    impl Lock {
        pub fn take() -> Result<Self> {
            let name = wide(MUTEX);
            let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
            anyhow::ensure!(!mutex.is_null(), "creating the recording's mutex failed");
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                unsafe { CloseHandle(mutex) };
                bail!("already recording; stop it with 'clipt record --stop'");
            }
            let lock = Self { mutex };
            lock.set(false)?;
            let pipe = wide(&pipe_name());
            std::thread::spawn(move || {
                loop {
                    let h = unsafe {
                        CreateNamedPipeW(pipe.as_ptr(), PIPE_ACCESS_INBOUND, PIPE_TYPE_BYTE | PIPE_WAIT, 1, 0, 64, 0, std::ptr::null())
                    };
                    if h == INVALID_HANDLE_VALUE {
                        return;
                    }
                    // A client that connected before this call fails it with
                    // ERROR_PIPE_CONNECTED, and is connected all the same.
                    unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) };
                    let mut b = [0u8; 16];
                    let mut n = 0u32;
                    while unsafe { ReadFile(h, b.as_mut_ptr(), b.len() as u32, &mut n, std::ptr::null_mut()) } != 0 && n > 0 {
                        b[..n as usize].iter().for_each(|&c| apply(c));
                    }
                    unsafe {
                        DisconnectNamedPipe(h);
                        CloseHandle(h);
                    }
                }
            });
            Ok(lock)
        }

        pub fn set(&self, paused: bool) -> Result<()> {
            let p = state_file();
            std::fs::write(&p, if paused { "paused" } else { "recording" }).with_context(|| format!("writing {}", p.display()))
        }
    }

    impl Drop for Lock {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(state_file());
            unsafe { CloseHandle(self.mutex) };
        }
    }

    /// Hand the running recording a command.
    pub fn send(cmd: u8) -> Result<()> {
        use std::io::Write;
        if status().is_none() {
            bail!("no recording is running");
        }
        // The pipe has one instance, busy between a client and the next
        // CreateNamedPipeW: try again for a moment.
        let mut tries = 0;
        let mut pipe = loop {
            match std::fs::OpenOptions::new().write(true).open(pipe_name()) {
                Ok(p) => break p,
                Err(_) if tries < 50 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => return Err(e).context("no recording is running (its control pipe isn't open)"),
            }
        };
        pipe.write_all(&[cmd]).context("sending the command to the recording")?;
        Ok(())
    }

    pub fn free_bytes(path: &Path) -> Option<u64> {
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let w: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        let mut free = 0u64;
        let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
        (ok != 0).then_some(free)
    }

    use std::os::windows::ffi::OsStrExt;
}
