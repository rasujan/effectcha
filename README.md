# Miniature

A small macOS app (Rust + egui) that turns videos into tilt-shift "miniature" footage:
a sharp focus band, blurred top and bottom, punchy colors, and sped-up, slightly
choppy motion.

## Requirements

- Rust (https://rustup.rs)
- ffmpeg: `brew install ffmpeg`

## Run

    cargo run --release

## Build a .app

    cargo install cargo-bundle
    cargo bundle --release
    # -> target/release/bundle/osx/Miniature.app

The app looks for ffmpeg in /opt/homebrew/bin and /usr/local/bin, so the bundled
app works when launched from Finder.

## Using it

1. Open a video. Footage shot from high up, looking down, works best.
2. Click the preview to move the focus band, then tune it with the sliders.
   Yellow lines mark the sharp band, white lines where the blur reaches full strength.
3. Set speed and output fps (these only affect the export), then Export MP4.

Audio is dropped, since sped-up audio rarely suits the effect.

## Code layout

- `src/effect.rs`: the effect itself (color grade, separable Gaussian blur, focus blend), parallelised with rayon
- `src/video.rs`: ffmpeg decode → Rust processing → ffmpeg H.264 encode, over raw RGB pipes
- `src/main.rs`: the egui interface

Tests: `cargo test`. To also run an end-to-end export:
`MINIATURE_TEST_VIDEO=/path/to/clip.mp4 cargo test --release`
