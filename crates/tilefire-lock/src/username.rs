use std::{ffi::CStr, io, mem, ptr};

pub fn current() -> io::Result<String> {
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut capacity = if suggested > 0 {
        suggested as usize
    } else {
        16 * 1024
    };
    loop {
        let mut record = unsafe { mem::zeroed::<libc::passwd>() };
        let mut result = ptr::null_mut();
        let mut storage = vec![0_u8; capacity];
        // SAFETY: record and storage are valid writable buffers for the supplied sizes, result
        // points to a valid pointer, and getpwuid_r does not retain any of these pointers.
        let status = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &mut record,
                storage.as_mut_ptr().cast(),
                storage.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && capacity < 1024 * 1024 {
            capacity *= 2;
            continue;
        }
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        if result.is_null() || record.pw_name.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the current user has no account name",
            ));
        }
        // SAFETY: A successful getpwuid_r returns a NUL-terminated pw_name within storage, which
        // remains alive while the name is copied.
        let name = unsafe { CStr::from_ptr(record.pw_name) }
            .to_str()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            .to_owned();
        if name.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the current account name is empty",
            ));
        }
        return Ok(name);
    }
}
