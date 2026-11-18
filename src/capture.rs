//! OBS（libobs）での取り込みと録画
//!
//! OBS の呼び出しは、すべて main のスレッドから行う（このモジュールの値は別のスレッドに渡さない）。
//! ゲームキャプチャは、ゲームのウィンドウが出てから作る（それまでは WaitingForGame）。
//! 録画の出力は 1 つを使い回す（録画のたびに保存先だけ変える）。止めると、ファイルを書き終えるまで待つ。

use anyhow::{Context, Result, anyhow, bail};
use libobs_simple::output::simple::{
    HardwareCodec, HardwarePreset, ObsContextSimpleExt, X264Preset,
};
use libobs_simple::sources::ObsSourceBuilder;
use libobs_simple::sources::windows::{
    GameCaptureSourceBuilder, MonitorCaptureSourceBuilder, ObsGameCaptureMode,
};
use libobs_window_helper::{WindowSearchMode, get_all_windows};
use libobs_wrapper::context::ObsContext;
use libobs_wrapper::data::ObsDataSetters;
use libobs_wrapper::data::object::ObsObjectTrait;
use libobs_wrapper::data::output::{ObsOutputRef, ObsOutputTrait};
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
    output: ObsOutputRef,
    /// ゲームキャプチャを作って取り込みを始めたか
    started: bool,
    /// 録画中のファイル
    recording: Option<PathBuf>,
}

impl Capture {
    /// OBS を起動し、録画の出力を用意する（取り込みはまだ始めない）
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
        let placeholder = Path::new(&settings.dir).join("rrcap_recording.mp4");
        let builder = ctx
            .simple_output_builder(
                "rrcap_recording",
                ObsPath::new(&placeholder.display().to_string()),
            )
            .video_bitrate(settings.bitrate_kbps)
            .audio_bitrate(160);
        let builder = match settings.codec.as_str() {
            "h264" => {
                builder.require_hardware_encoder(HardwareCodec::H264, HardwarePreset::Balanced)
            }
            "hevc" => {
                builder.require_hardware_encoder(HardwareCodec::HEVC, HardwarePreset::Balanced)
            }
            "av1" => builder.require_hardware_encoder(HardwareCodec::AV1, HardwarePreset::Balanced),
            "x264" => builder.x264_encoder(X264Preset::VeryFast),
            other => bail!("知らない圧縮の方式です: {other}"),
        };
        let output = builder
            .build()
            .context("録画を用意できませんでした（GPU の圧縮が使えない可能性）")?;
        let scene = ctx.scene("rrcap_scene", Some(0))?;
        Ok(Self {
            settings,
            ctx,
            scene,
            output,
            started: false,
            recording: None,
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
        self.started = true;
        Ok(Some(label))
    }

    /// 録画を始める（path に書く）。録画中なら、いまの録画を捨ててから始める
    pub fn record(&mut self, path: &str) -> Result<()> {
        if !self.started {
            bail!("まだ取り込みを始めていません");
        }
        if self.recording.is_some() {
            self.cancel()?;
        }
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut data = self.ctx.data()?;
        data.set_string("path", path.display().to_string())?;
        self.output.update_settings(data)?;
        self.output.start().context("録画を始められませんでした")?;
        self.recording = Some(path);
        Ok(())
    }

    /// 録画を止めて（ファイルを書き終えるまで待つ）、ファイルの場所を返す
    pub fn finish(&mut self) -> Result<String> {
        let path = self
            .recording
            .take()
            .ok_or_else(|| anyhow!("録画していません"))?;
        self.output.stop().context("録画を止められませんでした")?;
        Ok(path.display().to_string())
    }

    /// 録画を止めて、ファイルを消す
    pub fn cancel(&mut self) -> Result<()> {
        let Some(path) = self.recording.take() else {
            return Ok(());
        };
        let stopped = self.output.stop();
        let _ = std::fs::remove_file(&path); // 書きかけのファイル。消せなくても、次の録画は始められる
        stopped.context("録画を止められませんでした")?;
        Ok(())
    }

    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    pub fn encoder_name(&self) -> String {
        self.settings.codec.clone()
    }

    /// 終わる: 録画中なら、書きかけのファイルは消す（試合の途中で終わった録画は使わない）
    pub fn stop(mut self) -> Result<()> {
        self.cancel()
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

/// 使う OBS のプラグイン。配信の宛先の取得（rtmp-services。起動時にネットへ出る）や、使わない入力・フィルターは読み込まない
const PLUGINS: &[&str] = &[
    "win-capture.dll",
    "win-wasapi.dll",
    "obs-ffmpeg.dll",
    "obs-outputs.dll", // Hybrid MP4（録画中に落ちても途中まで再生できる）の書き出し
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
