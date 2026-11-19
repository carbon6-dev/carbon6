//! Spawning workloads without inheriting the daemon's Linux OOM protection.

use std::ffi::OsStr;

/// Use for session subprocesses, including wrappers and keepers. This resets
/// only the child's negative adjustment, not a hosting environment's positive
/// adjustment. A daemon re-exec must retain its own score instead.
pub fn session_command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    let command = tokio::process::Command::new(program);
    #[cfg(target_os = "linux")]
    let command = {
        let mut command = command;
        // SAFETY: the hook only uses stack storage and async-signal-safe syscalls.
        unsafe { command.pre_exec(reset_oom_adjustment) };
        command
    };
    command
}

#[cfg(target_os = "linux")]
fn reset_oom_adjustment() -> std::io::Result<()> {
    use nix::libc;
    use std::io::Error;

    const PATH: &std::ffi::CStr = c"/proc/self/oom_score_adj";
    // SAFETY: we own the descriptors and use valid C strings and buffers.
    unsafe {
        let fd = libc::open(PATH.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(Error::last_os_error());
        }
        let mut first = 0u8;
        let n = libc::read(fd, (&raw mut first).cast(), 1);
        let error = Error::last_os_error();
        libc::close(fd);
        if n < 0 {
            return Err(error);
        }
        if n == 0 {
            return Err(Error::from_raw_os_error(libc::EIO));
        }
        if first != b'-' {
            return Ok(());
        }

        // Fork inherits both the adjustment and its privileged minimum.
        // Raising a negative value to zero does not need CAP_SYS_RESOURCE.
        let fd = libc::open(PATH.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(Error::last_os_error());
        }
        let n = libc::write(fd, b"0".as_ptr().cast(), 1);
        let error = Error::last_os_error();
        libc::close(fd);
        match n {
            1 => Ok(()),
            -1 => Err(error),
            _ => Err(Error::from_raw_os_error(libc::EIO)),
        }
    }
}

/// PTY launchers that don't expose pre-exec can prepend `/bin/sh -c` with this
/// script, a `$0` placeholder, then the original argv. Only shell builtins run
/// before exec, so `/proc/self` names the actual child.
#[cfg(target_os = "linux")]
pub const SESSION_EXEC: &str = r#"
IFS= read -r adjustment < /proc/self/oom_score_adj || exit 126
case "$adjustment" in
    -*) printf 0 > /proc/self/oom_score_adj || exit 126 ;;
esac
exec "$@"
"#;

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_children_do_not_inherit_negative_oom_adjustments() {
        // Run from a system service with OOMScoreAdjust=-500 and User= (non-
        // root) to cover inheritance from a service manager with privileges.
        let score = || std::fs::read_to_string("/proc/self/oom_score_adj").unwrap().trim().parse::<i32>().unwrap();
        let parent = score();
        let native = session_command("cat").arg("/proc/self/oom_score_adj").output().await.unwrap();
        let wrapped = tokio::process::Command::new("/bin/sh")
            .args(["-c", SESSION_EXEC, "c6-session", "cat", "/proc/self/oom_score_adj"])
            .output()
            .await
            .unwrap();
        for child in [native, wrapped] {
            assert!(child.status.success(), "{child:?}");
            assert_eq!(String::from_utf8(child.stdout).unwrap().trim().parse::<i32>().unwrap(), parent.max(0));
        }
        assert_eq!(score(), parent);
    }
}
