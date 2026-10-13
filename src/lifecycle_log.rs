// Low-volume lifecycle diagnostics only: never record keys, text, or window titles.
use std::fs::{self, OpenOptions};
use std::io::Write;
use winapi::um::minwinbase::SYSTEMTIME;
use winapi::um::sysinfoapi::GetLocalTime;

pub fn event(message: &str) {
    let Ok(config) = crate::config::get_config_path() else {
        return;
    };
    let Some(directory) = config.parent() else {
        return;
    };
    let path = directory.join("lifecycle.log");
    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 1_048_576) {
        let previous = directory.join("lifecycle.previous.log");
        let _ = fs::remove_file(&previous);
        let _ = fs::rename(&path, previous);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let mut time: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut time) };
    let _ = writeln!(
        file,
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} pid={} {}",
        time.wYear,
        time.wMonth,
        time.wDay,
        time.wHour,
        time.wMinute,
        time.wSecond,
        std::process::id(),
        message
    );
}
