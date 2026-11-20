//! 配信・録画のアプリ（OBS Studio など）が動いているかを調べる
//!
//! OBS のゲームキャプチャは、ゲームに入れる部品（graphics-hook）が 1 つの相手にしか映像を渡さない。
//! rrcap が先につなぐと、あとから起動した OBS のゲームキャプチャは最後まで真っ黒になる（2026-11-20 に確かめた）。
//! そのため、これらのアプリが動いている間は、rrcap はゲームキャプチャを使わず、ウィンドウキャプチャ（WGC。ゲームに何も入れない）で録る。

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

/// ゲームキャプチャを譲る相手（実行ファイル名。大文字・小文字は区別しない）
const OBS_EXES: &[&str] = &["obs64.exe", "obs32.exe", "obs.exe", "streamlabs obs.exe", "streamlabs desktop.exe"];

/// OBS などが動いていれば true（調べられなかったときは false: これまでどおりゲームキャプチャを使う）
pub fn obs_running() -> bool {
    process_names().iter().any(|n| OBS_EXES.iter().any(|o| n.eq_ignore_ascii_case(o)))
}

fn process_names() -> Vec<String> {
    let mut names = Vec::new();
    // SAFETY: スナップショットは関数の中で閉じる。PROCESSENTRY32W は dwSize を入れてから渡す
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE || snap.is_null() {
            return names;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut entry) != 0;
        while ok {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            names.push(String::from_utf16_lossy(&entry.szExeFile[..len]));
            ok = Process32NextW(snap, &mut entry) != 0;
        }
        CloseHandle(snap);
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_own_process() {
        let me = std::env::current_exe().unwrap();
        let me = me.file_name().unwrap().to_string_lossy().to_string();
        assert!(process_names().iter().any(|n| n.eq_ignore_ascii_case(&me)));
    }
}
