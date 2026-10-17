//! Opt-in foreground ownership for the Emaki lifecycle experiment.
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

pub const OWNER_PID_ENV: &str = "PANERU_EMAKI_OWNER_PID";

pub fn enabled() -> bool {
    std::env::var_os(OWNER_PID_ENV).is_some()
}

fn parse_owner(value: &OsStr, parent: libc::pid_t) -> io::Result<libc::pid_t> {
    let pid = value
        .to_str()
        .and_then(|value| value.parse::<libc::pid_t>().ok());
    match pid {
        Some(pid) if pid > 1 && pid == parent => Ok(pid),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Emaki owner must be the current non-launchd parent PID",
        )),
    }
}

pub fn owner_pid() -> io::Result<Option<libc::pid_t>> {
    std::env::var_os(OWNER_PID_ENV)
        .map(|value| parse_owner(&value, unsafe { libc::getppid() }))
        .transpose()
}

pub fn reject_service_control() -> io::Result<()> {
    if enabled() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Emaki-owned engines cannot control the upstream launch service",
        ));
    }
    Ok(())
}

/// Arm synchronously before setup; wait without polling on a dedicated thread.
pub fn watch_parent(
    parent: libc::pid_t,
    on_exit: impl FnOnce() + Send + 'static,
) -> io::Result<()> {
    let raw = unsafe { libc::kqueue() };
    if raw == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: kqueue returned a new descriptor; this is its sole owner.
    let queue = unsafe { OwnedFd::from_raw_fd(raw) };
    let event = libc::kevent {
        ident: usize::try_from(parent).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    if unsafe {
        libc::kevent(
            queue.as_raw_fd(),
            &raw const event,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    // Registration plus this recheck closes the parent-exit/PID-reuse race.
    let already_gone = unsafe { libc::getppid() } != parent;
    std::thread::Builder::new()
        .name("emaki-parent-exit".into())
        .spawn(move || {
            if !already_gone {
                loop {
                    let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
                    let count = unsafe {
                        libc::kevent(
                            queue.as_raw_fd(),
                            std::ptr::null(),
                            0,
                            event.as_mut_ptr(),
                            1,
                            std::ptr::null(),
                        )
                    };
                    if count >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                    {
                        break;
                    }
                }
            }
            on_exit();
            // A blocked main thread must not leave an orphan managing indefinitely.
            // This exit explicitly provides no graceful-cleanup guarantee.
            std::thread::sleep(Duration::from_secs(5));
            std::process::exit(78);
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_must_match_the_actual_non_launchd_parent() {
        assert_eq!(parse_owner(OsStr::new("42"), 42).unwrap(), 42);
        for value in ["", "0", "1", "-1", "43", "not-a-pid"] {
            assert!(parse_owner(OsStr::new(value), 42).is_err());
        }
    }
}
