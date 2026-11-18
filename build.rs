fn main() {
    // obs.dll を遅延読み込みにする。OBS の部品が無くても main() まで進めて、先にダウンロード（bootstrap）できるように
    libobs_bootstrapper::build::emit_windows_obs_delay_load();
}
