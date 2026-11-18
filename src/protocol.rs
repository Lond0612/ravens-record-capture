//! 呼び出し側（RAVEN'S RECORD のコアなど）とのやり取り
//!
//! 標準入力に 1 行 1 件の JSON で命令を受け、標準出力に 1 行 1 件の JSON でイベントを返す。
//! OBS などのログは標準エラーに出す（標準出力はイベントだけ）。

use serde::{Deserialize, Serialize};
use std::io::Write;

/// 命令
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// OBS の部品が使える状態か調べる（ネットワークには出ない）
    Status,
    /// OBS の部品をダウンロードして使える状態にする（済んでいれば何もしない）
    Bootstrap,
    /// 取り込みを始める（ゲームのウィンドウを待つ。録画は record で始める）
    Start(StartSettings),
    /// 録画を始める（path に書く）
    Record { path: String },
    /// 録画を止めて、ファイルを書き終える
    RecordStop,
    /// 録画を止めて、ファイルを消す
    RecordCancel,
    /// 取り込みを止める
    Stop,
    /// 終了する
    Quit,
}

/// 取り込みの設定
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct StartSettings {
    /// 取り込むゲームの実行ファイル名
    pub exe: String,
    /// 取り込みの方式: "game"（ゲームキャプチャ）/ "monitor"（画面全体。動作確認用）
    pub source: String,
    /// 保存先フォルダ
    pub dir: String,
    /// 出力の解像度
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// 映像のビットレート（kbps）
    pub bitrate_kbps: u32,
    /// 圧縮: "h264" / "hevc" / "av1"（GPU の圧縮。使えなければ失敗する）/ "x264"（CPU。明示したときだけ）
    pub codec: String,
    /// ゲームの音を入れる
    pub audio: bool,
}

impl Default for StartSettings {
    fn default() -> Self {
        Self {
            exe: "armoredcore6.exe".into(),
            source: "game".into(),
            dir: ".".into(),
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 12_000,
            codec: "h264".into(),
            audio: true,
        }
    }
}

/// イベント
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// 起動した
    Hello { version: &'static str },
    /// OBS の部品の状態
    Status {
        installed: bool,
        capturing: bool,
        recording: bool,
    },
    /// OBS の部品のダウンロードが済んだ（provisioned: 今回入れたなら true）
    Bootstrapped { provisioned: bool },
    /// ゲームのウィンドウを待っている
    WaitingForGame { exe: String },
    /// 取り込みを始めた（ゲームのウィンドウを見つけた）
    Capturing { window: String, encoder: String },
    /// 録画を始めた
    Recording { path: String },
    /// 録画を止めた（ファイルを書き終えた）
    Recorded { path: String },
    /// 録画を捨てた
    RecordCancelled,
    /// 止めた
    Stopped,
    /// 失敗（command: どの命令で起きたか）
    Error { command: String, message: String },
}

pub fn emit(event: &Event) {
    if let Ok(line) = serde_json::to_string(event) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}
