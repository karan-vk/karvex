use std::fs;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

#[cfg(unix)]
use interprocess::local_socket::traits::Stream as _;

pub(crate) type LocalListener = interprocess::local_socket::Listener;
pub(crate) type LocalStream = interprocess::local_socket::Stream;

pub(crate) enum LocalStreamRead {
    Data,
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

/// Capacity of `sockaddr_un.sun_path` on macOS, the tightest limit across the
/// Unix targets we support (Linux allows 108). Test helpers that build socket
/// paths assert against this so a path that is fine on Linux cannot silently
/// break the macOS CI leg.
#[cfg(unix)]
pub(crate) const MACOS_SUN_PATH_LIMIT: usize = 104;

/// Capacity of `sockaddr_un.sun_path` on Linux.
#[cfg(unix)]
pub(crate) const LINUX_SUN_PATH_LIMIT: usize = 108;

/// This platform's `sockaddr_un.sun_path` capacity, in bytes.
///
/// A pure cross-platform policy constant: both arms compile on every Unix
/// target we ship, so `cfg!` (not `#[cfg]`) is what picks between them, per
/// the platform-code convention in `CLAUDE.md`.
#[cfg(unix)]
pub(crate) fn sun_path_limit() -> usize {
    if cfg!(target_os = "macos") {
        MACOS_SUN_PATH_LIMIT
    } else {
        LINUX_SUN_PATH_LIMIT
    }
}

/// H1: fails fast, with a clear and actionable message, when `path` will not
/// fit in `sockaddr_un.sun_path` on this platform.
///
/// Without this, the failure happens deep inside `bind()`/`connect()` in a
/// server process spawned with its stdio redirected to `/dev/null`
/// (`server::autodetect::build_server_daemon_command`): the process exits
/// before `tracing` ever gets a line to write, leaving a 0-byte
/// `karvex-server.log` and a client that only learns "server did not become
/// ready within 15s" after paying the full wait.
#[cfg(unix)]
pub(crate) fn check_socket_path_len(path: &Path) -> Result<(), String> {
    check_socket_path_len_against(path, sun_path_limit())
}

#[cfg(unix)]
fn check_socket_path_len_against(path: &Path, limit: usize) -> Result<(), String> {
    let len = path.as_os_str().len();
    if len <= limit {
        return Ok(());
    }
    Err(format!(
        "socket path is too long for this platform: {len} bytes, but \
         sockaddr_un.sun_path holds at most {limit}.\n  path: {}\n\n\
         Karvex derives this path from XDG_CONFIG_HOME (or the platform config \
         directory) plus the session name. Fix it with a shorter XDG_CONFIG_HOME \
         or a shorter `--session <name>`.",
        path.display()
    ))
}

#[cfg(all(test, unix))]
mod sun_path_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn macos_and_linux_limits_match_the_documented_values() {
        assert_eq!(MACOS_SUN_PATH_LIMIT, 104);
        assert_eq!(LINUX_SUN_PATH_LIMIT, 108);
    }

    #[test]
    fn a_path_at_or_under_the_limit_passes() {
        let path = PathBuf::from("a".repeat(104));
        assert!(check_socket_path_len_against(&path, MACOS_SUN_PATH_LIMIT).is_ok());
        let path = PathBuf::from("a".repeat(108));
        assert!(check_socket_path_len_against(&path, LINUX_SUN_PATH_LIMIT).is_ok());
    }

    #[test]
    fn a_path_one_byte_over_the_macos_limit_fails_with_the_path_length_and_limit() {
        let path = PathBuf::from("a".repeat(105));
        let err =
            check_socket_path_len_against(&path, MACOS_SUN_PATH_LIMIT).expect_err("over limit");
        assert!(err.contains("105 bytes"), "{err}");
        assert!(err.contains("104"), "{err}");
        assert!(err.contains("XDG_CONFIG_HOME"), "{err}");
        assert!(err.contains("--session"), "{err}");
    }

    #[test]
    fn a_path_one_byte_over_the_linux_limit_fails() {
        let path = PathBuf::from("a".repeat(109));
        let err =
            check_socket_path_len_against(&path, LINUX_SUN_PATH_LIMIT).expect_err("over limit");
        assert!(err.contains("109 bytes"), "{err}");
        assert!(err.contains("108"), "{err}");
    }

    /// The reported H1 repro: a long `XDG_CONFIG_HOME` plus a named session
    /// pushes `.../sessions/<name>/karvex-client.sock` over the limit.
    #[test]
    fn a_realistic_long_xdg_config_home_and_session_name_is_caught() {
        let deep = "x".repeat(80);
        let path = PathBuf::from(format!(
            "/home/user/.config-{deep}/karvex/sessions/a-fairly-long-session-name/karvex-client.sock"
        ));
        assert!(
            check_socket_path_len_against(&path, MACOS_SUN_PATH_LIMIT).is_err(),
            "this path is a realistic overflow and must be caught before bind/connect"
        );
    }

    #[test]
    fn a_typical_short_path_is_never_flagged() {
        let path = PathBuf::from("/home/user/.config/karvex/karvex-client.sock");
        assert!(check_socket_path_len_against(&path, MACOS_SUN_PATH_LIMIT).is_ok());
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

pub(crate) fn poll_local_stream_read(
    stream: &mut LocalStream,
    buf: &mut [u8],
) -> io::Result<LocalStreamRead> {
    #[cfg(unix)]
    {
        match stream.read(buf) {
            Ok(0) => Ok(LocalStreamRead::Closed),
            Ok(_) => Ok(LocalStreamRead::Data),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(LocalStreamRead::Pending),
            Err(err) => Err(err),
        }
    }

    #[cfg(windows)]
    {
        match windows_named_pipe_available(stream)? {
            None => Ok(LocalStreamRead::Closed),
            Some(0) => Ok(LocalStreamRead::Pending),
            Some(_) => match stream.read(buf) {
                Ok(0) => Ok(LocalStreamRead::Closed),
                Ok(_) => Ok(LocalStreamRead::Data),
                Err(err) if is_connection_closed_error(&err) => Ok(LocalStreamRead::Closed),
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
        std::env::temp_dir().join(format!("karvex-{name}-{}.sock", std::process::id()))
    }
}
