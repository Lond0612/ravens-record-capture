//! 録画した MP4 の前の部分を切り落とす（試合のクリップを、ALPHA の側の紹介の少し前から始めるため）
//!
//! 圧縮をやり直さずに、ファイルの中身（サンプル）をそのまま写す。映像はキーフレームでしか切れないので、
//! 指定した秒数より前で、いちばん近いキーフレームから始める（録画はキーフレームを 1 秒ごとに打つので、指定の 0〜1 秒前）。
//! 映像・音声の圧縮の方式に関係なく使えるよう、索引（moov）の表だけを作り直し、サンプルの説明（stsd）はそのまま使う。
//! 書き出すファイルは ftyp・moov・mdat の順（索引が先なので、すぐ再生を始められる）。

use anyhow::{Context, Result, anyhow, bail};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// 箱（box）。入れ物の箱は中身を読んで children に、それ以外は中身をそのまま data に持つ
#[derive(Clone, Debug)]
struct Atom {
    kind: [u8; 4],
    data: Vec<u8>,
    children: Option<Vec<Atom>>,
}

const CONTAINERS: &[&[u8; 4]] = &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"edts"];

impl Atom {
    fn is(&self, k: &[u8; 4]) -> bool {
        &self.kind == k
    }
    fn child(&self, k: &[u8; 4]) -> Option<&Atom> {
        self.children.as_ref()?.iter().find(|a| a.is(k))
    }
    fn child_mut(&mut self, k: &[u8; 4]) -> Option<&mut Atom> {
        self.children.as_mut()?.iter_mut().find(|a| a.is(k))
    }
    fn size(&self) -> u64 {
        8 + match &self.children {
            Some(c) => c.iter().map(Atom::size).sum(),
            None => self.data.len() as u64,
        }
    }
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.size() as u32).to_be_bytes());
        out.extend_from_slice(&self.kind);
        match &self.children {
            Some(c) => c.iter().for_each(|a| a.write(out)),
            None => out.extend_from_slice(&self.data),
        }
    }
    fn leaf(kind: &[u8; 4], data: Vec<u8>) -> Atom {
        Atom { kind: *kind, data, children: None }
    }
}

fn parse_atoms(buf: &[u8]) -> Result<Vec<Atom>> {
    let mut atoms = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= buf.len() {
        let mut size = u32::from_be_bytes(buf[pos..pos + 4].try_into()?) as u64;
        let kind: [u8; 4] = buf[pos + 4..pos + 8].try_into()?;
        let mut header = 8u64;
        if size == 1 {
            size = u64::from_be_bytes(buf.get(pos + 8..pos + 16).ok_or_else(|| anyhow!("箱が途中で切れています"))?.try_into()?);
            header = 16;
        } else if size == 0 {
            size = (buf.len() - pos) as u64;
        }
        let end = pos + size as usize;
        if size < header || end > buf.len() {
            bail!("箱の大きさがおかしいです");
        }
        let body = &buf[pos + header as usize..end];
        let atom = if CONTAINERS.contains(&&kind) {
            Atom { kind, data: Vec::new(), children: Some(parse_atoms(body)?) }
        } else {
            Atom::leaf(&kind, body.to_vec())
        };
        atoms.push(atom);
        pos = end;
    }
    Ok(atoms)
}

fn be32(b: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(b.get(at..at + 4).ok_or_else(|| anyhow!("表が途中で切れています"))?.try_into()?))
}
fn be64(b: &[u8], at: usize) -> Result<u64> {
    Ok(u64::from_be_bytes(b.get(at..at + 8).ok_or_else(|| anyhow!("表が途中で切れています"))?.try_into()?))
}

/// mvhd / mdhd: (timescale, duration) の位置。version 0 と 1 で位置が違う
fn time_fields(data: &[u8]) -> Result<(u32, u64, bool)> {
    let v1 = data.first() == Some(&1);
    Ok(if v1 { (be32(data, 20)?, be64(data, 24)?, true) } else { (be32(data, 12)?, be32(data, 16)? as u64, false) })
}
fn set_duration(data: &mut [u8], duration: u64) {
    if data.first() == Some(&1) {
        data[24..32].copy_from_slice(&duration.to_be_bytes());
    } else {
        data[16..20].copy_from_slice(&(duration.min(u32::MAX as u64) as u32).to_be_bytes());
    }
}
fn set_tkhd_duration(data: &mut [u8], duration: u64) {
    if data.first() == Some(&1) {
        data[28..36].copy_from_slice(&duration.to_be_bytes());
    } else {
        data[20..24].copy_from_slice(&(duration.min(u32::MAX as u64) as u32).to_be_bytes());
    }
}

/// トラックの全サンプル
struct Track {
    handler: [u8; 4],
    timescale: u32,
    sizes: Vec<u32>,
    offsets: Vec<u64>,
    durations: Vec<u32>,
    ctts: Option<(u8, Vec<i64>)>,
    sync: Option<Vec<bool>>,
    desc_index: u32,
    media_time: Option<i64>, // edts/elst の最後の空でない区間の media_time
}

impl Track {
    fn decode_times(&self) -> Vec<u64> {
        let mut t = 0u64;
        self.durations.iter().map(|d| { let s = t; t += *d as u64; s }).collect()
    }
}

fn read_track(trak: &Atom) -> Result<Track> {
    let mdia = trak.child(b"mdia").ok_or_else(|| anyhow!("mdia がありません"))?;
    let (timescale, _, _) = time_fields(&mdia.child(b"mdhd").ok_or_else(|| anyhow!("mdhd がありません"))?.data)?;
    let hdlr = &mdia.child(b"hdlr").ok_or_else(|| anyhow!("hdlr がありません"))?.data;
    let handler: [u8; 4] = hdlr.get(8..12).ok_or_else(|| anyhow!("hdlr が短すぎます"))?.try_into()?;
    let stbl = mdia.child(b"minf").and_then(|m| m.child(b"stbl")).ok_or_else(|| anyhow!("stbl がありません"))?;

    // 大きさ
    let stsz = &stbl.child(b"stsz").ok_or_else(|| anyhow!("stsz がありません"))?.data;
    let fixed = be32(stsz, 4)?;
    let count = be32(stsz, 8)? as usize;
    let sizes: Vec<u32> = if fixed != 0 { vec![fixed; count] } else { (0..count).map(|i| be32(stsz, 12 + i * 4)).collect::<Result<_>>()? };

    // 長さ
    let stts = &stbl.child(b"stts").ok_or_else(|| anyhow!("stts がありません"))?.data;
    let mut durations = Vec::with_capacity(count);
    for i in 0..be32(stts, 4)? as usize {
        let (n, d) = (be32(stts, 8 + i * 8)?, be32(stts, 12 + i * 8)?);
        durations.extend(std::iter::repeat_n(d, n as usize));
    }
    if durations.len() != count {
        bail!("サンプルの数が合いません（stts）");
    }

    // 表示のずれ
    let ctts = match stbl.child(b"ctts") {
        Some(a) => {
            let version = a.data[0];
            let mut v = Vec::with_capacity(count);
            for i in 0..be32(&a.data, 4)? as usize {
                let n = be32(&a.data, 8 + i * 8)?;
                let raw = be32(&a.data, 12 + i * 8)?;
                let off = if version == 1 { raw as i32 as i64 } else { raw as i64 };
                v.extend(std::iter::repeat_n(off, n as usize));
            }
            if v.len() != count {
                bail!("サンプルの数が合いません（ctts）");
            }
            Some((version, v))
        }
        None => None,
    };

    // キーフレーム
    let sync = match stbl.child(b"stss") {
        Some(a) => {
            let mut v = vec![false; count];
            for i in 0..be32(&a.data, 4)? as usize {
                let n = be32(&a.data, 8 + i * 4)? as usize;
                if (1..=count).contains(&n) {
                    v[n - 1] = true;
                }
            }
            Some(v)
        }
        None => None,
    };

    // 場所（チャンク → サンプル）
    let chunk_offsets: Vec<u64> = if let Some(a) = stbl.child(b"stco") {
        (0..be32(&a.data, 4)? as usize).map(|i| be32(&a.data, 8 + i * 4).map(u64::from)).collect::<Result<_>>()?
    } else if let Some(a) = stbl.child(b"co64") {
        (0..be32(&a.data, 4)? as usize).map(|i| be64(&a.data, 8 + i * 8)).collect::<Result<_>>()?
    } else {
        bail!("stco / co64 がありません");
    };
    let stsc = &stbl.child(b"stsc").ok_or_else(|| anyhow!("stsc がありません"))?.data;
    let entries: Vec<(u32, u32, u32)> = (0..be32(stsc, 4)? as usize)
        .map(|i| Ok((be32(stsc, 8 + i * 12)?, be32(stsc, 12 + i * 12)?, be32(stsc, 16 + i * 12)?)))
        .collect::<Result<_>>()?;
    let mut offsets = Vec::with_capacity(count);
    let mut sample = 0usize;
    let mut desc_index = 1;
    for (ci, &chunk_start) in chunk_offsets.iter().enumerate() {
        let chunk_no = ci as u32 + 1;
        let Some(&(_, per, desc)) = entries.iter().rev().find(|e| e.0 <= chunk_no) else { bail!("stsc がおかしいです") };
        if ci == 0 {
            desc_index = desc;
        }
        let mut at = chunk_start;
        for _ in 0..per {
            if sample >= count {
                break;
            }
            offsets.push(at);
            at += sizes[sample] as u64;
            sample += 1;
        }
    }
    if offsets.len() != count {
        bail!("サンプルの数が合いません（stsc / stco）");
    }

    let media_time = trak.child(b"edts").and_then(|e| e.child(b"elst")).and_then(|elst| {
        let d = &elst.data;
        let v1 = d.first() == Some(&1);
        let n = be32(d, 4).ok()? as usize;
        let mut last = None;
        for i in 0..n {
            let mt = if v1 { be64(d, 8 + i * 20 + 8).ok()? as i64 } else { be32(d, 8 + i * 12 + 4).ok()? as i32 as i64 };
            if mt >= 0 {
                last = Some(mt);
            }
        }
        last
    });

    Ok(Track { handler, timescale, sizes, offsets, durations, ctts, sync, desc_index, media_time })
}

fn full_box(version: u8, body: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut v = vec![version, 0, 0, 0];
    body(&mut v);
    v
}

/// サンプル first.. だけの表に作り直す（stbl の中身。stsd は元のまま）。co64 の値は、あとで書き込む位置で埋める
fn rebuild_stbl(stbl: &mut Atom, t: &Track, first: usize) {
    let n = t.sizes.len() - first;
    let keep = |a: &Atom| a.is(b"stsd");
    let mut children: Vec<Atom> = stbl.children.take().unwrap_or_default().into_iter().filter(keep).collect();

    // stts（連続する同じ長さをまとめる）
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &d in &t.durations[first..] {
        match runs.last_mut() {
            Some((c, dd)) if *dd == d => *c += 1,
            _ => runs.push((1, d)),
        }
    }
    children.push(Atom::leaf(b"stts", full_box(0, |v| {
        v.extend_from_slice(&(runs.len() as u32).to_be_bytes());
        for (c, d) in &runs {
            v.extend_from_slice(&c.to_be_bytes());
            v.extend_from_slice(&d.to_be_bytes());
        }
    })));
    if let Some((version, offs)) = &t.ctts {
        let mut runs: Vec<(u32, i64)> = Vec::new();
        for &o in &offs[first..] {
            match runs.last_mut() {
                Some((c, oo)) if *oo == o => *c += 1,
                _ => runs.push((1, o)),
            }
        }
        children.push(Atom::leaf(b"ctts", full_box(*version, |v| {
            v.extend_from_slice(&(runs.len() as u32).to_be_bytes());
            for (c, o) in &runs {
                v.extend_from_slice(&c.to_be_bytes());
                v.extend_from_slice(&(*o as i32 as u32).to_be_bytes());
            }
        })));
    }
    if let Some(sync) = &t.sync {
        let idx: Vec<u32> = sync[first..].iter().enumerate().filter(|(_, s)| **s).map(|(i, _)| i as u32 + 1).collect();
        children.push(Atom::leaf(b"stss", full_box(0, |v| {
            v.extend_from_slice(&(idx.len() as u32).to_be_bytes());
            idx.iter().for_each(|i| v.extend_from_slice(&i.to_be_bytes()));
        })));
    }
    // 1 チャンクに 1 サンプル
    children.push(Atom::leaf(b"stsc", full_box(0, |v| {
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(&t.desc_index.to_be_bytes());
    })));
    children.push(Atom::leaf(b"stsz", full_box(0, |v| {
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend_from_slice(&(n as u32).to_be_bytes());
        t.sizes[first..].iter().for_each(|s| v.extend_from_slice(&s.to_be_bytes()));
    })));
    children.push(Atom::leaf(b"co64", full_box(0, |v| {
        v.extend_from_slice(&(n as u32).to_be_bytes());
        v.extend(std::iter::repeat_n(0u8, n * 8));
    })));
    stbl.children = Some(children);
}

/// 切り落とす。start_sec より前で、いちばん近いキーフレームから始める。
/// 切り落とした秒数（映像のトラックで。0 なら切らずにそのまま写した）を返す
pub fn trim(input: &Path, output: &Path, start_sec: f64) -> Result<f64> {
    let mut f = File::open(input).with_context(|| format!("開けませんでした: {}", input.display()))?;
    let len = f.metadata()?.len();

    // 一番上の箱の位置（mdat の中身は読まない）
    let mut top: Vec<([u8; 4], u64, u64, u64)> = Vec::new(); // (種類, 始まり, ヘッダーの長さ, 全体の長さ)
    let mut pos = 0u64;
    while pos + 8 <= len {
        f.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 16];
        let got = f.read(&mut h)?;
        let mut size = u32::from_be_bytes(h[0..4].try_into()?) as u64;
        let kind: [u8; 4] = h[4..8].try_into()?;
        let mut header = 8;
        if size == 1 && got >= 16 {
            size = u64::from_be_bytes(h[8..16].try_into()?);
            header = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < header {
            bail!("ファイルの形がおかしいです");
        }
        top.push((kind, pos, header, size));
        pos += size;
    }
    let read_box = |f: &mut File, kind: &[u8; 4]| -> Result<Vec<u8>> {
        let &(_, start, header, size) = top.iter().find(|b| &b.0 == kind).ok_or_else(|| anyhow!("{} がありません", String::from_utf8_lossy(kind)))?;
        let mut buf = vec![0u8; (size - header) as usize];
        f.seek(SeekFrom::Start(start + header))?;
        f.read_exact(&mut buf)?;
        Ok(buf)
    };
    let ftyp = read_box(&mut f, b"ftyp")?;
    let moov_body = read_box(&mut f, b"moov")?;
    let mut moov = Atom { kind: *b"moov", data: Vec::new(), children: Some(parse_atoms(&moov_body)?) };

    let (movie_ts, _, _) = time_fields(&moov.child(b"mvhd").ok_or_else(|| anyhow!("mvhd がありません"))?.data)?;
    let tracks: Vec<Track> = moov.children.as_ref().unwrap().iter().filter(|a| a.is(b"trak")).map(read_track).collect::<Result<_>>()?;
    let video = tracks.iter().position(|t| &t.handler == b"vide").ok_or_else(|| anyhow!("映像のトラックがありません"))?;

    // 映像: start_sec 以前で、いちばん近いキーフレーム
    let v = &tracks[video];
    let vt = v.decode_times();
    let target = (start_sec * v.timescale as f64) as u64;
    let first_v = (0..vt.len())
        .filter(|&i| vt[i] <= target && v.sync.as_ref().is_none_or(|s| s[i]))
        .next_back()
        .unwrap_or(0);
    let cut_sec = vt[first_v] as f64 / v.timescale as f64;
    if first_v == 0 || cut_sec <= 0.0 {
        std::fs::copy(input, output)?;
        return Ok(0.0);
    }
    // ほかのトラック: 切った時刻から
    let firsts: Vec<usize> = tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            if i == video {
                return first_v;
            }
            let cut = (cut_sec * t.timescale as f64).round() as u64;
            t.decode_times().iter().position(|&d| d >= cut).unwrap_or(t.sizes.len())
        })
        .collect();

    // 索引を作り直す
    let mut max_movie_duration = 0u64;
    let mut ti = 0usize;
    for trak in moov.children.as_mut().unwrap().iter_mut().filter(|a| a.is(b"trak")) {
        let t = &tracks[ti];
        let first = firsts[ti];
        let media_duration: u64 = t.durations[first..].iter().map(|&d| d as u64).sum();
        let movie_duration = media_duration * movie_ts as u64 / t.timescale.max(1) as u64;
        max_movie_duration = max_movie_duration.max(movie_duration);
        if let Some(tkhd) = trak.child_mut(b"tkhd") {
            set_tkhd_duration(&mut tkhd.data, movie_duration);
        }
        if let Some(edts) = trak.child_mut(b"edts") {
            // 区間は 1 つ（元の media_time のまま。切る前の頭の空白は、もう要らない）
            let mt = t.media_time.unwrap_or(0);
            edts.children = Some(vec![Atom::leaf(b"elst", full_box(1, |v| {
                v.extend_from_slice(&1u32.to_be_bytes());
                v.extend_from_slice(&movie_duration.to_be_bytes());
                v.extend_from_slice(&mt.to_be_bytes());
                v.extend_from_slice(&1u16.to_be_bytes());
                v.extend_from_slice(&0u16.to_be_bytes());
            }))]);
        }
        let mdia = trak.child_mut(b"mdia").unwrap();
        set_duration(&mut mdia.child_mut(b"mdhd").unwrap().data, media_duration);
        let stbl = mdia.child_mut(b"minf").and_then(|m| m.child_mut(b"stbl")).unwrap();
        rebuild_stbl(stbl, t, first);
        ti += 1;
    }
    set_duration(&mut moov.child_mut(b"mvhd").unwrap().data, max_movie_duration);

    // 位置を決める: ftyp, moov, mdat（16 バイトのヘッダー）の順。トラックごとに続けて置く
    let ftyp_atom = Atom::leaf(b"ftyp", ftyp);
    let mut data_at = ftyp_atom.size() + moov.size() + 16;
    let mut ti = 0usize;
    let mut plan: Vec<(u64, u32)> = Vec::new(); // (元の位置, 大きさ) を書き出す順に
    for trak in moov.children.as_mut().unwrap().iter_mut().filter(|a| a.is(b"trak")) {
        let t = &tracks[ti];
        let first = firsts[ti];
        let stbl = trak.child_mut(b"mdia").and_then(|m| m.child_mut(b"minf")).and_then(|m| m.child_mut(b"stbl")).unwrap();
        let co64 = stbl.child_mut(b"co64").unwrap();
        for (k, i) in (first..t.sizes.len()).enumerate() {
            co64.data[8 + k * 8..16 + k * 8].copy_from_slice(&data_at.to_be_bytes());
            plan.push((t.offsets[i], t.sizes[i]));
            data_at += t.sizes[i] as u64;
        }
        ti += 1;
    }
    let mdat_body: u64 = plan.iter().map(|p| p.1 as u64).sum();

    let mut out = BufWriter::with_capacity(1 << 20, File::create(output).with_context(|| format!("書けませんでした: {}", output.display()))?);
    let mut head = Vec::new();
    ftyp_atom.write(&mut head);
    moov.write(&mut head);
    head.extend_from_slice(&1u32.to_be_bytes());
    head.extend_from_slice(b"mdat");
    head.extend_from_slice(&(mdat_body + 16).to_be_bytes());
    out.write_all(&head)?;
    let mut buf = Vec::new();
    for (at, size) in plan {
        buf.resize(size as usize, 0);
        f.seek(SeekFrom::Start(at))?;
        f.read_exact(&mut buf)?;
        out.write_all(&buf)?;
    }
    out.flush()?;
    Ok(cut_sec)
}
