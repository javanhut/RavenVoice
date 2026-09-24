//! Whisper model files: where they live and how to fetch them.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Models published in ggml format by the whisper.cpp project.
pub const KNOWN_MODELS: &[(&str, &str)] = &[
    ("tiny.en", "75 MB, fastest, rough"),
    ("base.en", "142 MB, fast, good for dictation"),
    ("small.en", "466 MB, noticeably more accurate"),
    ("medium.en", "1.5 GB, very accurate, slow on CPU"),
    ("large-v3-turbo", "1.6 GB, multilingual, best quality"),
    ("tiny", "75 MB, multilingual"),
    ("base", "142 MB, multilingual"),
    ("small", "466 MB, multilingual"),
];

pub fn models_dir() -> PathBuf {
    crate::config::data_dir().join("models")
}

pub fn path_for(name: &str) -> PathBuf {
    models_dir().join(format!("ggml-{name}.bin"))
}

fn url_for(name: &str) -> String {
    format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{name}.bin")
}

/// Download `name` to `dest`, calling `progress(fraction)` as bytes arrive.
///
/// Writes to a `.part` file first, so an interrupted download never leaves a
/// truncated model that would later fail to load.
pub fn download(name: &str, dest: &Path, mut progress: impl FnMut(f32)) -> Result<()> {
    if name.contains('/') || name.contains("..") {
        bail!("invalid model name {name:?}");
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let url = url_for(name);
    log::info!("downloading {url}");
    let response = ureq::get(&url)
        .call()
        .with_context(|| format!("fetching {url}"))?;
    let total: u64 = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let part = dest.with_extension("bin.part");
    let mut file = std::fs::File::create(&part)?;
    let mut reader = response.into_body().into_reader();
    let mut buf = vec![0u8; 1 << 16];
    let mut done: u64 = 0;
    let mut last_report = 0.0f32;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        done += n as u64;
        if total > 0 {
            let f = done as f32 / total as f32;
            if f - last_report >= 0.01 {
                last_report = f;
                progress(f);
            }
        }
    }
    file.sync_all()?;
    drop(file);
    if total > 0 && done != total {
        bail!("download of {name} was cut short ({done} of {total} bytes)");
    }
    if done < 1_000_000 {
        bail!("{url} returned only {done} bytes; is `{name}` a real model name?");
    }
    std::fs::rename(&part, dest)?;
    progress(1.0);
    Ok(())
}
