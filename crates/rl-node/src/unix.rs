//! Unix process-tree control: children lead their own process group, which is signalled as a
//! whole.

/// Sends `signal` to the process group `pgid` leads. `false` when it no longer exists.
pub(crate) fn signal_group(pgid: u32, signal: libc::c_int) -> bool {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return false;
    };
    // SAFETY: plain syscall; a negative pid addresses the process group.
    unsafe { libc::kill(-pgid, signal) == 0 }
}
