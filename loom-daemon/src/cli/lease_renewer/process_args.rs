//! Boundary-preserving argv reads for the renewer signal gate. Display text
//! from `ps` cannot prove ownership: whitespace in one filename looks like
//! several arguments. An unavailable native read must fail closed.

#[cfg(target_os = "linux")]
pub(super) fn process_argv(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let raw = raw.strip_suffix(&[0])?;
    let argv: Vec<String> = raw
        .split(|byte| *byte == 0)
        .map(|arg| String::from_utf8(arg.to_vec()).ok())
        .collect::<Option<_>>()?;
    argv.first().filter(|arg| !arg.is_empty())?;
    Some(argv)
}

#[cfg(target_os = "macos")]
pub(super) fn process_argv(pid: u32) -> Option<Vec<String>> {
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROCARGS2,
        libc::c_int::try_from(pid).ok()?,
    ];
    let mut size: libc::size_t = 0;
    // SAFETY: size query, with no output buffer and a valid size pointer.
    let rc = unsafe {
        libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0)
    };
    if rc != 0 || size == 0 {
        return None;
    }
    let mut buffer = vec![0_u8; size];
    // SAFETY: buffer owns `size` writable bytes; all pointers remain live.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size > buffer.len() {
        return None;
    }
    buffer.truncate(size);
    parse_procargs2(&buffer)
}

/// Native argc, executable path, NUL padding, then argc terminated argv
/// entries. Preserve empty arguments and stop before the environment.
#[cfg(any(target_os = "macos", test))]
fn parse_procargs2(buffer: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?);
    let argc = usize::try_from(argc)
        .ok()
        .filter(|n| *n > 0 && *n <= buffer.len())?;
    let rest = &buffer[4..];
    let rest = &rest[rest.iter().position(|byte| *byte == 0)?..];
    let mut rest = &rest[rest.iter().position(|byte| *byte != 0)?..];
    let mut argv = Vec::with_capacity(argc);
    for _ in 0..argc {
        let end = rest.iter().position(|byte| *byte == 0)?;
        argv.push(std::str::from_utf8(&rest[..end]).ok()?.to_owned());
        rest = &rest[end + 1..];
    }
    Some(argv)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn process_argv(_pid: u32) -> Option<Vec<String>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed(args: &[&str]) -> Vec<u8> {
        let mut bytes = i32::try_from(args.len()).unwrap().to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/bin/bash\0\0\0");
        for arg in args {
            bytes.extend_from_slice(arg.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    #[test]
    fn native_args_preserve_spaces_empty_arguments_and_exclude_environment() {
        let args = ["/bin/bash", "/work tree/sweep-lease-renew.sh", "", "start"];
        let mut bytes = packed(&args);
        bytes.extend_from_slice(b"IGNORED=value\0");
        assert_eq!(parse_procargs2(&bytes).unwrap(), args);
        let false_positive = ["/bin/bash", "/work/sweep-lease-renew.sh start 7"];
        let argv = parse_procargs2(&packed(&false_positive)).unwrap();
        assert!(!super::super::is_renewer_argv(&argv));
        let valid = ["/bin/bash", "/work tree/sweep-lease-renew.sh", "start", "7"];
        assert!(super::super::is_renewer_argv(&parse_procargs2(&packed(&valid)).unwrap()));
    }

    #[test]
    fn malformed_native_args_fail_closed() {
        assert!(parse_procargs2(&[]).is_none());
        assert!(parse_procargs2(&packed(&[])).is_none());
        let mut bytes = packed(&["bash", "script"]);
        bytes.pop();
        assert!(parse_procargs2(&bytes).is_none(), "unterminated final argument");
        assert!(parse_procargs2(&(-1_i32).to_ne_bytes()).is_none());
        let mut bytes = packed(&["bash"]);
        bytes[0..4].copy_from_slice(&2_i32.to_ne_bytes());
        assert!(parse_procargs2(&bytes).is_none(), "missing argument");
        let mut bytes = packed(&["bash"]);
        bytes[4] = 0xff;
        // The executable path is not interpreted; invalid argv must refuse.
        *bytes.iter_mut().rev().nth(1).unwrap() = 0xff;
        assert!(parse_procargs2(&bytes).is_none());
    }
}
