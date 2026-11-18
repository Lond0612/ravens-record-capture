//! rrcap: ゲームの映像と音を取り込み、直前 N 秒をクリップとして保存するプログラム
//!
//! 呼び出し側とは、標準入出力の JSON Lines でやり取りする（protocol.rs）。
//! OBS の部品（obs.dll など）は、この exe と同じフォルダに置く。無ければ bootstrap の命令でダウンロードする。

#![allow(unknown_lints, ensure_obs_call_in_runtime)]

mod capture;
mod protocol;

use libobs_bootstrapper::{ObsBootstrapper, ObsBootstrapperOptions, ObsBootstrapperResult};
use protocol::{Command, Event, emit};
use std::io::BufRead;
use std::sync::mpsc;
use std::time::Duration;

/// ゲームのウィンドウを探し直す間隔
const POLL: Duration = Duration::from_secs(2);

fn installed() -> bool {
    ObsBootstrapper::is_valid_installation().unwrap_or(false)
}

fn error(command: &str, e: impl std::fmt::Display) {
    emit(&Event::Error {
        command: command.into(),
        message: format!("{e:#}"),
    });
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .target(env_logger::Target::Stderr)
        .init();
    emit(&Event::Hello {
        version: env!("CARGO_PKG_VERSION"),
    });

    // 標準入力は別のスレッドで読む（main のスレッドは OBS とゲームのウィンドウ探しに使う）
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio");
    let mut capture: Option<capture::Capture> = None;

    loop {
        let line = match rx.recv_timeout(POLL) {
            Ok(line) => Some(line),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break, // 呼び出し側が閉じた
        };

        if let Some(line) = line {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let command: Command = match serde_json::from_str(line) {
                Ok(c) => c,
                Err(e) => {
                    error("parse", e);
                    continue;
                }
            };
            match command {
                Command::Status => emit(&Event::Status {
                    installed: installed(),
                    capturing: capture.as_ref().is_some_and(|c| c.is_started()),
                }),
                Command::Bootstrap => {
                    if capture.is_some() {
                        error("bootstrap", "取り込み中はダウンロードできません");
                        continue;
                    }
                    match runtime.block_on(ObsBootstrapper::bootstrap(
                        &ObsBootstrapperOptions::default(),
                    )) {
                        Ok(r) => emit(&Event::Bootstrapped {
                            provisioned: matches!(r, ObsBootstrapperResult::Provisioned),
                        }),
                        Err(e) => error("bootstrap", e),
                    }
                }
                Command::Start(settings) => {
                    if capture.is_some() {
                        error("start", "すでに始めています");
                        continue;
                    }
                    if !installed() {
                        error("start", "OBS の部品がありません（先に bootstrap）");
                        continue;
                    }
                    let exe = settings.exe.clone();
                    match capture::Capture::new(settings) {
                        Ok(c) => {
                            capture = Some(c);
                            emit(&Event::WaitingForGame { exe });
                        }
                        Err(e) => error("start", e),
                    }
                }
                Command::Save => match capture.as_ref() {
                    Some(c) => match c.save() {
                        Ok(path) => emit(&Event::Saved { path }),
                        Err(e) => error("save", e),
                    },
                    None => error("save", "まだ始めていません"),
                },
                Command::Stop => {
                    if let Some(c) = capture.take()
                        && let Err(e) = c.stop()
                    {
                        error("stop", e);
                    }
                    emit(&Event::Stopped);
                }
                Command::Quit => break,
            }
        }

        // ゲームのウィンドウが出ていれば、取り込みを始める
        if let Some(c) = capture.as_mut()
            && !c.is_started()
        {
            match c.try_start() {
                Ok(Some(window)) => emit(&Event::Capturing {
                    window,
                    encoder: c.encoder_name(),
                }),
                Ok(None) => {}
                Err(e) => {
                    error("start", e);
                    capture = None;
                }
            }
        }
    }

    if let Some(c) = capture.take() {
        let _ = c.stop();
    }
}
