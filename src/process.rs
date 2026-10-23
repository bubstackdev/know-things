//! Bounded framed subprocess I/O with a single deadline and no output/log scraping.
use crate::contracts::{Error, Result};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::File,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::process::CommandExt,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
const MAX_FRAME: usize = 1024 * 1024;
pub struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    output: ChildStdout,
    failed: bool,
}
impl Worker {
    pub fn spawn_with_control(mut command: Command) -> Result<(Self, Control)> {
        let mut descriptors = [-1; 2];
        // SAFETY: pipe receives space for two fds, immediately transferred to owned Files.
        if unsafe { libc::pipe(descriptors.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let read = unsafe { File::from_raw_fd(descriptors[0]) };
        let write = unsafe { File::from_raw_fd(descriptors[1]) };
        for fd in descriptors {
            // SAFETY: both fds are owned live pipe ends; close-on-exec prevents unintended inheritance.
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        let fd = read.as_raw_fd();
        command.env("KNOW_THINGS_CONTROL_FD", fd.to_string());
        // SAFETY: this child-only hook uses only async-signal-safe descriptor operations.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let worker = Self::spawn(&mut command)?;
        drop(read);
        let flags = unsafe { libc::fcntl(write.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(write.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok((worker, Control(Arc::new(Mutex::new(write)))))
    }
    pub fn spawn(command: &mut Command) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let input = child.stdin.take().expect("piped stdin");
        let output = child.stdout.take().expect("piped stdout");
        // Nonblocking descriptors make both slow input consumers and partial output frames bounded.
        for fd in [input.as_raw_fd(), output.as_raw_fd()] {
            // SAFETY: fds are live owned child pipes; flags do not change ownership.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(Self {
            child,
            input: Some(input),
            output,
            failed: false,
        })
    }
    pub fn receive<T: DeserializeOwned>(&mut self, deadline: Instant) -> Result<T> {
        self.receive_checked(deadline, || Ok(()))
    }
    pub fn receive_checked<T: DeserializeOwned>(
        &mut self,
        deadline: Instant,
        mut check: impl FnMut() -> Result<()>,
    ) -> Result<T> {
        if self.failed {
            return Err(Error::new(
                "worker_failed",
                "Worker is unavailable after an earlier failure",
            ));
        }
        let result = (|| -> Result<T> {
            let mut prefix = [0u8; 4];
            read_all_checked(&mut self.output, &mut prefix, deadline, &mut check)?;
            let size = u32::from_be_bytes(prefix) as usize;
            if size == 0 || size > MAX_FRAME {
                return Err(Error::new(
                    "worker_failed",
                    "Worker frame exceeds protocol limits",
                ));
            }
            let mut bytes = vec![0; size];
            read_all_checked(&mut self.output, &mut bytes, deadline, &mut check)?;
            serde_json::from_slice(&bytes)
                .map_err(|_| Error::new("worker_failed", "Malformed worker frame"))
        })();
        if result.is_err() {
            self.stop();
        }
        result
    }
    pub fn send(&mut self, value: &impl Serialize, deadline: Instant) -> Result<()> {
        self.send_checked(value, deadline, || Ok(()))
    }
    pub fn send_checked(
        &mut self,
        value: &impl Serialize,
        deadline: Instant,
        mut check: impl FnMut() -> Result<()>,
    ) -> Result<()> {
        if self.failed {
            return Err(Error::new(
                "worker_failed",
                "Worker is unavailable after an earlier failure",
            ));
        }
        let bytes = serde_json::to_vec(value)?;
        if bytes.len() > MAX_FRAME {
            return Err(Error::new(
                "resource_exhausted",
                "Worker request exceeds 1 MiB",
            ));
        }
        let result = (|| -> Result<()> {
            let input = self
                .input
                .as_mut()
                .ok_or_else(|| Error::new("worker_failed", "Worker request pipe is closed"))?;
            write_all_checked(
                input,
                &(bytes.len() as u32).to_be_bytes(),
                deadline,
                &mut check,
            )?;
            write_all_checked(input, &bytes, deadline, &mut check)
        })();
        if result.is_err() {
            self.stop();
        }
        result
    }
    pub fn stop(&mut self) {
        if self.failed {
            return;
        }
        self.failed = true;
        drop(self.input.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
    pub fn deadline(timeout: Duration) -> Instant {
        Instant::now() + timeout
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        drop(self.input.take());
        if !self.failed {
            // Healthy idle workers can release their own runtime resources on EOF.
            // Failures/cancellation use stop() directly and never wait this grace.
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => {
                        self.failed = true;
                        return;
                    }
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(
                            Duration::from_millis(10)
                                .min(deadline.saturating_duration_since(Instant::now())),
                        );
                    }
                    _ => break,
                }
            }
        }
        self.stop();
    }
}
fn poll_checked(
    fd: i32,
    event: i16,
    deadline: Instant,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    loop {
        check()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::new(
                "deadline_exceeded",
                "Local worker deadline exceeded",
            ));
        }
        let mut pollfd = libc::pollfd {
            fd,
            events: event,
            revents: 0,
        };
        // SAFETY: pollfd refers to one initialized descriptor for the duration of this call.
        let result =
            unsafe { libc::poll(&mut pollfd, 1, remaining.as_millis().clamp(1, 20) as i32) };
        if result > 0 {
            return Ok(());
        }
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(Error::new("worker_failed", "Worker pipe polling failed"));
        }
    }
}
fn read_all(reader: &mut (impl Read + AsRawFd), bytes: &mut [u8], deadline: Instant) -> Result<()> {
    read_all_checked(reader, bytes, deadline, &mut || Ok(()))
}
fn read_all_checked(
    reader: &mut (impl Read + AsRawFd),
    mut bytes: &mut [u8],
    deadline: Instant,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    while !bytes.is_empty() {
        poll_checked(reader.as_raw_fd(), libc::POLLIN, deadline, check)?;
        match reader.read(bytes) {
            Ok(0) => return Err(Error::new("worker_failed", "Worker closed its event pipe")),
            Ok(n) => bytes = &mut bytes[n..],
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(Error::new("worker_failed", "Worker event pipe failed")),
        }
    }
    Ok(())
}
fn write_all(writer: &mut (impl Write + AsRawFd), bytes: &[u8], deadline: Instant) -> Result<()> {
    write_all_checked(writer, bytes, deadline, &mut || Ok(()))
}
fn write_all_checked(
    writer: &mut (impl Write + AsRawFd),
    mut bytes: &[u8],
    deadline: Instant,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    while !bytes.is_empty() {
        poll_checked(writer.as_raw_fd(), libc::POLLOUT, deadline, check)?;
        match writer.write(bytes) {
            Ok(0) => {
                return Err(Error::new(
                    "worker_failed",
                    "Worker closed its request pipe",
                ));
            }
            Ok(n) => bytes = &bytes[n..],
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(Error::new("worker_failed", "Worker request pipe failed")),
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct Control(Arc<Mutex<File>>);
impl Control {
    pub fn send(&self, value: &impl Serialize, deadline: Instant) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        if bytes.len() > 4096 {
            return Err(Error::new("invalid_request", "Control frame exceeds 4 KiB"));
        }
        let mut writer = self
            .0
            .lock()
            .map_err(|_| Error::new("worker_failed", "Control channel unavailable"))?;
        write_all(&mut *writer, &(bytes.len() as u32).to_be_bytes(), deadline)?;
        write_all(&mut *writer, &bytes, deadline)
    }
}

/// Write a whole frame under one deadline; callers must own a nonblocking fd.
pub fn write_all_until(
    writer: &mut (impl Write + AsRawFd),
    bytes: &[u8],
    deadline: Instant,
) -> Result<()> {
    write_all(writer, bytes, deadline)
}

pub fn read_all_until(
    reader: &mut (impl Read + AsRawFd),
    bytes: &mut [u8],
    deadline: Instant,
) -> Result<()> {
    read_all(reader, bytes, deadline)
}
