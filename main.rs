mod effect;
mod video;

use eframe::egui::{self, Color32, Stroke};
use effect::Params;
use image::RgbImage;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Preview frames are downscaled to this width so sliders stay responsive.
const PREVIEW_WIDTH: u32 = 1280;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Miniature")
            .with_inner_size([1200.0, 760.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native("Miniature", options, Box::new(|_cc| Ok(Box::new(App::new()))))
}

struct ExportJob {
    output: PathBuf,
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<anyhow::Result<()>>>,
}

struct App {
    tools: Result<video::Tools, String>,
    input: Option<PathBuf>,
    duration: f64,
    preview_time: f64,
    source: Option<RgbImage>,
    texture: Option<egui::TextureHandle>,
    params: Params,
    rendered_params: Option<Params>,
    show_guides: bool,
    export: Option<ExportJob>,
    status: String,
}

impl App {
    fn new() -> Self {
        Self {
            tools: video::Tools::locate().map_err(|e| e.to_string()),
            input: None,
            duration: 0.0,
            preview_time: 0.0,
            source: None,
            texture: None,
            params: Params::default(),
            rendered_params: None,
            show_guides: true,
            export: None,
            status: String::new(),
        }
    }

    fn open_file(&mut self) {
        let Ok(tools) = &self.tools else { return };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Video", &["mp4", "mov", "m4v", "mkv", "avi"])
            .pick_file()
        else {
            return;
        };
        match video::duration(tools, &path) {
            Ok(d) => {
                self.duration = d;
                self.preview_time = d * 0.1; // first frame is often black
                self.input = Some(path);
                self.load_preview_frame();
            }
            Err(e) => self.status = format!("Couldn't open video: {e:#}"),
        }
    }

    fn load_preview_frame(&mut self) {
        let at = self.preview_time.min((self.duration - 0.2).max(0.0));
        let result = match (&self.tools, &self.input) {
            (Ok(t), Some(i)) => video::grab_frame(t, i, at, Some(PREVIEW_WIDTH)),
            _ => return,
        };
        match result {
            Ok(img) => {
                self.source = Some(img);
                self.rendered_params = None; // force re-render
                self.status.clear();
            }
            Err(e) => self.status = format!("{e:#}"),
        }
    }

    fn refresh_preview(&mut self, ctx: &egui::Context) {
        let Some(src) = &self.source else { return };
        if self.rendered_params == Some(self.params) {
            return;
        }
        let mut img = src.clone();
        let (w, h) = img.dimensions();
        effect::apply(&mut img, w as usize, h as usize, &self.params);
        let color = egui::ColorImage::from_rgb([w as usize, h as usize], &img);
        self.texture = Some(ctx.load_texture("preview", color, egui::TextureOptions::LINEAR));
        self.rendered_params = Some(self.params);
    }

    fn start_export(&mut self) {
        let (Ok(tools), Some(input)) = (&self.tools, &self.input) else { return };
        let stem = input
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "video".into());
        let Some(output) = rfd::FileDialog::new()
            .set_file_name(format!("{stem}_miniature.mp4"))
            .add_filter("MP4 video", &["mp4"])
            .save_file()
        else {
            return;
        };

        let progress = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tools, input, params, duration) = (tools.clone(), input.clone(), self.params, self.duration);
        let (p, c, out) = (progress.clone(), cancel.clone(), output.clone());
        let handle = std::thread::spawn(move || {
            video::export(&tools, &input, &out, &params, duration, &p, &c)
        });

        self.export = Some(ExportJob { output, progress, cancel, handle: Some(handle) });
        self.status.clear();
    }

    fn poll_export(&mut self, ctx: &egui::Context) {
        let Some(job) = &mut self.export else { return };
        if !job.handle.as_ref().is_some_and(|h| h.is_finished()) {
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        let result = job.handle.take().unwrap().join();
        self.status = if job.cancel.load(Ordering::Relaxed) {
            "Export cancelled.".into()
        } else {
            match result {
                Ok(Ok(())) => format!("Saved {}", job.output.display()),
                Ok(Err(e)) => format!("Export failed: {e:#}"),
                Err(_) => "Export failed: worker thread panicked".into(),
            }
        };
        self.export = None;
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.heading("Miniature");
        ui.add_space(6.0);

        if let Err(e) = &self.tools {
            ui.colored_label(Color32::LIGHT_RED, e);
            ui.label("Install it with Homebrew, then restart the app:");
            ui.code("brew install ffmpeg");
            return;
        }

        let busy = self.export.is_some();
        ui.add_enabled_ui(!busy, |ui| {
            if ui.button("Open video…").clicked() {
                self.open_file();
            }
            if let Some(name) = self.input.as_ref().and_then(|p| p.file_name()) {
                ui.label(name.to_string_lossy());
            }

            if self.input.is_some() {
                let r = ui.add(
                    egui::Slider::new(&mut self.preview_time, 0.0..=self.duration)
                        .text("preview at (s)"),
                );
                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                    self.load_preview_frame();
                }
            }

            let p = &mut self.params;
            section(ui, "Focus");
            ui.add(egui::Slider::new(&mut p.focus_center, 0.0..=1.0).text("position"));
            ui.add(egui::Slider::new(&mut p.focus_size, 0.0..=0.6).text("sharp band"));
            ui.add(egui::Slider::new(&mut p.falloff, 0.02..=0.6).text("falloff"));
            ui.add(egui::Slider::new(&mut p.blur, 0.0..=0.04).text("blur"));
            ui.checkbox(&mut self.show_guides, "Show guides (click preview to move)");

            let p = &mut self.params;
            section(ui, "Color");
            ui.add(egui::Slider::new(&mut p.saturation, 0.5..=2.5).text("saturation"));
            ui.add(egui::Slider::new(&mut p.contrast, 0.8..=1.6).text("contrast"));

            section(ui, "Motion (export only)");
            ui.add(egui::Slider::new(&mut p.speed, 1.0..=12.0).text("speed ×"));
            ui.add(egui::Slider::new(&mut p.fps, 8..=30).text("output fps"));

            ui.add_space(10.0);
            if ui.button("Reset settings").clicked() {
                self.params = Params::default();
            }

            ui.add_space(16.0);
            let can_export = self.input.is_some();
            if ui
                .add_enabled(can_export, egui::Button::new("Export MP4…").min_size([140.0, 28.0].into()))
                .clicked()
            {
                self.start_export();
            }
        });
    }

    fn preview(&mut self, ui: &mut egui::Ui) {
        let Some(tex) = &self.texture else {
            ui.centered_and_justified(|ui| {
                ui.label(
                    "Open a video to begin.\n\nThe effect works best on footage shot from \
                     high up, looking down: rooftops, drones, hillsides.",
                );
            });
            return;
        };

        let avail = ui.available_size();
        let resp = ui
            .vertical_centered(|ui| {
                ui.add(
                    egui::Image::new(egui::load::SizedTexture::from_handle(tex))
                        .max_size(avail)
                        .sense(egui::Sense::click()),
                )
            })
            .inner;

        let rect = resp.rect;
        if resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                self.params.focus_center = ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
            }
        }

        if self.show_guides {
            let p = &self.params;
            let painter = ui.painter_at(rect);
            let y = |f: f32| rect.top() + f.clamp(0.0, 1.0) * rect.height();
            let sharp = Stroke::new(1.5, Color32::from_rgba_unmultiplied(255, 210, 0, 210));
            let soft = Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 110));
            let top = p.focus_center - p.focus_size / 2.0;
            let bottom = p.focus_center + p.focus_size / 2.0;
            painter.hline(rect.x_range(), y(top), sharp);
            painter.hline(rect.x_range(), y(bottom), sharp);
            painter.hline(rect.x_range(), y(top - p.falloff), soft);
            painter.hline(rect.x_range(), y(bottom + p.falloff), soft);
        }
    }
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(10.0);
    ui.separator();
    ui.strong(title);
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_export(ctx);
        self.refresh_preview(ctx);

        egui::SidePanel::left("controls")
            .resizable(false)
            .exact_width(300.0)
            .show(ctx, |ui| self.controls(ui));

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(4.0);
            if let Some(job) = &self.export {
                ui.horizontal(|ui| {
                    let frac = job.progress.load(Ordering::Relaxed) as f32 / 1000.0;
                    ui.add(egui::ProgressBar::new(frac).show_percentage().desired_width(320.0));
                    if ui.button("Cancel").clicked() {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                });
            } else {
                ui.label(&self.status);
            }
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| self.preview(ui));
    }
}
