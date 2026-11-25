use std::{
    fs,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

use rustix::net::sockopt::socket_peercred;
use tilefire_core::LaunchIdentity;

const PROC_LIMIT: usize = 1024 * 1024;

pub(crate) fn capture(stream: &UnixStream) -> LaunchIdentity {
    let Ok(credentials) = socket_peercred(stream) else {
        return LaunchIdentity::default();
    };
    let pid = credentials.pid.as_raw_nonzero().get();
    let root = PathBuf::from(format!("/proc/{pid}"));
    let command = fs::read(root.join("cmdline"))
        .ok()
        .map(|bytes| parse_strings(&bytes))
        .unwrap_or_default();
    let working_directory = fs::read_link(root.join("cwd"))
        .ok()
        .map(|path| path.to_string_lossy().into_owned());
    let launch_id = fs::read(root.join("environ")).ok().and_then(|bytes| {
        parse_strings(&bytes).into_iter().find_map(|entry| {
            entry.strip_prefix("TILEFIRE_LAUNCH_ID=").map(str::to_owned)
        })
    });
    let app_id = command
        .first()
        .and_then(|program| {
            Path::new(program)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    LaunchIdentity {
        app_id,
        launch_id,
        command,
        working_directory,
        title: None,
        launch_group: Some(format!("pid-{pid}")),
    }
}

fn parse_strings(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .take(PROC_LIMIT)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}
