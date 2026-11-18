//! OBS（libobs）での取り込みとリプレイバッファ
//!
//! OBS の呼び出しは、すべて main のスレッドから行う（このモジュールの値は別のスレッドに渡さない）。
//! ゲームキャプチャは、ゲームのウィンドウが出てから作る（それまでは WaitingForGame）。

use anyhow::{Context, Result, anyhow, bail};
use libobs_simple::output::replay::ObsContextReplayExt;
use libobs_simple::output::simple::{HardwareCodec, HardwarePreset, X264Preset};
use libobs_simple::sources::ObsSourceBuilder;
use libobs_simple::sources::windows::{
    GameCaptureSourceBuilder, MonitorCaptureSourceBuilder, ObsGameCaptureMode,
};
use libobs_window_helper::{WindowSearchMode, get_all_windows};
use libobs_wrapper::context::ObsContext;
use libobs_wrapper::data::output::{ObsOutputTrait, ObsReplayBufferOutputRef};
use libobs_wrapper::data::video::ObsVideoInfoBuilder;
use libobs_wrapper::enums::ObsLogLevel;
use libobs_wrapper::logger::ObsLogger;
use libobs_wrapper::scenes::{ObsSceneRef, SceneItemTrait};
use libobs_wrapper::utils::{ObsPath, StartupInfo, StartupPaths};
use std::path::{Path, PathBuf};

use crate::protocol::StartSettings;

pub struct Capture {
    settings: StartSettings,
    ctx: ObsContext,
    scene: ObsSceneRef,
    replay: ObsReplayBufferOutputRef,
    /// ゲームキャプチャを作って取り込みを始めたか
    started: bool,
}

impl Capture {
    /// OBS を起動し、リプレイバッファを用意する（取り込みはまだ始めない）
    pub fn new(settings: StartSettings) -> Result<Self> {
        let video = ObsVideoInfoBuilder::new()
            .base_width(settings.width)
            .base_height(settings.height)
            .output_width(settings.width)
            .output_height(settings.height)
            .fps_num(settings.fps)
            .fps_den(1)
            .build();
        let paths = StartupPaths::new(
            ObsPath::from_relative("data/libobs"),
            ObsPath::new(&prepare_plugins()?.display().to_string()),
            ObsPath::from_relative("data/obs-plugins/%module%"),
        );
        // プラグインの設定ファイルの置き場（指定しないと、起動したときのフォルダに書かれる）
        let config_dir = exe_dir()?.join("rrcap-config");
        std::fs::create_dir_all(&config_dir)?;
        let mut ctx = StartupInfo::new()
            .set_startup_paths(paths)
            .set_module_config_path(ObsPath::new(&config_dir.display().to_string()))
            .set_video_info(video)
            .set_logger(Box::new(StderrLogger))
            .start()
            .context("OBS を起動できませんでした")?;

        std::fs::create_dir_all(&settings.dir)
            .with_context(|| format!("保存先を作れませんでした: {}", settings.dir))?;
        let builder = ctx
            .replay_buffer_builder("rrcap_replay", ObsPath::new(&settings.dir))
            .max_time_sec(settings.seconds)
            // 保持の上限（MB）: ビットレートから見積もった量の 2 倍。秒数で切れるより先に容量で切れないように
            .max_size_mb(
                ((settings.bitrate_kbps as i64 + 192) * settings.seconds / 8 / 1000 * 2).max(100),
            )
            .video_bitrate(settings.bitrate_kbps)
            .audio_bitrate(160)
            .format("%CCYY-%MM-%DD_%hh-%mm-%ss")
            .allow_spaces(false);
        let builder = match settings.codec.as_str() {
            "h264" => builder.hardware_encoder(HardwareCodec::H264, HardwarePreset::Balanced),
            "hevc" => builder.hardware_encoder(HardwareCodec::HEVC, HardwarePreset::Balanced),
            "av1" => builder.hardware_encoder(HardwareCodec::AV1, HardwarePreset::Balanced),
            "x264" => builder.x264_encoder(X264Preset::VeryFast),
            other => bail!("知らない圧縮の方式です: {other}"),
        };
        let replay = builder
            .build()
            .context("リプレイバッファを用意できませんでした（GPU の圧縮が使えない可能性）")?;
        let scene = ctx.scene("rrcap_scene", Some(0))?;
        Ok(Self {
            settings,
            ctx,
            scene,
            replay,
            started: false,
        })
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    /// まだ取り込みを始めていなければ、ゲームのウィンドウを探して始める。始めたらウィンドウの名前を返す
    pub fn try_start(&mut self) -> Result<Option<String>> {
        if self.started {
            return Ok(None);
        }
        let label = match self.settings.source.as_str() {
            "monitor" => {
                let monitors = MonitorCaptureSourceBuilder::get_monitors()?;
                let monitor = monitors
                    .first()
                    .ok_or_else(|| anyhow!("画面が見つかりません"))?;
                let item = self
                    .ctx
                    .source_builder::<MonitorCaptureSourceBuilder, _>("rrcap_monitor")?
                    .set_monitor(monitor)
                    .add_to_scene(&mut self.scene)?;
                item.fit_source_to_screen()?;
                "monitor".to_string()
            }
            "game" => {
                let Some(window) = find_window(&self.settings.exe)? else {
                    return Ok(None);
                };
                let builder = self
                    .ctx
                    .source_builder::<GameCaptureSourceBuilder, _>("rrcap_game")?
                    .set_capture_mode(ObsGameCaptureMode::CaptureSpecificWindow)
                    .set_window(&window)
                    .set_capture_cursor(false)
                    .set_capture_overlays(false)
                    .set_anti_cheat_hook(true); // アンチチートのあるゲーム向けの方式（OBS の既定と同じ）
                let builder = if self.settings.audio {
                    builder.set_capture_audio(true)?
                } else {
                    builder
                };
                let item = builder.add_to_scene(&mut self.scene)?;
                item.fit_source_to_screen()?;
                window
                    .title
                    .clone()
                    .unwrap_or_else(|| self.settings.exe.clone())
            }
            other => bail!("知らない取り込みの方式です: {other}"),
        };
        self.replay
            .start()
            .context("リプレイバッファを始められませんでした")?;
        self.started = true;
        Ok(Some(label))
    }

    /// 直前 N 秒を保存し、ファイルの場所を返す
    pub fn save(&self) -> Result<String> {
        if !self.started {
            bail!("まだ取り込みを始めていません");
        }
        let path = self.replay.save_buffer().context("保存できませんでした")?;
        Ok(path.display().to_string())
    }

    pub fn encoder_name(&self) -> String {
        self.settings.codec.clone()
    }

    pub fn stop(self) -> Result<()> {
        if self.started {
            self.replay.stop()?;
        }
        Ok(())
    }
}

/// 実行ファイル名が exe のウィンドウ（最小化しているものも含む）
fn find_window(exe: &str) -> Result<Option<libobs_window_helper::WindowInfo>> {
    let windows = get_all_windows(WindowSearchMode::IncludeMinimized)
        .map_err(|e| anyhow!("ウィンドウの一覧を取れませんでした: {e:?}"))?;
    Ok(windows.into_iter().find(|w| {
        Path::new(&w.full_exe)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case(exe))
    }))
}

fn exe_dir() -> Result<PathBuf> {
    Ok(std::env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow!("exe のフォルダが分かりません"))?
        .to_path_buf())
}

/// 使う OBS のプラグイン。配信用（rtmp-services など。起動時にネットへ出る）や、使わない入力・フィルターは読み込まない
const PLUGINS: &[&str] = &[
    "win-capture.dll",
    "win-wasapi.dll",
    "obs-ffmpeg.dll",
    "obs-nvenc.dll",
    "obs-qsv11.dll",
    "obs-x264.dll",
];

/// 使うプラグインだけを、exe と同じフォルダの rrcap-plugins に写す（OBS の部品のフォルダはそのままにする）
fn prepare_plugins() -> Result<PathBuf> {
    let base = exe_dir()?;
    let src = base.join("obs-plugins").join("64bit");
    let dst = base.join("rrcap-plugins");
    std::fs::create_dir_all(&dst)?;
    for name in PLUGINS {
        let from = src.join(name);
        let to = dst.join(name);
        let stale = match (std::fs::metadata(&from), std::fs::metadata(&to)) {
            (Ok(a), Ok(b)) => a.len() != b.len() || a.modified().ok() > b.modified().ok(),
            (Ok(_), Err(_)) => true,
            (Err(_), _) => bail!("OBS のプラグインがありません: {name}"),
        };
        if stale {
            std::fs::copy(&from, &to)
                .with_context(|| format!("プラグインを写せませんでした: {name}"))?;
        }
    }
    Ok(dst)
}

/// OBS のログは標準エラーへ（標準出力はイベントだけにする）
#[derive(Debug)]
struct StderrLogger;

impl ObsLogger for StderrLogger {
    fn log(&mut self, level: ObsLogLevel, msg: String) {
        eprintln!("[obs {level:?}] {msg}");
    }
}
