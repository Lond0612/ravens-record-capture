# ravens-record-capture (rrcap)

ゲームの映像と音を [libobs](https://github.com/obsproject/obs-studio)（[libobs-rs](https://github.com/libobs-rs/libobs-rs)）で取り込み、直前 N 秒をクリップとして保存するプログラムです（試作中）。
呼び出し側とは、標準入出力の JSON Lines（1 行 1 件の JSON）でやり取りします。

## 命令（標準入力）

| 命令 | 内容 |
|---|---|
| `{"cmd":"status"}` | OBS の部品があるか・取り込み中か |
| `{"cmd":"bootstrap"}` | OBS の部品を OBS 公式のリリースからダウンロードする（exe と同じフォルダ） |
| `{"cmd":"start", ...設定}` | ゲームのウィンドウを待って、取り込みとリプレイバッファを始める |
| `{"cmd":"save"}` | 直前 N 秒を保存する |
| `{"cmd":"stop"}` / `{"cmd":"quit"}` | 止める / 終了する |

`start` の設定（すべて省略できます）: `exe`（既定 `armoredcore6.exe`）・`source`（`game` / `monitor`）・`seconds`・`dir`・`width`・`height`・`fps`・`bitrate_kbps`・`codec`（`h264` / `hevc` / `av1` は GPU の圧縮、`x264` は CPU）・`audio`（ゲームの音）

## イベント（標準出力）

`hello`・`status`・`bootstrapped`・`waiting_for_game`・`capturing`・`saved`（`path`）・`stopped`・`error`。OBS のログは標準エラーに出ます。

## ビルド

Windows・Rust（MSVC）で `cargo build`。

## ライセンス

GPL-3.0-or-later（libobs-rs に合わせています）。OBS Studio は GPL-2.0-or-later です。
