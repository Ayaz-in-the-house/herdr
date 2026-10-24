use std::fs;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

#[cfg(unix)]
use interprocess::local_socket::traits::Stream as _;

pub(crate) type LocalListener = interprocess::local_socket::Listener;
pub(crate) type LocalStream = interprocess::local_socket::Stream;

#[cfg(windows)]
pub(crate) enum LocalStreamRead {
    Data,
    Pending,
    Closed,
}

pub(crate) enum LocalStreamReadCount {
    Data(usize),
    Pending,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SocketFileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    marker: Vec<u8>,
}

pub(crate) fn connect_local_stream(path: &Path) -> io::Result<LocalStream> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::{prelude::*, GenericFilePath};

        let name = path.to_fs_name::<GenericFilePath>()?;
        LocalStream::connect(name)
    }

    #[cfg(windows)]
    {
        use interprocess::local_socket::{prelude::*, GenericNamespaced};

        let name = path.to_string_lossy().to_string();
        let name = name.to_ns_name::<GenericNamespaced>()?;
        LocalStream::connect(name)
    }
}

pub(crate) fn bind_local_listener(path: &Path) -> io::Result<LocalListener> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::{prelude::*, GenericFilePath, ListenerOptions};

        let name = path.to_fs_name::<GenericFilePath>()?;
        ListenerOptions::new()
            .name(name)
            .reclaim_name(false)
            .create_sync()
    }

    #[cfg(windows)]
    {
        use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};

        let name = path.to_string_lossy().to_string();
        let name = name.to_ns_name::<GenericNamespaced>()?;
        let listener = ListenerOptions::new()
            .name(name)
            .reclaim_name(false)
            .create_sync()?;
        fs::write(path, windows_socket_marker())?;
        Ok(listener)
    }
}

pub(crate) fn prepare_socket_path(
    path: &Path,
    busy_message: impl FnOnce(&Path) -> String,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    if !path.exists() {
        return Ok(());
    }

    match connect_local_stream(path) {
        Ok(_) => {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, busy_message(path)));
        }
        Err(err) if stale_socket_connect_error(err.kind()) => {}
        Err(err) => return Err(err),
    }

    if let Err(err) = fs::remove_file(path) {
        if err.kind() != io::ErrorKind::NotFound {
            return Err(err);
        }
    }

    Ok(())
}

fn stale_socket_connect_error(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound | io::ErrorKind::TimedOut
    ) || (cfg!(windows) && kind == io::ErrorKind::WouldBlock)
}

pub(crate) fn local_stream_peer_closed(stream: &mut LocalStream) -> io::Result<bool> {
    probe_stream_closed(stream)
}

pub(crate) fn set_local_stream_polling(stream: &mut LocalStream, enabled: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        stream.set_nonblocking(enabled)
    }

    #[cfg(windows)]
    {
        let _ = (stream, enabled);
        Ok(())
    }
}

/// Binds a listener for private terminal traffic. Unix callers restrict the
/// socket file after binding; Windows must set the named-pipe DACL at creation.
pub(crate) fn bind_private_local_listener(path: &Path) -> io::Result<LocalListener> {
    #[cfg(unix)]
    {
        bind_local_listener(path)
    }

    #[cfg(windows)]
    {
        use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};
        use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        use widestring::U16CString;

        let sddl = U16CString::from_str("D:P(A;;GA;;;SY)(A;;GA;;;OW)")
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        let security_descriptor = SecurityDescriptor::deserialize(&sddl)?;
        let name = path.to_string_lossy().to_string();
        let name = name.to_ns_name::<GenericNamespaced>()?;
        let listener = ListenerOptions::new()
            .name(name)
            .reclaim_name(false)
            .security_descriptor(security_descriptor)
            .create_sync()?;
        fs::write(path, windows_socket_marker())?;
        Ok(listener)
    }
}

#[cfg(windows)]
pub(crate) fn poll_local_stream_read(
    stream: &mut LocalStream,
    buf: &mut [u8],
) -> io::Result<LocalStreamRead> {
    match poll_local_stream_read_count(stream, buf)? {
        LocalStreamReadCount::Data(read) => {
            let _ = read;
            Ok(LocalStreamRead::Data)
        }
        LocalStreamReadCount::Pending => Ok(LocalStreamRead::Pending),
        LocalStreamReadCount::Closed => Ok(LocalStreamRead::Closed),
    }
}

/// What is waiting on the socket, without taking it.
///
/// A streaming method reads its body from the same socket as its request
/// line, so a bulk read that took one byte of that body with the line would
/// desynchronise every frame after it. Peeking lets the reader find the
/// newline in bulk and then consume exactly up to it.
/// TP-API-READ-01 · TP-API-READ-02
#[cfg(unix)]
pub(crate) fn peek_local_stream(
    stream: &LocalStream,
    buf: &mut [u8],
) -> io::Result<LocalStreamReadCount> {
    use std::os::fd::AsRawFd as _;

    let LocalStream::UdSocket(socket) = stream;
    let fd = socket.inner().as_raw_fd();
    // SAFETY: `buf` is a live, exclusively borrowed slice and `recv` writes at
    // most `buf.len()` bytes into it; `fd` stays open for the call because
    // `socket` is borrowed for it; `MSG_PEEK` leaves the socket's queue as it
    // was, and `MSG_DONTWAIT` turns a wait into `EAGAIN`.
    let read = unsafe {
        libc::recv(
            fd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if read < 0 {
        let err = io::Error::last_os_error();
        return match err.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => {
                Ok(LocalStreamReadCount::Pending)
            }
            _ => Err(err),
        };
    }
    if read == 0 {
        return Ok(LocalStreamReadCount::Closed);
    }
    Ok(LocalStreamReadCount::Data(read as usize))
}

/// Waits until the socket has something to read, or `deadline` passes;
/// `true` means readable.
///
/// This is what a reader does between looks instead of sleeping a fixed
/// interval: sleeping costs up to one interval per empty look, and a line
/// that lands during the sleep waits for the next tick. Hang-up and error
/// count as readable — the next look reports them. TP-API-READ-05
#[cfg(unix)]
pub(crate) fn wait_local_stream_readable(
    stream: &LocalStream,
    deadline: std::time::Instant,
) -> io::Result<bool> {
    use std::os::fd::AsRawFd as _;

    let LocalStream::UdSocket(socket) = stream;
    let mut poll_fds = [libc::pollfd {
        fd: socket.inner().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        poll_fds[0].revents = 0;
        // SAFETY: `poll_fds` is a live array of one initialised `pollfd`, and
        // the count passed is its length.
        let ready = unsafe { libc::poll(poll_fds.as_mut_ptr(), 1, timeout_ms) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if ready > 0 {
            return Ok(true);
        }
        // Zero is poll's own timeout; the loop's clock decides whether the
        // deadline has really passed, since poll rounds to milliseconds.
    }
}

/// The unix request-line reader does not use this: it peeks and waits on the
/// socket instead (`peek_local_stream`, `wait_local_stream_readable`). The
/// bridge in `remote::attach` reads its pipe this way on windows, and the
/// graphics-stream body reader falls back to it on any platform whose socket
/// refuses a receive timeout.
pub(crate) fn poll_local_stream_read_count(
    stream: &mut LocalStream,
    buf: &mut [u8],
) -> io::Result<LocalStreamReadCount> {
    #[cfg(unix)]
    {
        match stream.read(buf) {
            Ok(0) => Ok(LocalStreamReadCount::Closed),
            Ok(read) => Ok(LocalStreamReadCount::Data(read)),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                Ok(LocalStreamReadCount::Pending)
            }
            Err(err) => Err(err),
        }
    }

    #[cfg(windows)]
    {
        match windows_named_pipe_available(stream)? {
            None => Ok(LocalStreamReadCount::Closed),
            Some(0) => Ok(LocalStreamReadCount::Pending),
            Some(_) => match stream.read(buf) {
                Ok(0) => Ok(LocalStreamReadCount::Closed),
                Ok(read) => Ok(LocalStreamReadCount::Data(read)),
                Err(err) if is_connection_closed_error(&err) => Ok(LocalStreamReadCount::Closed),
                Err(err) => Err(err),
            },
        }
    }
}

#[cfg(unix)]
fn probe_stream_closed(stream: &mut LocalStream) -> io::Result<bool> {
    stream.set_nonblocking(true)?;
    let mut probe = [0u8; 1];
    let status = match stream.read(&mut probe) {
        Ok(0) => Ok(true),
        Ok(_) => Ok(true),
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(err) if is_connection_closed_error(&err) => Ok(true),
        Err(err) => Err(err),
    };
    stream.set_nonblocking(false)?;
    status
}

#[cfg(windows)]
fn probe_stream_closed(stream: &mut LocalStream) -> io::Result<bool> {
    Ok(windows_named_pipe_available(stream)?.is_none())
}

#[cfg(windows)]
fn windows_named_pipe_available(stream: &mut LocalStream) -> io::Result<Option<u32>> {
    use std::os::windows::io::{AsHandle, AsRawHandle};

    let LocalStream::NamedPipe(pipe) = stream;
    let mut available = 0;
    let ok = unsafe {
        windows_sys::Win32::System::Pipes::PeekNamedPipe(
            pipe.as_handle().as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        return Ok(Some(available));
    }

    let err = io::Error::last_os_error();
    if is_connection_closed_error(&err) || windows_named_pipe_closed_error(&err) {
        return Ok(None);
    }
    Err(err)
}

pub(crate) fn is_connection_closed_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::WriteZero
    )
}

#[cfg(windows)]
fn windows_named_pipe_closed_error(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(6 | 109 | 232 | 233))
}

pub(crate) fn socket_file_identity(path: &Path) -> io::Result<SocketFileIdentity> {
    #[cfg(windows)]
    {
        Ok(SocketFileIdentity {
            marker: fs::read(path)?,
        })
    }

    #[cfg(unix)]
    {
        let metadata = fs::metadata(path)?;
        Ok(SocketFileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
}

pub(crate) fn remove_socket_file_if_owned(
    path: &Path,
    identity: &SocketFileIdentity,
) -> io::Result<()> {
    let current = match socket_file_identity(path) {
        Ok(current) => current,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    if current != *identity {
        return Ok(());
    }

    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(windows)]
fn windows_socket_marker() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}:{now}", std::process::id())
}

#[cfg(unix)]
pub(crate) fn restrict_socket_permissions(path: &Path, mode: u32) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)
}

#[cfg(windows)]
pub(crate) fn restrict_socket_permissions(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use interprocess::local_socket::traits::Listener as _;
    #[cfg(windows)]
    use std::path::PathBuf;

    #[test]
    fn stale_socket_connect_errors_keep_unix_would_block_strict() {
        assert!(stale_socket_connect_error(io::ErrorKind::ConnectionRefused));
        assert!(stale_socket_connect_error(io::ErrorKind::NotFound));
        assert!(stale_socket_connect_error(io::ErrorKind::TimedOut));
        assert_eq!(
            stale_socket_connect_error(io::ErrorKind::WouldBlock),
            cfg!(windows)
        );
    }

    #[cfg(windows)]
    #[test]
    fn private_named_pipe_accepts_same_user() {
        use std::io::Write as _;

        let path = temp_socket_marker_path("private-pipe");
        let _ = fs::remove_file(&path);
        let listener = bind_private_local_listener(&path).unwrap();
        let mut client = connect_local_stream(&path).unwrap();
        let mut server = listener.accept().unwrap();
        client.write_all(b"remote").unwrap();

        let mut buffer = [0_u8; 16];
        assert!(matches!(
            poll_local_stream_read_count(&mut server, &mut buffer).unwrap(),
            LocalStreamReadCount::Data(6)
        ));
        assert_eq!(&buffer[..6], b"remote");

        drop(client);
        drop(server);
        drop(listener);
        let _ = fs::remove_file(path);
    }

    #[cfg(windows)]
    #[test]
    fn remove_socket_file_if_owned_compares_windows_marker_contents() {
        let path = temp_socket_marker_path("same-len-marker");
        let _ = fs::remove_file(&path);

        fs::write(&path, b"marker-aa").expect("write first marker");
        let identity = socket_file_identity(&path).expect("read first identity");
        fs::write(&path, b"marker-bb").expect("replace with same-length marker");

        remove_socket_file_if_owned(&path, &identity).expect("remove owned marker");

        assert!(path.exists(), "same-length replacement marker must survive");

        let _ = fs::remove_file(&path);
    }

    #[cfg(windows)]
    #[test]
    fn idle_named_pipe_peer_is_not_treated_as_closed() {
        let path = temp_socket_marker_path("idle-pipe");
        let listener = bind_local_listener(&path).unwrap();
        let _client = connect_local_stream(&path).unwrap();
        let mut server = listener.accept().unwrap();

        assert!(!local_stream_peer_closed(&mut server).unwrap());

        let _ = fs::remove_file(path);
    }

    #[cfg(windows)]
    #[test]
    fn disconnected_named_pipe_peer_is_treated_as_closed() {
        let path = temp_socket_marker_path("disconnected-pipe");
        let listener = bind_local_listener(&path).unwrap();
        let client = connect_local_stream(&path).unwrap();
        let mut server = listener.accept().unwrap();

        drop(client);

        assert!(local_stream_peer_closed(&mut server).unwrap());

        let _ = fs::remove_file(path);
    }

    #[cfg(windows)]
    fn temp_socket_marker_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("herdr-{name}-{}.sock", std::process::id()))
    }
}
