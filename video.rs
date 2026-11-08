//! Video I/O via ffmpeg. Frames are decoded to raw RGB, processed in Rust,
//! and piped back into a second ffmpeg process for H.264 encoding.

use crate::effect::{self, Params};
use anyhow::{bail, Context, Result};
use image::RgbImage;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;

/// yuv420p needs even dimensions.
const EVEN: &str = "scale=trunc(iw/2)*2:trunc(ih/2)*2";

#[derive(Clone)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Tools {
    pub fn locate() -> Result<Self> {
        Ok(Self {
            ffmpeg: find_tool("ffmpeg").context("ffmpeg not found")?,
            ffprobe: find_tool("ffprobe").context("ffprobe not found")?,
        })
    }
}

/// Apps launched from Finder don't inherit the shell PATH, so check the
/// Homebrew locations explicitly before falling back to PATH.
fn find_tool(name: &str) -> Option<PathBuf> {
    ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"]
        .iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| p.exists())
        .or_else(|| {
            let paths = std::env::var_os("PATH")?;
            std::env::split_paths(&paths).map(|d| d.join(name)).find(|p| p.exists())
        })
}

pub fn duration(tools: &Tools, input: &Path) -> Result<f64> {
    let out = Command::new(&tools.ffprobe)
        .args(["-v", "error", "-show_entries", "format=duration"])
        .args(["-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(input)
        .output()
        .context("running ffprobe")?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .context("couldn't read the video's duration")
}

/// Grab one frame. With `max_width`, it's downscaled for previewing.
pub fn grab_frame(tools: &Tools, input: &Path, at: f64, max_width: Option<u32>) -> Result<RgbImage> {
    let vf = match max_width {
        Some(mw) => format!("scale='min(iw,{mw})':-2"),
        None => EVEN.to_string(),
    };
    let out = Command::new(&tools.ffmpeg)
        .args(["-v", "error", "-ss", &format!("{at:.3}"), "-i"])
        .arg(input)
        .args(["-frames:v", "1", "-vf", &vf, "-f", "image2pipe", "-vcodec", "png", "-"])
        .stdin(Stdio::null())
        .output()
        .context("running ffmpeg")?;
    if !out.status.success() || out.stdout.is_empty() {
        bail!("couldn't read a frame: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(image::load_from_memory(&out.stdout)?.to_rgb8())
}

pub fn export(
    tools: &Tools,
    input: &Path,
    output: &Path,
    p: &Params,
    duration: f64,
    progress: &AtomicU32,
    cancel: &AtomicBool,
) -> Result<()> {
    // Decoding a frame through the same scale filter gives the true output
    // size, including any rotation ffmpeg applies (e.g. portrait iPhone clips).
    let (w, h) = grab_frame(tools, input, 0.0, None)?.dimensions();
    let expected = (duration / p.speed as f64 * p.fps as f64).max(1.0);

    let vf = format!("{EVEN},setpts=PTS/{},fps={}", p.speed, p.fps);
    let mut dec = Command::new(&tools.ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(input)
        .args(["-an", "-vf", &vf, "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting ffmpeg decoder")?;

    let mut enc = Command::new(&tools.ffmpeg)
        .args(["-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgb24"])
        .args(["-s", &format!("{w}x{h}"), "-r", &p.fps.to_string(), "-i", "-"])
        .args(["-c:v", "libx264", "-preset", "medium", "-crf", "18"])
        .args(["-pix_fmt", "yuv420p", "-movflags", "+faststart"])
        .arg(output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting ffmpeg encoder")?;

    let dec_log = drain(dec.stderr.take());
    let enc_log = drain(enc.stderr.take());
    let mut reader = dec.stdout.take().context("no decoder output")?;
    let mut writer = enc.stdin.take().context("no encoder input")?;

    let pumped = pump(&mut reader, &mut writer, w as usize, h as usize, p, expected, progress, cancel);

    drop(writer); // signals end-of-stream to the encoder
    drop(reader);
    if pumped.is_err() {
        let _ = dec.kill();
        let _ = enc.kill();
    }
    let dec_status = dec.wait()?;
    let enc_status = enc.wait()?;
    let dec_log = dec_log.join().unwrap_or_default();
    let enc_log = enc_log.join().unwrap_or_default();

    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(output);
        bail!("cancelled");
    }
    let frames = match pumped {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_file(output);
            return Err(e.context(enc_log.trim().to_string()));
        }
    };
    if !dec_status.success() {
        bail!("decoding failed: {}", dec_log.trim());
    }
    if !enc_status.success() {
        bail!("encoding failed: {}", enc_log.trim());
    }
    if frames == 0 {
        bail!("no frames were decoded");
    }
    progress.store(1000, Ordering::Relaxed);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn pump(
    reader: &mut impl Read,
    writer: &mut impl Write,
    w: usize,
    h: usize,
    p: &Params,
    expected: f64,
    progress: &AtomicU32,
    cancel: &AtomicBool,
) -> Result<u64> {
    let mut buf = vec![0u8; w * h * 3];
    let mut frames = 0u64;
    while read_frame(reader, &mut buf)? {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        effect::apply(&mut buf, w, h, p);
        writer.write_all(&buf).context("writing to encoder")?;
        frames += 1;
        let frac = (frames as f64 / expected).min(0.99);
        progress.store((frac * 1000.0) as u32, Ordering::Relaxed);
    }
    Ok(frames)
}

/// Fill `buf` with one frame. Returns false at end of stream.
fn read_frame(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = r.read(&mut buf[filled..])?;
        if n == 0 {
            return Ok(false);
        }
        filled += n;
    }
    Ok(true)
}

/// Read stderr on a thread so a chatty ffmpeg can never block on a full pipe.
fn drain(stderr: Option<ChildStderr>) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut e) = stderr {
            let _ = e.read_to_string(&mut s);
        }
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end export. Set MINIATURE_TEST_VIDEO to a video file to run it.
    #[test]
    fn export_roundtrip() {
        let Ok(input) = std::env::var("MINIATURE_TEST_VIDEO") else { return };
        let tools = Tools::locate().expect("ffmpeg installed");
        let input = PathBuf::from(input);
        let output = std::env::temp_dir().join("miniature_test_out.mp4");
        let dur = duration(&tools, &input).unwrap();
        let progress = AtomicU32::new(0);
        let cancel = AtomicBool::new(false);
        export(&tools, &input, &output, &Params::default(), dur, &progress, &cancel).unwrap();
        assert_eq!(progress.load(Ordering::Relaxed), 1000);
        let out_dur = duration(&tools, &output).unwrap();
        let want = dur / Params::default().speed as f64;
        assert!((out_dur - want).abs() < 0.3, "got {out_dur}s, want ~{want}s");
    }
}
