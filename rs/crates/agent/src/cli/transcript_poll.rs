//! Whether a stream's reader has gone (Go: internal/cli/transcript_poll.go,
//! and transcript_poll_other.go for the systems it can't tell on).

/// Reports whether the reader of fd, a pipe or a socket, has closed its end,
/// without writing to it: poll reports POLLERR (a pipe on Linux) or POLLHUP (a
/// socket; a pipe on macOS). macOS reports either only when POLLOUT is asked
/// for, so it is, though a full pipe never has room. A terminal or a file
/// reports neither (macOS's /dev/null: POLLNVAL).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn reader_gone(fd: std::os::fd::RawFd) -> bool {
    let mut fds = [libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    }];
    // SAFETY: one pollfd, a zero timeout; fd is only polled, never used.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) };
    n > 0 && fds[0].revents & (libc::POLLERR | libc::POLLHUP) != 0
}

/// It can't tell here: a closed reader is noticed at the next write.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn reader_gone(_fd: std::os::fd::RawFd) -> bool {
    false
}
