mod capture;

use capture::{
    FrameCapturer, OverlayConfig, PreviewConfig, PreviewCropEdge, PreviewEdge, PreviewOptions,
    Rect, parse_color,
};
use image::{
    ImageBuffer, ImageEncoder, Rgba,
    codecs::png::{CompressionType, FilterType, PngEncoder},
    imageops::{self, FilterType as ResizeFilterType},
};
use std::env;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const APP_NAME: &str = "wl-longshot";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_FPS: u32 = 15;
const CLIPBOARD_MAX_PIXELS: u64 = 24_000_000;
const CLIPBOARD_MAX_HEIGHT: u32 = 16_000;

type Image = ImageBuffer<Rgba<u8>, Vec<u8>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Backend {
    Frame,
    Grim,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GrimMode {
    Auto,
    Manual,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlMode {
    Stdin,
    Menu,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GrimAction {
    Next,
    Finish,
    Abort,
}

#[derive(Clone, Copy)]
struct UiText {
    capture_next: &'static str,
    finish: &'static str,
    abort: &'static str,
    stop: &'static str,
    recording: &'static str,
    grim_mode: &'static str,
}

#[derive(Debug)]
struct Config {
    output: Option<String>,
    geometry: Option<String>,
    backend: Backend,
    fps: u32,
    copy: bool,
    open: bool,
    edit: bool,
    debug_dir: Option<PathBuf>,
    debug_timing: bool,
    show_border: bool,
    border_color: Option<[u8; 3]>,
    preview: bool,
    preview_width: u32,
    stream: Option<PathBuf>,
    stream_keep_frames: bool,
    stream_every: usize,
    grim_mode: GrimMode,
    grim_fixed_width: bool,
    grim_dedup: bool,
    control: ControlMode,
    menu_cmd: Option<String>,
}

#[derive(Debug, Default)]
struct TimingStats {
    capture: Duration,
    stitch: Duration,
    write_png: Duration,
    post_process: Duration,
}

#[derive(Debug)]
struct Stitcher {
    full: Option<Image>,
    full_cols: Vec<[f32; 3]>,
    last_cols: Vec<[f32; 3]>,
    last_signature: Option<Vec<u8>>,
    anchor_pos: i32,
    last_offset: i32,
    pending_edge: Option<Edge>,
    growth_edge: Option<Edge>,
    frames: usize,
    accepted: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Edge {
    Start,
    End,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StitchStatus {
    FirstFrame,
    Appended,
    NoProgress,
    NoMatch,
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
struct StitchResult {
    status: StitchStatus,
    added: u32,
    edge: Option<Edge>,
    position: i32,
    frame_len: u32,
}

impl StitchResult {
    fn accepted(self) -> bool {
        matches!(
            self.status,
            StitchStatus::FirstFrame | StitchStatus::Appended
        )
    }
}

#[derive(Debug)]
struct MatchResult {
    pos: i32,
    diff: f32,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let config = parse_args(env::args().skip(1).collect())?;
    let output_path = match &config.output {
        Some(output) if output == "-" => OutputTarget::Stdout,
        Some(output) => OutputTarget::File(PathBuf::from(output)),
        None => OutputTarget::File(default_output_path()?),
    };

    match config.backend {
        Backend::Frame => run_frame_backend(&config, &output_path),
        Backend::Grim => run_grim_backend(&config, &output_path),
    }
}

enum OutputTarget {
    File(PathBuf),
    Stdout,
}

#[derive(Debug)]
struct PreviewView {
    zoomed: bool,
    scrub_pos: u32,
    following: bool,
    frame_len: u32,
    edge: Option<Edge>,
    capture_pos: u32,
    capture_len: u32,
    crop_enabled: bool,
    crop_top: u32,
    crop_bottom: u32,
    crop_image_height: u32,
}

impl Default for PreviewView {
    fn default() -> Self {
        Self {
            zoomed: false,
            scrub_pos: 0,
            following: true,
            frame_len: 0,
            edge: None,
            capture_pos: 0,
            capture_len: 0,
            crop_enabled: true,
            crop_top: 0,
            crop_bottom: 0,
            crop_image_height: 0,
        }
    }
}

struct StreamWriter {
    dir: PathBuf,
    keep_frames: bool,
    every: usize,
    index: usize,
    accepted_seen: usize,
}

impl StreamWriter {
    fn new(dir: PathBuf, keep_frames: bool, every: usize) -> Result<Self, String> {
        let frames_dir = dir.join("frames");
        fs::create_dir_all(&dir)
            .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
        if keep_frames {
            fs::create_dir_all(&frames_dir)
                .map_err(|error| format!("failed to create {}: {error}", frames_dir.display()))?;
        }
        Ok(Self {
            dir,
            keep_frames,
            every,
            index: 0,
            accepted_seen: 0,
        })
    }

    fn write_update(
        &mut self,
        image: &Image,
        frames: usize,
        accepted: usize,
    ) -> Result<(), String> {
        self.accepted_seen += 1;
        if self.accepted_seen % self.every != 0 {
            return Ok(());
        }
        self.index += 1;
        let frame_rel = format!("frames/{:06}.png", self.index);
        if self.keep_frames {
            let frame_path = self.dir.join(&frame_rel);
            write_png_atomic(image, &frame_path)?;
        }
        write_png_atomic(image, &self.dir.join("latest.png"))?;
        let manifest = format!(
            "{{\n  \"version\": 1,\n  \"index\": {},\n  \"latest\": \"latest.png\",\n  \"frame\": {},\n  \"width\": {},\n  \"height\": {},\n  \"frames\": {},\n  \"accepted\": {},\n  \"updated_at_ms\": {}\n}}\n",
            self.index,
            if !self.keep_frames {
                "null".to_string()
            } else {
                format!("\"{frame_rel}\"")
            },
            image.width(),
            image.height(),
            frames,
            accepted,
            now_millis(),
        );
        write_text_atomic(&manifest, &self.dir.join("manifest.json"))
    }
}

fn parse_args(args: Vec<String>) -> Result<Config, String> {
    let mut config = Config {
        output: None,
        geometry: None,
        backend: Backend::Frame,
        fps: DEFAULT_FPS,
        copy: true,
        open: false,
        edit: false,
        debug_dir: None,
        debug_timing: false,
        show_border: true,
        border_color: None,
        preview: true,
        preview_width: 320,
        stream: None,
        stream_keep_frames: false,
        stream_every: 1,
        grim_mode: GrimMode::Auto,
        grim_fixed_width: true,
        grim_dedup: true,
        control: ControlMode::Stdin,
        menu_cmd: None,
    };

    let mut positional = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-v" | "--version" => {
                println!("{APP_NAME} {VERSION}");
                std::process::exit(0);
            }
            "-o" | "--output" => {
                index += 1;
                config.output = Some(take_arg(&args, index, "--output")?);
            }
            "-g" | "--geometry" => {
                index += 1;
                config.geometry = Some(take_arg(&args, index, "--geometry")?);
            }
            "-b" | "--backend" => {
                index += 1;
                config.backend = parse_backend(&take_arg(&args, index, "--backend")?)?;
            }
            "-f" | "--fps" => {
                index += 1;
                let value = take_arg(&args, index, "--fps")?;
                config.fps = value
                    .parse::<u32>()
                    .map_err(|_| "fps must be an integer".to_string())?;
                if !(1..=120).contains(&config.fps) {
                    return Err("fps must be in 1..120".to_string());
                }
            }
            "-c" | "--copy" => config.copy = true,
            "--no-copy" => config.copy = false,
            "--open" => config.open = true,
            "--edit" => config.edit = true,
            "--debug-dir" => {
                index += 1;
                config.debug_dir = Some(PathBuf::from(take_arg(&args, index, "--debug-dir")?));
            }
            "--debug-timing" => config.debug_timing = true,
            "--no-boarder" | "--no-border" => config.show_border = false,
            "--boarder-color" | "--border-color" => {
                index += 1;
                let value = take_arg(&args, index, "--boarder-color")?;
                config.border_color = Some(
                    parse_color(&value).ok_or_else(|| format!("invalid border color: {value}"))?,
                );
            }
            "--preview" => config.preview = true,
            "--no-preview" => config.preview = false,
            "--preview-width" => {
                index += 1;
                let value = take_arg(&args, index, "--preview-width")?;
                config.preview_width = value
                    .parse::<u32>()
                    .map_err(|_| "preview width must be an integer".to_string())?;
                if !(80..=1200).contains(&config.preview_width) {
                    return Err("preview width must be in 80..1200".to_string());
                }
            }
            "--stream" => {
                index += 1;
                config.stream = Some(PathBuf::from(take_arg(&args, index, "--stream")?));
            }
            "--stream-keep-frames" => config.stream_keep_frames = true,
            "--stream-every" => {
                index += 1;
                let value = take_arg(&args, index, "--stream-every")?;
                config.stream_every = value
                    .parse::<usize>()
                    .map_err(|_| "stream every must be an integer".to_string())?;
                if config.stream_every == 0 {
                    return Err("stream every must be greater than 0".to_string());
                }
            }
            "--grim-mode" => {
                index += 1;
                config.grim_mode = parse_grim_mode(&take_arg(&args, index, "--grim-mode")?)?;
            }
            "--grim-fixed-width" => config.grim_fixed_width = true,
            "--no-grim-fixed-width" => config.grim_fixed_width = false,
            "--grim-dedup" => config.grim_dedup = true,
            "--no-grim-dedup" => config.grim_dedup = false,
            "--control" => {
                index += 1;
                config.control = parse_control_mode(&take_arg(&args, index, "--control")?)?;
            }
            "--menu-cmd" => {
                index += 1;
                config.menu_cmd = Some(take_arg(&args, index, "--menu-cmd")?);
            }
            "--list-backends" => {
                println!("frame");
                println!("grim");
                std::process::exit(0);
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            value => positional.push(value.to_string()),
        }
        index += 1;
    }

    if positional.len() > 1 {
        return Err("too many output files".to_string());
    }
    if config.output.is_none() {
        config.output = positional.pop();
    }
    if config.copy && config.output.as_deref() == Some("-") {
        return Err("--copy cannot be used with stdout output".to_string());
    }
    if config.open && config.output.as_deref() == Some("-") {
        return Err("--open cannot be used with stdout output".to_string());
    }
    if config.edit && config.output.as_deref() == Some("-") {
        return Err("--edit cannot be used with stdout output".to_string());
    }
    Ok(config)
}

fn take_arg(args: &[String], index: usize, option: &str) -> Result<String, String> {
    args.get(index)
        .cloned()
        .ok_or_else(|| format!("missing argument for {option}"))
}

fn parse_backend(value: &str) -> Result<Backend, String> {
    match value {
        "frame" | "frame-helper" => Ok(Backend::Frame),
        "grim" => Ok(Backend::Grim),
        _ => Err(format!("invalid backend: {value}")),
    }
}

fn parse_grim_mode(value: &str) -> Result<GrimMode, String> {
    match value {
        "auto" => Ok(GrimMode::Auto),
        "manual" => Ok(GrimMode::Manual),
        _ => Err(format!("invalid grim mode: {value}")),
    }
}

fn parse_control_mode(value: &str) -> Result<ControlMode, String> {
    match value {
        "stdin" => Ok(ControlMode::Stdin),
        "menu" => Ok(ControlMode::Menu),
        _ => Err(format!("invalid control mode: {value}")),
    }
}

fn print_help() {
    println!("Usage: wl-longshot [options] [output-file]");
    println!();
    println!("A scrolling screenshot tool for Wayland compositors.");
    println!();
    println!("Options:");
    println!("  -h, --help              Show help message and quit.");
    println!("  -v, --version           Show version and quit.");
    println!("  -g, --geometry <geo>    Capture region, e.g. \"10,20 800x1200\".");
    println!(
        "  -o, --output <file>     Write PNG to file; use '-' for stdout. Defaults to $XDG_PICTURES_DIR/Screenshots/longshots."
    );
    println!("  -b, --backend <name>    Backend: frame, grim. Defaults to frame.");
    println!("      --list-backends     Print available backends and quit.");
    println!("  -f, --fps <n>           Frame capture rate. Defaults to 15.");
    println!("      --no-boarder        Hide the capture border overlay.");
    println!("      --boarder-color <c> Set border color, e.g. '#d2c973'.");
    println!(
        "      --preview           Show a live layer-shell preview beside the capture region. Enabled by default."
    );
    println!("      --no-preview       Disable the live layer-shell preview.");
    println!("      --preview-width <n> Preview content width in pixels. Defaults to 320.");
    println!("                         Left-click preview to zoom; use mouse wheel to scroll.");
    println!("      --stream <dir>      Write accepted intermediate PNGs to a stream directory.");
    println!("      --stream-keep-frames  Also keep numbered PNG snapshots under frames/.");
    println!("      --stream-every <n>  Stream every N accepted updates. Defaults to 1.");
    println!("      --grim-mode <mode>  Grim mode: auto, manual. Defaults to auto.");
    println!("      --no-grim-fixed-width  Allow manual grim captures to change width.");
    println!("      --no-grim-dedup     Append manual grim captures without overlap dedup.");
    println!("      --control <mode>    Control mode: stdin, menu. Defaults to stdin.");
    println!("      --menu-cmd <cmd>    Menu command for --control menu.");
    println!(
        "  -c, --copy              Copy result to clipboard with wl-copy. Enabled by default."
    );
    println!("      --no-copy           Do not copy result to clipboard.");
    println!("      --open              Open result with xdg-open.");
    println!("      --edit              Open result with satty.");
    println!("      --debug-dir <dir>   Dump captured frames and stitch logs.");
    println!("      --debug-timing      Print timing breakdown to stderr.");
}

fn default_output_path() -> Result<PathBuf, String> {
    let pictures = env::var_os("XDG_PICTURES_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join("Pictures")))
        .ok_or_else(|| "HOME is not set".to_string())?;
    let dir = pictures.join("Screenshots").join("longshots");
    fs::create_dir_all(&dir)
        .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
    Ok(dir.join(format!("longshot_{}.png", timestamp_compact())))
}

fn timestamp_compact() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    seconds.to_string()
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn write_png_atomic(image: &Image, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file = File::create(&tmp)
            .map_err(|error| format!("failed to create {}: {error}", tmp.display()))?;
        encode_png_fast(image, &mut file)
            .map_err(|error| format!("failed to write {}: {error}", tmp.display()))?;
        file.flush()
            .map_err(|error| format!("failed to flush {}: {error}", tmp.display()))?;
    }
    fs::rename(&tmp, path).map_err(|error| {
        format!(
            "failed to rename {} to {}: {error}",
            tmp.display(),
            path.display()
        )
    })
}

fn write_text_atomic(text: &str, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text).map_err(|error| format!("failed to write {}: {error}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|error| {
        format!(
            "failed to rename {} to {}: {error}",
            tmp.display(),
            path.display()
        )
    })
}

fn run_frame_backend(config: &Config, output: &OutputTarget) -> Result<(), String> {
    let geometry = match &config.geometry {
        Some(geometry) => geometry.clone(),
        None => run_slurp()?,
    };
    if geometry.trim().is_empty() {
        return Ok(());
    }
    let rect = Rect::parse(&geometry)?;
    let mut capturer = FrameCapturer::new(
        rect,
        config.fps,
        OverlayConfig {
            enabled: config.show_border,
            color: config.border_color,
            preview: PreviewConfig {
                enabled: config.preview,
                width: config.preview_width,
            },
        },
    )?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_wake = spawn_stop_controller(stop.clone(), config)?;
    let mut timing = TimingStats::default();
    let mut preview_view = PreviewView::default();
    let mut stream = config
        .stream
        .clone()
        .map(|dir| StreamWriter::new(dir, config.stream_keep_frames, config.stream_every))
        .transpose()?;
    let image = capture_and_stitch(
        &mut capturer,
        stop,
        &stop_wake,
        config.debug_dir.as_deref(),
        config.preview,
        config.preview_width,
        &mut preview_view,
        stream.as_mut(),
        config.debug_timing.then_some(&mut timing),
    )?;
    let image = crop_image_vertical(image, preview_view.crop_top, preview_view.crop_bottom)?;
    let write_start = Instant::now();
    write_png(&image, output)?;
    timing.write_png = write_start.elapsed();
    let post_start = Instant::now();
    post_process(config, output, &image)?;
    timing.post_process = post_start.elapsed();
    if config.debug_timing {
        print_timing(&timing);
    }
    Ok(())
}

fn run_grim_backend(config: &Config, output: &OutputTarget) -> Result<(), String> {
    let geometry = match &config.geometry {
        Some(geometry) => geometry.clone(),
        None => run_slurp()?,
    };
    if geometry.trim().is_empty() {
        return Ok(());
    }
    let base_rect = Rect::parse(&geometry)?;
    let mut overlay =
        if config.preview || (config.grim_mode == GrimMode::Auto && config.show_border) {
            let overlay = FrameCapturer::new(
                base_rect,
                0,
                OverlayConfig {
                    enabled: config.grim_mode == GrimMode::Auto && config.show_border,
                    color: config.border_color,
                    preview: PreviewConfig {
                        enabled: config.preview,
                        width: config.preview_width,
                    },
                },
            )?;
            thread::sleep(Duration::from_millis(50));
            Some(overlay)
        } else {
            None
        };
    let mut stitcher = Stitcher::new();
    let mut preview_view = PreviewView::default();
    let mut stream = config
        .stream
        .clone()
        .map(|dir| StreamWriter::new(dir, config.stream_keep_frames, config.stream_every))
        .transpose()?;
    let first = grim_capture(&geometry)?;
    stitcher.push_frame(first);
    write_stream_update(stream.as_mut(), &stitcher)?;
    if config.preview {
        update_preview(
            overlay.as_mut(),
            &stitcher,
            config.preview_width,
            &mut preview_view,
        )?;
        handle_preview_events(
            overlay.as_mut(),
            &stitcher,
            config.preview_width,
            &mut preview_view,
        )?;
    }

    if config.grim_mode == GrimMode::Auto {
        let stop = Arc::new(AtomicBool::new(false));
        let _stop_wake = spawn_stop_controller(stop.clone(), config)?;
        let frame_interval = Duration::from_secs_f64(1.0 / config.fps as f64);
        while !stop.load(Ordering::SeqCst) {
            let started = Instant::now();
            let frame = grim_capture(&geometry)?;
            let outcome = stitcher.push_frame_result(frame);
            let accepted = outcome.accepted();
            if config.preview {
                sync_preview_view(&mut preview_view, &stitcher, outcome, config.preview_width);
            }
            if accepted {
                write_stream_update(stream.as_mut(), &stitcher)?;
            }
            if config.preview {
                update_preview(
                    overlay.as_mut(),
                    &stitcher,
                    config.preview_width,
                    &mut preview_view,
                )?;
                handle_preview_events(
                    overlay.as_mut(),
                    &stitcher,
                    config.preview_width,
                    &mut preview_view,
                )?;
            }
            let elapsed = started.elapsed();
            if elapsed < frame_interval {
                sleep_with_preview_events(
                    frame_interval - elapsed,
                    &stop,
                    overlay.as_mut(),
                    &stitcher,
                    config.preview,
                    config.preview_width,
                    &mut preview_view,
                )?;
            }
        }

        let image = stitcher
            .full
            .ok_or_else(|| "no frames captured from grim".to_string())?;
        let image = crop_image_vertical(image, preview_view.crop_top, preview_view.crop_bottom)?;
        write_png(&image, output)?;
        post_process(config, output, &image)?;
        return Ok(());
    }

    let action_rx = if config.control == ControlMode::Stdin {
        Some(spawn_grim_stdin_action_controller())
    } else {
        None
    };
    let menu_cmd = if config.control == ControlMode::Menu {
        Some(resolve_menu_cmd(config.menu_cmd.as_deref())?)
    } else {
        None
    };

    loop {
        let action = if let Some(rx) = &action_rx {
            wait_for_grim_action(
                rx,
                overlay.as_mut(),
                &stitcher,
                config.preview,
                config.preview_width,
                &mut preview_view,
            )?
        } else if let Some(menu_cmd) = &menu_cmd {
            wait_for_grim_menu_action(
                menu_cmd.clone(),
                overlay.as_mut(),
                &stitcher,
                config.preview,
                config.preview_width,
                &mut preview_view,
            )?
        } else {
            None
        };
        match action {
            Some(GrimAction::Next) => {}
            Some(GrimAction::Finish) | None => break,
            Some(GrimAction::Abort) => return Ok(()),
        }

        let selected = run_slurp()?;
        if selected.trim().is_empty() {
            continue;
        }
        let selected_rect = Rect::parse(&selected)?;
        let next_rect = if config.grim_fixed_width {
            Rect {
                x: base_rect.x,
                y: selected_rect.y,
                width: base_rect.width,
                height: selected_rect.height,
            }
        } else {
            selected_rect
        };
        let next_geometry = format!(
            "{},{} {}x{}",
            next_rect.x, next_rect.y, next_rect.width, next_rect.height
        );
        let frame = grim_capture(&next_geometry)?;
        if config.grim_mode == GrimMode::Manual && !config.grim_dedup {
            stitcher.append_without_dedup(frame);
            write_stream_update(stream.as_mut(), &stitcher)?;
            if config.preview {
                force_preview_to_bottom(&mut preview_view, &stitcher, next_rect.height as u32);
                update_preview(
                    overlay.as_mut(),
                    &stitcher,
                    config.preview_width,
                    &mut preview_view,
                )?;
                handle_preview_events(
                    overlay.as_mut(),
                    &stitcher,
                    config.preview_width,
                    &mut preview_view,
                )?;
            }
        } else if config.grim_mode == GrimMode::Manual && !config.grim_fixed_width {
            if stitcher.push_frame_keep_widths(frame) {
                write_stream_update(stream.as_mut(), &stitcher)?;
            }
            if config.preview {
                force_preview_to_bottom(&mut preview_view, &stitcher, next_rect.height as u32);
                update_preview(
                    overlay.as_mut(),
                    &stitcher,
                    config.preview_width,
                    &mut preview_view,
                )?;
                handle_preview_events(
                    overlay.as_mut(),
                    &stitcher,
                    config.preview_width,
                    &mut preview_view,
                )?;
            }
        } else {
            if stitcher.push_frame(frame) {
                write_stream_update(stream.as_mut(), &stitcher)?;
            }
            if config.preview {
                force_preview_to_bottom(&mut preview_view, &stitcher, next_rect.height as u32);
                update_preview(
                    overlay.as_mut(),
                    &stitcher,
                    config.preview_width,
                    &mut preview_view,
                )?;
                handle_preview_events(
                    overlay.as_mut(),
                    &stitcher,
                    config.preview_width,
                    &mut preview_view,
                )?;
            }
        }
    }

    let image = stitcher
        .full
        .ok_or_else(|| "no frames captured from grim".to_string())?;
    let image = crop_image_vertical(image, preview_view.crop_top, preview_view.crop_bottom)?;
    write_png(&image, output)?;
    post_process(config, output, &image)?;
    Ok(())
}

fn grim_capture(geometry: &str) -> Result<Image, String> {
    let output = Command::new("grim")
        .arg("-g")
        .arg(geometry)
        .arg("-t")
        .arg("png")
        .arg("-")
        .output()
        .map_err(|error| format!("failed to start grim: {error}"))?;
    if !output.status.success() {
        return Err("grim failed to capture image".to_string());
    }
    let image = image::load_from_memory(&output.stdout)
        .map_err(|error| format!("failed to decode grim PNG: {error}"))?
        .to_rgba8();
    Ok(image)
}

fn run_slurp() -> Result<String, String> {
    let output = Command::new("slurp")
        .output()
        .map_err(|error| format!("failed to start slurp: {error}"))?;
    if !output.status.success() {
        return Ok(String::new());
    }
    String::from_utf8(output.stdout)
        .map(|text| text.trim().to_string())
        .map_err(|_| "slurp returned invalid UTF-8".to_string())
}

fn spawn_stop_controller(stop: Arc<AtomicBool>, config: &Config) -> Result<UnixStream, String> {
    let (read_end, mut write_end) = UnixStream::pair()
        .map_err(|error| format!("failed to create stop wake socket: {error}"))?;
    let control = config.control;
    let menu_cmd = if control == ControlMode::Menu {
        Some(resolve_menu_cmd(config.menu_cmd.as_deref())?)
    } else {
        None
    };
    thread::spawn(move || {
        match control {
            ControlMode::Stdin => {
                eprintln!("Press Enter to stop capturing.");
                let mut line = String::new();
                let _ = io::stdin().read_line(&mut line);
            }
            ControlMode::Menu => {
                let menu_cmd = menu_cmd.expect("menu command was resolved");
                loop {
                    match menu_select_stop(menu_cmd.as_str()) {
                        Ok(true) => break,
                        Ok(false) => thread::sleep(Duration::from_millis(100)),
                        Err(error) => {
                            eprintln!("error: {error}");
                            return;
                        }
                    }
                }
            }
        }
        stop.store(true, Ordering::SeqCst);
        let _ = write_end.write_all(&[1]);
    });
    Ok(read_end)
}

fn spawn_grim_stdin_action_controller() -> mpsc::Receiver<GrimAction> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        loop {
            eprintln!(
                "Press Enter to capture next, 'f' then Enter to finish, 'q' then Enter to abort."
            );
            let mut input = String::new();
            let Ok(bytes) = io::stdin().read_line(&mut input) else {
                let _ = tx.send(GrimAction::Finish);
                break;
            };
            let action = if bytes == 0 {
                GrimAction::Finish
            } else {
                match input.trim() {
                    "f" | "finish" => GrimAction::Finish,
                    "q" | "abort" => GrimAction::Abort,
                    _ => GrimAction::Next,
                }
            };
            let done = !matches!(action, GrimAction::Next);
            if tx.send(action).is_err() || done {
                break;
            }
        }
    });
    rx
}

fn wait_for_grim_action(
    rx: &mpsc::Receiver<GrimAction>,
    mut capturer: Option<&mut FrameCapturer>,
    stitcher: &Stitcher,
    preview: bool,
    preview_width: u32,
    preview_view: &mut PreviewView,
) -> Result<Option<GrimAction>, String> {
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(action) => return Ok(Some(action)),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if preview {
                    handle_preview_events(
                        capturer.as_deref_mut(),
                        stitcher,
                        preview_width,
                        preview_view,
                    )?;
                }
            }
        }
    }
}

fn wait_for_grim_menu_action(
    menu_cmd: String,
    mut capturer: Option<&mut FrameCapturer>,
    stitcher: &Stitcher,
    preview: bool,
    preview_width: u32,
    preview_view: &mut PreviewView,
) -> Result<Option<GrimAction>, String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(menu_select_grim_action(&menu_cmd));
    });
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if preview {
                    handle_preview_events(
                        capturer.as_deref_mut(),
                        stitcher,
                        preview_width,
                        preview_view,
                    )?;
                }
            }
        }
    }
}

fn sleep_with_preview_events(
    duration: Duration,
    stop: &AtomicBool,
    mut capturer: Option<&mut FrameCapturer>,
    stitcher: &Stitcher,
    preview: bool,
    preview_width: u32,
    preview_view: &mut PreviewView,
) -> Result<(), String> {
    let step = Duration::from_millis(50);
    let mut slept = Duration::ZERO;
    while slept < duration && !stop.load(Ordering::SeqCst) {
        let delay = (duration - slept).min(step);
        thread::sleep(delay);
        slept += delay;
        if preview {
            handle_preview_events(
                capturer.as_deref_mut(),
                stitcher,
                preview_width,
                preview_view,
            )?;
        }
    }
    Ok(())
}

fn crop_image_vertical(image: Image, top: u32, bottom: u32) -> Result<Image, String> {
    let height = image.height();
    let top = top.min(height.saturating_sub(1));
    let bottom = bottom.min(height.saturating_sub(top + 1));
    let new_height = height.saturating_sub(top).saturating_sub(bottom);
    if top == 0 && bottom == 0 {
        return Ok(image);
    }
    Ok(imageops::crop_imm(&image, 0, top, image.width(), new_height).to_image())
}

fn resolve_menu_cmd(menu_cmd: Option<&str>) -> Result<String, String> {
    if let Some(cmd) = menu_cmd.filter(|cmd| !cmd.trim().is_empty()) {
        return Ok(cmd.to_string());
    }
    for cmd in ["fuzzel", "rofi", "wofi"] {
        if Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {cmd} >/dev/null 2>&1"))
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
        {
            return Ok(cmd.to_string());
        }
    }
    Err("no menu command found for --control menu".to_string())
}

fn menu_select_stop(menu_cmd: &str) -> Result<bool, String> {
    let ui = ui_text();
    let choice = menu_select(menu_cmd, ui.recording, &[ui.stop], true)?;
    Ok(choice.as_deref() == Some(ui.stop))
}

fn menu_select_grim_action(menu_cmd: &str) -> Result<Option<GrimAction>, String> {
    let ui = ui_text();
    let choice = menu_select(
        menu_cmd,
        ui.grim_mode,
        &[ui.capture_next, ui.finish, ui.abort],
        false,
    )?;
    Ok(match choice.as_deref() {
        Some(value) if value == ui.capture_next => Some(GrimAction::Next),
        Some(value) if value == ui.finish => Some(GrimAction::Finish),
        Some(value) if value == ui.abort => Some(GrimAction::Abort),
        _ => None,
    })
}

fn ui_text() -> UiText {
    let lang = env::var("LANG").unwrap_or_default().to_ascii_lowercase();
    if lang.contains("zh") {
        UiText {
            capture_next: "截取下一张",
            finish: "完成",
            abort: "放弃",
            stop: "停止",
            recording: "录制中",
            grim_mode: "Grim 模式",
        }
    } else {
        UiText {
            capture_next: "Capture Next",
            finish: "Finish",
            abort: "Abort",
            stop: "Stop",
            recording: "Recording",
            grim_mode: "Grim Mode",
        }
    }
}

fn menu_select(
    menu_cmd: &str,
    prompt: &str,
    items: &[&str],
    stop_menu: bool,
) -> Result<Option<String>, String> {
    let input = items.join("\n") + "\n";
    let lines = items.len().max(1).to_string();
    let prompt_colon = format!("{prompt}: ");
    let mut command = if menu_cmd == "fuzzel" {
        let mut command = Command::new("fuzzel");
        command.args(["-d", "--anchor", "top", "--y-margin", "20"]);
        if stop_menu {
            command.args(["--lines", "1", "--width", "12", "-p", &format!("{prompt} ")]);
        } else {
            command.args(["--lines", &lines, "--width", "20", "-p", &prompt_colon]);
        }
        command
    } else if menu_cmd == "rofi" {
        let mut command = Command::new("rofi");
        command.args(["-dmenu", "-disable-history", "-p", prompt]);
        if stop_menu {
            command.args([
                "-location",
                "2",
                "-yoffset",
                "20",
                "-theme-str",
                "window {width: 250px;}",
                "-l",
                "1",
            ]);
        } else {
            command.args(["-theme-str", "window {width: 230px;}"]);
        }
        command
    } else if menu_cmd == "wofi" {
        let mut command = Command::new("wofi");
        command.args(["-d", "-i", "-p", prompt, "--cache-file", "/dev/null"]);
        if stop_menu {
            command.args([
                "-l", "top", "-y", "20", "-W", "250", "-H", "45", "--lines", "1",
            ]);
        } else {
            command.args(["--lines", &lines, "-W", "230"]);
        }
        command
    } else {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("{} -p '{}: '", menu_cmd, prompt));
        command
    };
    command.stdin(Stdio::piped()).stdout(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start menu command '{menu_cmd}': {error}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(input.as_bytes())
            .map_err(|error| format!("failed to write menu input: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("failed to read menu output: {error}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let choice = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if choice.is_empty() {
        Ok(None)
    } else {
        Ok(Some(choice))
    }
}

fn capture_and_stitch(
    capturer: &mut FrameCapturer,
    stop: Arc<AtomicBool>,
    stop_wake: &UnixStream,
    debug_dir: Option<&Path>,
    preview: bool,
    preview_width: u32,
    preview_view: &mut PreviewView,
    mut stream: Option<&mut StreamWriter>,
    mut timing: Option<&mut TimingStats>,
) -> Result<Image, String> {
    let mut stitcher = Stitcher::new();
    let mut frame_index = 0usize;

    if let Some(dir) = debug_dir {
        fs::create_dir_all(dir)
            .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
    }

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let capture_start = Instant::now();
        let captured = capturer
            .capture_frame_interruptible(|| stop.load(Ordering::SeqCst), Some(stop_wake))?;
        if let Some(timing) = timing.as_deref_mut() {
            timing.capture += capture_start.elapsed();
        }
        match captured {
            Some(frame) => {
                frame_index += 1;
                if let Some(dir) = debug_dir {
                    let path = dir.join(format!("capture_{frame_index:05}.png"));
                    let _ = frame.save(path);
                }
                let stitch_start = Instant::now();
                let outcome = stitcher.push_frame_result(frame);
                let accepted = outcome.accepted();
                if let Some(timing) = timing.as_deref_mut() {
                    timing.stitch += stitch_start.elapsed();
                }
                if preview {
                    sync_preview_view(preview_view, &stitcher, outcome, preview_width);
                }
                if accepted {
                    write_stream_update(stream.as_deref_mut(), &stitcher)?;
                    if let Some(dir) = debug_dir {
                        if let Some(full) = &stitcher.full {
                            let path = dir.join(format!("accepted_{frame_index:05}.png"));
                            let _ = full.save(path);
                        }
                    }
                }
                if preview {
                    update_preview(Some(capturer), &stitcher, preview_width, preview_view)?;
                    handle_preview_events(Some(capturer), &stitcher, preview_width, preview_view)?;
                }
                capturer.sleep_frame_interval(|| stop.load(Ordering::SeqCst));
            }
            None => break,
        }
    }

    let image = stitcher
        .full
        .ok_or_else(|| "no frames received from Wayland screencopy".to_string())?;
    eprintln!(
        "frames={} stitched={} height={}",
        stitcher.frames,
        stitcher.accepted,
        image.height()
    );
    Ok(image)
}

fn update_preview(
    capturer: Option<&mut FrameCapturer>,
    stitcher: &Stitcher,
    preview_width: u32,
    view: &mut PreviewView,
) -> Result<(), String> {
    if let (Some(capturer), Some(image)) = (capturer, stitcher.full.as_ref()) {
        sync_crop_with_image_height(view, image.height());
        capturer.update_preview(
            image,
            PreviewOptions {
                width: preview_width,
                zoomed: view.zoomed,
                source_pos: view.scrub_pos,
                frame_len: if view.following { view.frame_len } else { 0 },
                edge: if view.following {
                    preview_edge(view.edge)
                } else {
                    PreviewEdge::None
                },
                capture_pos: view.capture_pos,
                capture_len: view.capture_len,
                crop_enabled: view.crop_enabled,
                crop_top: view.crop_top,
                crop_bottom: view.crop_bottom,
            },
        )?;
    }
    Ok(())
}

fn write_stream_update(
    stream: Option<&mut StreamWriter>,
    stitcher: &Stitcher,
) -> Result<(), String> {
    if let (Some(stream), Some(image)) = (stream, stitcher.full.as_ref()) {
        stream.write_update(image, stitcher.frames, stitcher.accepted)?;
    }
    Ok(())
}

fn handle_preview_events(
    capturer: Option<&mut FrameCapturer>,
    stitcher: &Stitcher,
    preview_width: u32,
    view: &mut PreviewView,
) -> Result<(), String> {
    let (Some(capturer), Some(image)) = (capturer, stitcher.full.as_ref()) else {
        return Ok(());
    };
    let events = capturer.take_preview_events()?;
    sync_crop_with_image_height(view, image.height());
    if let Some(drag) = events.crop_drag.filter(|_| view.crop_enabled) {
        let image_height = image.height();
        match drag.edge {
            PreviewCropEdge::Top => {
                view.crop_top = drag
                    .source_y
                    .min(image_height.saturating_sub(view.crop_bottom + 1));
            }
            PreviewCropEdge::Bottom => {
                let bottom_source = drag
                    .source_y
                    .max(view.crop_top.saturating_add(1))
                    .min(image_height);
                view.crop_bottom = image_height.saturating_sub(bottom_source);
            }
        }
    }
    if !events.toggle_zoom && events.scroll_delta.abs() < f32::EPSILON && events.crop_drag.is_none()
    {
        return Ok(());
    }
    if events.toggle_zoom {
        view.zoomed = !view.zoomed;
        view.scrub_pos = view.scrub_pos.min(image.height().saturating_sub(1));
    }
    let amount = preview_scroll_amount(
        events.scroll_delta,
        image,
        preview_width,
        view.zoomed,
        view.capture_len,
    );
    if events.scroll_delta < 0.0 {
        view.scrub_pos = view.scrub_pos.saturating_sub(amount);
        view.following = false;
    } else if events.scroll_delta > 0.0 {
        view.scrub_pos = view.scrub_pos.saturating_add(amount);
        view.scrub_pos = view.scrub_pos.min(image.height().saturating_sub(1));
        view.following = false;
    }
    capturer.update_preview(
        image,
        PreviewOptions {
            width: preview_width,
            zoomed: view.zoomed,
            source_pos: view.scrub_pos,
            frame_len: if view.following { view.frame_len } else { 0 },
            edge: if view.following {
                preview_edge(view.edge)
            } else {
                PreviewEdge::None
            },
            capture_pos: view.capture_pos,
            capture_len: view.capture_len,
            crop_enabled: view.crop_enabled,
            crop_top: view.crop_top,
            crop_bottom: view.crop_bottom,
        },
    )?;
    Ok(())
}

fn sync_crop_with_image_height(view: &mut PreviewView, image_height: u32) {
    if image_height == 0 {
        view.crop_top = 0;
        view.crop_bottom = 0;
        view.crop_image_height = 0;
        return;
    }
    if view.crop_image_height > 0 && image_height > view.crop_image_height && view.crop_bottom > 0 {
        view.crop_bottom = view
            .crop_bottom
            .saturating_add(image_height - view.crop_image_height);
    }
    view.crop_top = view.crop_top.min(image_height.saturating_sub(1));
    view.crop_bottom = view
        .crop_bottom
        .min(image_height.saturating_sub(view.crop_top + 1));
    view.crop_image_height = image_height;
}

fn sync_preview_view(
    view: &mut PreviewView,
    stitcher: &Stitcher,
    outcome: StitchResult,
    _preview_width: u32,
) {
    let accepted_or_seen = matches!(
        outcome.status,
        StitchStatus::FirstFrame | StitchStatus::Appended
    );
    let stable_no_progress = outcome.status == StitchStatus::NoProgress && outcome.edge.is_none();
    let should_follow = matches!(
        outcome.status,
        StitchStatus::FirstFrame | StitchStatus::Appended
    );
    if outcome.status == StitchStatus::FirstFrame {
        view.capture_pos = 0;
        view.capture_len = outcome.frame_len;
    } else if accepted_or_seen || stable_no_progress {
        view.capture_pos =
            if outcome.status == StitchStatus::Appended && outcome.edge == Some(Edge::Start) {
                0
            } else {
                outcome.position.max(0) as u32
            };
        view.capture_len = outcome.frame_len;
    }
    if !view.following && should_follow {
        view.following = true;
        view.scrub_pos = view.capture_pos;
        view.frame_len = outcome.frame_len;
        view.edge = outcome.edge;
        return;
    }
    if !view.following {
        if outcome.edge == Some(Edge::Start) && outcome.added > 0 {
            view.scrub_pos = view.scrub_pos.saturating_add(outcome.added);
        }
        return;
    }
    if !should_follow {
        return;
    }
    let Some(full) = stitcher.full.as_ref() else {
        return;
    };
    view.scrub_pos = view.capture_pos.min(full.height().saturating_sub(1));
    view.frame_len = outcome.frame_len;
    view.edge = outcome.edge;
}

fn force_preview_to_bottom(view: &mut PreviewView, stitcher: &Stitcher, frame_len: u32) {
    let Some(full) = stitcher.full.as_ref() else {
        return;
    };
    view.following = true;
    view.frame_len = frame_len;
    view.edge = Some(Edge::End);
    view.capture_len = 0;
    view.capture_pos = full.height().saturating_sub(frame_len);
    view.scrub_pos = view.capture_pos.min(full.height().saturating_sub(1));
}

fn preview_edge(edge: Option<Edge>) -> PreviewEdge {
    match edge {
        Some(Edge::Start) => PreviewEdge::Start,
        Some(Edge::End) => PreviewEdge::End,
        None => PreviewEdge::None,
    }
}

fn preview_scroll_amount(
    delta: f32,
    image: &Image,
    preview_width: u32,
    zoomed: bool,
    capture_len: u32,
) -> u32 {
    let base = 16.max(estimated_preview_source_len(image, preview_width, zoomed, capture_len) / 8);
    let notches = (delta.abs() / 15.0).max(0.35);
    (notches * base as f32).round() as u32
}

fn estimated_preview_source_len(
    image: &Image,
    preview_width: u32,
    zoomed: bool,
    capture_len: u32,
) -> u32 {
    if image.width() == 0 || image.height() == 0 {
        return 1;
    }
    let content_width = if zoomed {
        preview_width.max(240)
    } else {
        preview_width.max(1)
    };
    let scaled_height =
        ((image.height() as u64 * content_width as u64) / image.width() as u64).max(1) as u32;
    let visible_scaled = scaled_height
        .min(capture_len.saturating_sub(16).max(1))
        .max(1);
    ((visible_scaled as u64 * image.height() as u64) / scaled_height as u64)
        .max(1)
        .min(image.height() as u64) as u32
}

fn print_timing(timing: &TimingStats) {
    eprintln!(
        "timing capture={}ms stitch={}ms write_png={}ms post_process={}ms total={}ms",
        timing.capture.as_millis(),
        timing.stitch.as_millis(),
        timing.write_png.as_millis(),
        timing.post_process.as_millis(),
        (timing.capture + timing.stitch + timing.write_png + timing.post_process).as_millis(),
    );
}

impl Stitcher {
    fn new() -> Self {
        Self {
            full: None,
            full_cols: Vec::new(),
            last_cols: Vec::new(),
            last_signature: None,
            anchor_pos: 0,
            last_offset: 0,
            pending_edge: None,
            growth_edge: None,
            frames: 0,
            accepted: 0,
        }
    }

    fn push_frame(&mut self, frame: Image) -> bool {
        self.push_frame_result(frame).accepted()
    }

    fn push_frame_result(&mut self, frame: Image) -> StitchResult {
        self.frames += 1;
        let frame_len = frame.height();
        let signature = frame_signature(&frame);
        if self
            .last_signature
            .as_ref()
            .is_some_and(|previous| is_duplicate_signature(previous, &signature))
        {
            return StitchResult {
                status: StitchStatus::NoProgress,
                added: 0,
                edge: None,
                position: self.anchor_pos,
                frame_len,
            };
        }
        let frame_cols = compute_cols(&frame);
        let height = frame.height() as i32;
        let width = frame.width();
        let min_overlap = effective_min_overlap(height);

        if self.full.is_none() {
            self.full_cols = frame_cols.clone();
            self.last_cols = frame_cols;
            self.last_signature = Some(signature);
            self.full = Some(frame);
            self.accepted += 1;
            return StitchResult {
                status: StitchStatus::FirstFrame,
                added: frame_len,
                edge: None,
                position: 0,
                frame_len,
            };
        }

        if self.full.as_ref().is_some_and(|full| full.width() != width) {
            return StitchResult {
                status: StitchStatus::NoMatch,
                added: 0,
                edge: None,
                position: self.anchor_pos,
                frame_len,
            };
        }

        let offset_match = self.find_offset(&frame_cols, min_overlap);
        let old_anchor = self.anchor_pos;
        let predicted_pos = self.anchor_pos + offset_match.pos;
        let mut pos = predicted_pos;

        if offset_match.diff > 9.0 {
            if let Some(known) = self.find_known_position(&frame_cols, predicted_pos, min_overlap) {
                if known.diff <= 9.0 {
                    pos = known.pos;
                } else if let Some(edge) =
                    self.find_edge_position(&frame_cols, predicted_pos, min_overlap)
                {
                    if edge.diff <= 9.0 {
                        pos = edge.pos;
                    } else {
                        if let Some(result) = self.try_edge_append_down(
                            &frame,
                            &frame_cols,
                            signature,
                            old_anchor,
                            frame_len,
                            min_overlap,
                        ) {
                            return result;
                        }
                        return StitchResult {
                            status: StitchStatus::NoMatch,
                            added: 0,
                            edge: None,
                            position: old_anchor,
                            frame_len,
                        };
                    }
                } else {
                    if let Some(result) = self.try_edge_append_down(
                        &frame,
                        &frame_cols,
                        signature,
                        old_anchor,
                        frame_len,
                        min_overlap,
                    ) {
                        return result;
                    }
                    return StitchResult {
                        status: StitchStatus::NoMatch,
                        added: 0,
                        edge: None,
                        position: old_anchor,
                        frame_len,
                    };
                }
            }
        }

        let full_height = self.full.as_ref().map_or(0, |full| full.height() as i32);
        let (amount, edge) = overhang_amount(pos, height, full_height);
        if amount == 0 {
            if let Some(known) = self.known_overlap_diff(&frame_cols, pos, min_overlap) {
                if known.diff <= 9.0 {
                    self.anchor_pos = pos;
                    self.last_offset = pos - old_anchor;
                    self.last_cols = frame_cols;
                    self.last_signature = Some(signature);
                    self.pending_edge = None;
                    return StitchResult {
                        status: StitchStatus::NoProgress,
                        added: 0,
                        edge: None,
                        position: pos,
                        frame_len,
                    };
                }
            }
            if let Some(result) = self.try_edge_append_down(
                &frame,
                &frame_cols,
                signature,
                old_anchor,
                frame_len,
                min_overlap,
            ) {
                return result;
            }
            return StitchResult {
                status: StitchStatus::NoMatch,
                added: 0,
                edge: None,
                position: old_anchor,
                frame_len,
            };
        }
        if amount < 15 {
            self.pending_edge = Some(edge);
            return StitchResult {
                status: StitchStatus::NoProgress,
                added: 0,
                edge: Some(edge),
                position: pos,
                frame_len,
            };
        }

        let Some(overlap) = self.known_overlap_diff(&frame_cols, pos, min_overlap) else {
            return StitchResult {
                status: StitchStatus::NoMatch,
                added: 0,
                edge: None,
                position: old_anchor,
                frame_len,
            };
        };
        if overlap.diff > 9.0 || overlap.pos < min_overlap {
            if let Some(known) = self.find_known_position(&frame_cols, predicted_pos, min_overlap) {
                if known.diff <= 9.0 {
                    self.anchor_pos = known.pos;
                    self.last_offset = known.pos - old_anchor;
                    self.last_cols = frame_cols;
                    self.last_signature = Some(signature);
                    self.pending_edge = None;
                    return StitchResult {
                        status: StitchStatus::NoProgress,
                        added: 0,
                        edge: None,
                        position: known.pos,
                        frame_len,
                    };
                }
            }
            if let Some(result) = self.try_edge_append_down(
                &frame,
                &frame_cols,
                signature,
                old_anchor,
                frame_len,
                min_overlap,
            ) {
                return result;
            }
            return StitchResult {
                status: StitchStatus::NoMatch,
                added: 0,
                edge: None,
                position: old_anchor,
                frame_len,
            };
        }

        if self.pending_edge.is_some_and(|pending| pending != edge) {
            self.pending_edge = None;
            return StitchResult {
                status: StitchStatus::NoMatch,
                added: 0,
                edge: None,
                position: old_anchor,
                frame_len,
            };
        }

        if self.growth_edge.is_some_and(|growth| growth != edge) {
            let at_boundary = match edge {
                Edge::Start => pos <= 0,
                Edge::End => pos + height >= full_height,
            };
            if !at_boundary {
                return StitchResult {
                    status: StitchStatus::NoMatch,
                    added: 0,
                    edge: None,
                    position: old_anchor,
                    frame_len,
                };
            }
        }

        if edge == Edge::Start {
            self.prepend_start(&frame, &frame_cols, amount as u32);
            self.anchor_pos = pos + amount;
        } else {
            self.append_end(&frame, &frame_cols, amount as u32);
            self.anchor_pos = pos;
        }
        self.last_cols = frame_cols;
        self.last_signature = Some(signature);
        self.last_offset = pos - old_anchor;
        self.pending_edge = None;
        self.growth_edge = Some(edge);
        self.accepted += 1;
        StitchResult {
            status: StitchStatus::Appended,
            added: amount as u32,
            edge: Some(edge),
            position: self.anchor_pos,
            frame_len,
        }
    }

    fn push_frame_keep_widths(&mut self, frame: Image) -> bool {
        let Some(full) = self.full.as_ref() else {
            return self.push_frame(frame);
        };
        if frame.width() == full.width() {
            return self.push_frame(frame);
        }
        let width = full.width().max(frame.width());
        if let Some(full) = self.full.take() {
            self.full = Some(pad_width(&full, width));
            self.full_cols = self.full.as_ref().map(compute_cols).unwrap_or_default();
        }
        self.push_frame(pad_width(&frame, width))
    }

    fn append_without_dedup(&mut self, frame: Image) {
        if self.full.is_none() {
            self.full_cols = compute_cols(&frame);
            self.last_cols = self.full_cols.clone();
            self.full = Some(frame);
            self.frames += 1;
            self.accepted += 1;
            return;
        }
        let full_width = self.full.as_ref().map_or(frame.width(), ImageBuffer::width);
        let width = full_width.max(frame.width());
        if frame.width() != width {
            let padded = pad_width(&frame, width);
            self.append_raw(&padded);
        } else {
            self.append_raw(&frame);
        }
    }

    fn append_raw(&mut self, frame: &Image) {
        if let Some(full) = self.full.take() {
            let full = if full.width() == frame.width() {
                full
            } else {
                pad_width(&full, frame.width())
            };
            let width = full.width();
            let old_height = full.height();
            let new_height = old_height + frame.height();
            let mut merged = Image::new(width, new_height);
            let row_bytes = width as usize * 4;
            let old_bytes = old_height as usize * row_bytes;
            merged.as_mut()[..old_bytes].copy_from_slice(full.as_raw());
            merged.as_mut()[old_bytes..].copy_from_slice(frame.as_raw());
            self.full = Some(merged);
            self.full_cols = self.full.as_ref().map(compute_cols).unwrap_or_default();
            self.last_cols = compute_cols(frame);
            self.frames += 1;
            self.accepted += 1;
        }
    }

    fn find_offset(&self, frame_cols: &[[f32; 3]], min_overlap: i32) -> MatchResult {
        let max_offset = (self.last_cols.len() as i32 - min_overlap).max(0);
        let mut best = MatchResult {
            pos: 0,
            diff: f32::INFINITY,
        };
        for offset in offset_candidates(max_offset, self.last_offset) {
            let diff = col_diff(&self.last_cols, frame_cols, offset, min_overlap);
            if diff < best.diff {
                best = MatchResult { pos: offset, diff };
            }
            if best.diff < 0.25 {
                break;
            }
        }
        best
    }

    fn known_overlap_diff(
        &self,
        frame_cols: &[[f32; 3]],
        pos: i32,
        min_overlap: i32,
    ) -> Option<MatchResult> {
        let full_height = self.full_cols.len() as i32;
        let frame_height = frame_cols.len() as i32;
        let full_start = 0.max(pos);
        let full_end = full_height.min(pos + frame_height);
        let length = full_end - full_start;
        if length < min_overlap {
            return None;
        }
        let frame_start = full_start - pos;
        let diff = range_diff(
            &self.full_cols,
            frame_cols,
            full_start as usize,
            frame_start as usize,
            length as usize,
            min_overlap,
        );
        Some(MatchResult { pos: length, diff })
    }

    fn find_known_position(
        &self,
        frame_cols: &[[f32; 3]],
        predicted_pos: i32,
        min_overlap: i32,
    ) -> Option<MatchResult> {
        let full_height = self.full.as_ref()?.height() as i32;
        let frame_height = frame_cols.len() as i32;
        let max_pos = full_height - frame_height;
        if max_pos < 0 {
            return None;
        }
        let mut best = MatchResult {
            pos: predicted_pos.clamp(0, max_pos),
            diff: f32::INFINITY,
        };
        let mut best_good = MatchResult {
            pos: best.pos,
            diff: f32::INFINITY,
        };
        let mut best_good_distance = i32::MAX;
        let visit = |pos: i32,
                     best: &mut MatchResult,
                     best_good: &mut MatchResult,
                     best_good_distance: &mut i32| {
            let pos = pos.clamp(0, max_pos);
            if let Some(diff) = self.known_overlap_diff(frame_cols, pos, min_overlap) {
                if diff.diff < best.diff {
                    *best = MatchResult {
                        pos,
                        diff: diff.diff,
                    };
                }
                if diff.diff <= 9.0 {
                    let distance = (pos - predicted_pos.clamp(0, max_pos)).abs();
                    if distance < *best_good_distance
                        || (distance == *best_good_distance && diff.diff < best_good.diff)
                    {
                        *best_good_distance = distance;
                        *best_good = MatchResult {
                            pos,
                            diff: diff.diff,
                        };
                    }
                }
            }
        };
        visit(
            predicted_pos,
            &mut best,
            &mut best_good,
            &mut best_good_distance,
        );
        visit(
            self.anchor_pos,
            &mut best,
            &mut best_good,
            &mut best_good_distance,
        );
        visit(0, &mut best, &mut best_good, &mut best_good_distance);
        visit(max_pos, &mut best, &mut best_good, &mut best_good_distance);
        let mut pos = 0;
        while pos <= max_pos {
            visit(pos, &mut best, &mut best_good, &mut best_good_distance);
            pos += 8;
        }
        visit(max_pos, &mut best, &mut best_good, &mut best_good_distance);
        let refine_center = best.pos;
        let start = (refine_center - 8).clamp(0, max_pos);
        let end = (refine_center + 8).clamp(0, max_pos);
        let mut pos = start;
        while pos <= end {
            visit(pos, &mut best, &mut best_good, &mut best_good_distance);
            pos += 1;
        }
        if best_good.diff <= 9.0 {
            Some(best_good)
        } else {
            Some(best)
        }
    }

    fn find_edge_position(
        &self,
        frame_cols: &[[f32; 3]],
        predicted_pos: i32,
        min_overlap: i32,
    ) -> Option<MatchResult> {
        let full_height = self.full.as_ref()?.height() as i32;
        let frame_height = frame_cols.len() as i32;
        let min_pos = min_overlap - frame_height;
        let max_pos = full_height - min_overlap;
        if min_pos > max_pos {
            return None;
        }
        let mut best = MatchResult {
            pos: predicted_pos.clamp(min_pos, max_pos),
            diff: f32::INFINITY,
        };
        let visit = |pos: i32, best: &mut MatchResult| {
            let pos = pos.clamp(min_pos, max_pos);
            let (amount, _) = overhang_amount(pos, frame_height, full_height);
            if amount > 0 {
                if let Some(diff) = self.known_overlap_diff(frame_cols, pos, min_overlap) {
                    if diff.diff < best.diff {
                        *best = MatchResult {
                            pos,
                            diff: diff.diff,
                        };
                    }
                }
            }
        };
        let scan = |start: i32, end: i32, best: &mut MatchResult| {
            let start = start.clamp(min_pos, max_pos);
            let end = end.clamp(min_pos, max_pos);
            let mut pos = start.min(end);
            while pos <= start.max(end) {
                visit(pos, best);
                pos += 8;
            }
            visit(end, best);
        };
        scan(predicted_pos - 160, predicted_pos + 160, &mut best);
        scan(min_pos, -1, &mut best);
        scan(full_height - frame_height + 1, max_pos, &mut best);
        if best.diff.is_finite() {
            let start = (best.pos - 8).clamp(min_pos, max_pos);
            let end = (best.pos + 8).clamp(min_pos, max_pos);
            let mut pos = start;
            while pos <= end {
                visit(pos, &mut best);
                pos += 1;
            }
        }
        Some(best)
    }

    fn try_edge_append_down(
        &mut self,
        frame: &Image,
        frame_cols: &[[f32; 3]],
        signature: Vec<u8>,
        old_anchor: i32,
        frame_len: u32,
        min_overlap: i32,
    ) -> Option<StitchResult> {
        let full = self.full.as_ref()?;
        if full.width() != frame.width() {
            return None;
        }
        let overlap = edge_overlap_tail_head(full, frame, min_overlap as u32)?;
        let amount = frame.height().saturating_sub(overlap);
        let pos = full.height() as i32 - overlap as i32;
        if amount == 0 {
            self.anchor_pos = pos;
            self.last_offset = pos - old_anchor;
            self.last_cols = frame_cols.to_vec();
            self.last_signature = Some(signature);
            self.pending_edge = None;
            return Some(StitchResult {
                status: StitchStatus::NoProgress,
                added: 0,
                edge: None,
                position: pos,
                frame_len,
            });
        }
        if amount < 15 {
            return Some(StitchResult {
                status: StitchStatus::NoProgress,
                added: 0,
                edge: Some(Edge::End),
                position: pos,
                frame_len,
            });
        }
        self.append_end(frame, frame_cols, amount);
        self.anchor_pos = pos;
        self.last_cols = frame_cols.to_vec();
        self.last_signature = Some(signature);
        self.last_offset = pos - old_anchor;
        self.pending_edge = None;
        self.growth_edge = Some(Edge::End);
        self.accepted += 1;
        Some(StitchResult {
            status: StitchStatus::Appended,
            added: amount,
            edge: Some(Edge::End),
            position: pos,
            frame_len,
        })
    }

    fn append_end(&mut self, frame: &Image, frame_cols: &[[f32; 3]], amount: u32) {
        let full = self.full.take().expect("full image exists");
        let width = full.width();
        let old_height = full.height();
        let new_height = old_height + amount;
        let mut merged = Image::new(width, new_height);
        let row_bytes = width as usize * 4;
        let old_bytes = old_height as usize * row_bytes;
        merged.as_mut()[..old_bytes].copy_from_slice(full.as_raw());
        let start = frame.height() - amount;
        for y in 0..amount as usize {
            let source_start = (start as usize + y) * row_bytes;
            let source_end = source_start + row_bytes;
            let dest_start = old_bytes + y * row_bytes;
            let dest_end = dest_start + row_bytes;
            merged.as_mut()[dest_start..dest_end]
                .copy_from_slice(&frame.as_raw()[source_start..source_end]);
        }
        let col_start = frame.height().saturating_sub(amount) as usize;
        self.full_cols.extend_from_slice(&frame_cols[col_start..]);
        self.full = Some(merged);
    }

    fn prepend_start(&mut self, frame: &Image, frame_cols: &[[f32; 3]], amount: u32) {
        let full = self.full.take().expect("full image exists");
        let width = full.width();
        let old_height = full.height();
        let new_height = old_height + amount;
        let mut merged = Image::new(width, new_height);
        let row_bytes = width as usize * 4;
        let prepend_bytes = amount as usize * row_bytes;
        merged.as_mut()[..prepend_bytes].copy_from_slice(&frame.as_raw()[..prepend_bytes]);
        merged.as_mut()[prepend_bytes..prepend_bytes + old_height as usize * row_bytes]
            .copy_from_slice(full.as_raw());
        let mut new_cols = Vec::with_capacity(new_height as usize);
        new_cols.extend_from_slice(&frame_cols[..amount as usize]);
        new_cols.extend_from_slice(&self.full_cols);
        self.full_cols = new_cols;
        self.full = Some(merged);
    }
}

fn effective_min_overlap(frame_height: i32) -> i32 {
    100.min(12.max(frame_height / 4))
}

fn edge_overlap_tail_head(base: &Image, next: &Image, min_overlap: u32) -> Option<u32> {
    let search_h = base.height().min(next.height());
    if search_h <= min_overlap || base.width() == 0 || next.width() == 0 {
        return None;
    }
    let samples = base.width().min(160).max(1);
    let base_edges = edge_rows(base, base.height() - search_h, search_h, samples);
    let next_edges = edge_rows(next, 0, search_h, samples);
    let mut best_overlap = 0;
    let mut best_diff = f32::INFINITY;

    for overlap in min_overlap..search_h {
        let base_start = search_h - overlap;
        let next_start = 0;
        let base_slice =
            &base_edges[(base_start * samples) as usize..(search_h * samples) as usize];
        let next_slice = &next_edges
            [(next_start * samples) as usize..((next_start + overlap) * samples) as usize];
        let active = base_slice.iter().filter(|value| **value > 18).count()
            + next_slice.iter().filter(|value| **value > 18).count();
        if active < 24 {
            continue;
        }
        let diff = edge_diff(base_slice, next_slice);
        if diff < best_diff {
            best_diff = diff;
            best_overlap = overlap;
        }
    }

    if best_overlap > 0 && best_diff <= 16.0 {
        Some(best_overlap)
    } else {
        None
    }
}

fn edge_rows(image: &Image, start_y: u32, height: u32, samples: u32) -> Vec<u8> {
    let mut rows = Vec::with_capacity((height * samples) as usize);
    for local_y in 0..height {
        let y = start_y + local_y;
        let prev_y = y.saturating_sub(1);
        let mut previous = None;
        for step in 0..samples {
            let x = if samples == 1 {
                0
            } else {
                ((step as u64 * (image.width() - 1) as u64) / (samples - 1) as u64) as u32
            };
            let gray = gray_pixel_alpha(image.get_pixel(x, y));
            let left = previous.unwrap_or(gray);
            let up = gray_pixel_alpha(image.get_pixel(x, prev_y));
            let edge = (gray - left).abs() + (gray - up).abs();
            rows.push(edge.min(255.0).round() as u8);
            previous = Some(gray);
        }
    }
    rows
}

fn edge_diff(a: &[u8], b: &[u8]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return f32::INFINITY;
    }
    let mut total = 0u64;
    for (left, right) in a.iter().zip(b.iter()) {
        total += left.abs_diff(*right) as u64;
    }
    total as f32 / a.len() as f32
}

fn compute_cols(image: &Image) -> Vec<[f32; 3]> {
    let height = image.height();
    let width = image.width();
    let mut result = Vec::with_capacity(height as usize);
    for y in 0..height {
        let count = width.min(96).max(1);
        let mut values = Vec::with_capacity(count as usize);
        let mut total = 0.0;
        for step in 0..count {
            let x = if count == 1 {
                0
            } else {
                ((step as u64 * (width - 1) as u64) / (count - 1) as u64) as u32
            };
            let gray = gray_pixel(image.get_pixel(x, y));
            total += gray;
            values.push(gray);
        }
        let mean = total / count as f32;
        let mut bright = 0.0;
        let mut edges = 0.0;
        let mut contrast = 0.0;
        let mut previous = None;
        for gray in values.iter().copied() {
            contrast += f32::abs(gray - mean);
            bright += (gray - mean - 8.0).max(0.0);
            if let Some(previous) = previous {
                edges += f32::abs(gray - previous);
            }
            previous = Some(gray);
        }
        let edge_count = count.saturating_sub(1).max(1) as f32;
        result.push([
            mean * 2.0,
            contrast / count as f32,
            (bright / count as f32) + (edges / edge_count),
        ]);
    }
    result
}

fn pad_width(image: &Image, width: u32) -> Image {
    if image.width() >= width {
        return image.clone();
    }
    let mut padded = Image::new(width, image.height());
    let source_row = image.width() as usize * 4;
    let dest_row = width as usize * 4;
    for y in 0..image.height() as usize {
        let source_start = y * source_row;
        let source_end = source_start + source_row;
        let dest_start = y * dest_row;
        let dest_end = dest_start + source_row;
        padded.as_mut()[dest_start..dest_end]
            .copy_from_slice(&image.as_raw()[source_start..source_end]);
    }
    padded
}

fn gray_pixel(pixel: &Rgba<u8>) -> f32 {
    0.299 * pixel[0] as f32 + 0.587 * pixel[1] as f32 + 0.114 * pixel[2] as f32
}

fn gray_pixel_alpha(pixel: &Rgba<u8>) -> f32 {
    let alpha = pixel[3] as f32 / 255.0;
    gray_pixel(pixel) * alpha + 255.0 * (1.0 - alpha)
}

fn col_diff(cols1: &[[f32; 3]], cols2: &[[f32; 3]], offset: i32, min_overlap: i32) -> f32 {
    let h1 = cols1.len() as i32;
    let h2 = cols2.len() as i32;
    let (a_start, b_start, length) = if offset >= 0 {
        (offset, 0, (h1 - offset).min(h2))
    } else {
        (0, -offset, h1.min(h2 + offset))
    };
    if length < min_overlap {
        return f32::INFINITY;
    }
    range_diff(
        cols1,
        cols2,
        a_start as usize,
        b_start as usize,
        length as usize,
        min_overlap,
    )
}

fn range_diff(
    cols1: &[[f32; 3]],
    cols2: &[[f32; 3]],
    a_start: usize,
    b_start: usize,
    length: usize,
    min_overlap: i32,
) -> f32 {
    let top = content_top_ignore(length);
    let bottom = content_bottom_ignore(length);
    if length < min_overlap as usize + top + bottom {
        return f32::INFINITY;
    }
    let end = length.saturating_sub(bottom);
    let mut total = 0.0;
    let mut count = 0usize;
    for row in top..end {
        let a = cols1[a_start + row];
        let b = cols2[b_start + row];
        total += (a[0] - b[0]).abs() + (a[1] - b[1]).abs() + (a[2] - b[2]).abs();
        count += 3;
    }
    if count == 0 {
        f32::INFINITY
    } else {
        total / count as f32
    }
}

fn content_top_ignore(height: usize) -> usize {
    if height < 80 {
        0
    } else {
        (height / 4).min(16.max(height / 10))
    }
}

fn content_bottom_ignore(height: usize) -> usize {
    if height < 80 {
        0
    } else {
        (height / 4).min(16.max(height * 8 / 100))
    }
}

fn offset_candidates(max_offset: i32, predict: i32) -> Vec<i32> {
    let predict = predict.clamp(-max_offset, max_offset);
    let mut candidates = vec![predict];
    for delta in 1..=max_offset * 2 {
        if predict + delta <= max_offset {
            candidates.push(predict + delta);
        }
        if predict - delta >= -max_offset {
            candidates.push(predict - delta);
        }
    }
    candidates
}

fn overhang_amount(pos: i32, frame_height: i32, full_height: i32) -> (i32, Edge) {
    let over_top = 0.max(-pos);
    let over_bottom = 0.max(pos + frame_height - full_height);
    if over_bottom >= over_top {
        (over_bottom, Edge::End)
    } else {
        (over_top, Edge::Start)
    }
}

fn frame_signature(image: &Image) -> Vec<u8> {
    let cols = 18;
    let rows = 24;
    let mut signature = Vec::with_capacity(cols * rows);
    for row in 0..rows {
        let y = ((row as u32 * image.height()) / rows as u32).min(image.height() - 1);
        for col in 0..cols {
            let x = ((col as u32 * image.width()) / cols as u32).min(image.width() - 1);
            signature.push(gray_pixel(image.get_pixel(x, y)).round() as u8);
        }
    }
    signature
}

fn is_duplicate_signature(previous: &[u8], current: &[u8]) -> bool {
    if previous.len() != current.len() || previous.is_empty() {
        return false;
    }
    let mut total = 0u32;
    let mut max = 0u8;
    for (a, b) in previous.iter().zip(current.iter()) {
        let diff = a.abs_diff(*b);
        total += diff as u32;
        max = max.max(diff);
    }
    total as f32 / previous.len() as f32 <= 1.1 && max <= 4
}

fn write_png(image: &Image, target: &OutputTarget) -> Result<(), String> {
    match target {
        OutputTarget::File(path) => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
            }
            let mut file = File::create(path)
                .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
            encode_png_fast(image, &mut file)
                .map_err(|error| format!("failed to write {}: {error}", path.display()))
        }
        OutputTarget::Stdout => {
            let stdout = io::stdout();
            let mut lock = stdout.lock();
            encode_png_fast(image, &mut lock)
                .map_err(|error| format!("failed to write PNG to stdout: {error}"))?;
            lock.flush()
                .map_err(|error| format!("failed to flush stdout: {error}"))
        }
    }
}

fn encode_png_fast<W: Write>(image: &Image, writer: &mut W) -> image::ImageResult<()> {
    let encoder = PngEncoder::new_with_quality(writer, CompressionType::Fast, FilterType::NoFilter);
    encoder.write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        image::ColorType::Rgba8.into(),
    )
}

fn post_process(config: &Config, output: &OutputTarget, image: &Image) -> Result<(), String> {
    let OutputTarget::File(path) = output else {
        return Ok(());
    };
    if config.copy {
        copy_image_to_clipboard(image)?;
    }
    if config.open {
        spawn_detached("xdg-open", path)?;
    }
    if config.edit {
        spawn_detached("satty", path)?;
    }
    Ok(())
}

fn copy_image_to_clipboard(image: &Image) -> Result<(), String> {
    let clipboard_image = clipboard_image(image);
    let mut data = Vec::new();
    encode_png_compact_rgb(&clipboard_image, &mut data)
        .map_err(|error| format!("failed to encode clipboard PNG: {error}"))?;
    let mut child = Command::new("wl-copy")
        .arg("--foreground")
        .arg("-t")
        .arg("image/png")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("failed to start wl-copy: {error}"))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "failed to open wl-copy stdin".to_string())?;
        stdin
            .write_all(&data)
            .map_err(|error| format!("failed to write image to wl-copy: {error}"))?;
    }
    drop(child);
    Ok(())
}

fn clipboard_image(image: &Image) -> Image {
    let pixels = image.width() as u64 * image.height() as u64;
    if pixels <= CLIPBOARD_MAX_PIXELS && image.height() <= CLIPBOARD_MAX_HEIGHT {
        return image.clone();
    }
    let scale_by_pixels = (CLIPBOARD_MAX_PIXELS as f64 / pixels.max(1) as f64).sqrt();
    let scale_by_height = CLIPBOARD_MAX_HEIGHT as f64 / image.height().max(1) as f64;
    let scale = scale_by_pixels.min(scale_by_height).min(1.0);
    let width = ((image.width() as f64 * scale).round() as u32).max(1);
    let height = ((image.height() as f64 * scale).round() as u32).max(1);
    imageops::resize(image, width, height, ResizeFilterType::Triangle)
}

fn encode_png_compact_rgb<W: Write>(image: &Image, writer: &mut W) -> image::ImageResult<()> {
    let mut rgb = Vec::with_capacity(image.width() as usize * image.height() as usize * 3);
    for pixel in image.pixels() {
        rgb.extend_from_slice(&[pixel[0], pixel[1], pixel[2]]);
    }
    let encoder =
        PngEncoder::new_with_quality(writer, CompressionType::Default, FilterType::Adaptive);
    encoder.write_image(
        &rgb,
        image.width(),
        image.height(),
        image::ColorType::Rgb8.into(),
    )
}

fn spawn_detached(command: &str, path: &Path) -> Result<(), String> {
    Command::new(command)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to start {command}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_color(index: i32) -> Rgba<u8> {
        let index = index.wrapping_add(10_000) as u32;
        let hash = index.wrapping_mul(2_654_435_761).rotate_left(13) ^ index.wrapping_mul(97_531);
        Rgba([(hash >> 16) as u8, (hash >> 8) as u8, hash as u8, 255])
    }

    fn vertical_frame(first_row: i32, height: u32) -> Image {
        let width = 24;
        let mut image = Image::new(width, height);
        for y in 0..height {
            let color = row_color(first_row + y as i32);
            for x in 0..width {
                image.put_pixel(x, y, color);
            }
        }
        image
    }

    fn edge_pattern_frame(first_row: u32, height: u32) -> Image {
        let width = 64;
        let mut image = Image::new(width, height);
        for y in 0..height {
            let global_y = first_row + y;
            for x in 0..width {
                let mark = (x + global_y * 3) % 17 == 0 || (x * 5 + global_y) % 29 == 0;
                let value = if mark { 20 } else { 235 };
                image.put_pixel(x, y, Rgba([value, value, value, 255]));
            }
        }
        image
    }

    #[test]
    fn edge_overlap_matches_tail_to_head() {
        let base = edge_pattern_frame(0, 80);
        let next = edge_pattern_frame(50, 80);
        assert_eq!(edge_overlap_tail_head(&base, &next, 12), Some(30));
    }

    #[test]
    fn crop_image_vertical_keeps_middle_rows() {
        let image = vertical_frame(0, 10);
        let cropped = crop_image_vertical(image, 2, 3).expect("cropped image");
        assert_eq!(cropped.height(), 5);
        assert_eq!(*cropped.get_pixel(0, 0), row_color(2));
        assert_eq!(*cropped.get_pixel(0, 4), row_color(6));
    }

    #[test]
    fn no_progress_updates_capture_indicator_without_forcing_follow() {
        let mut stitcher = Stitcher::new();
        assert_eq!(
            stitcher.push_frame_result(vertical_frame(0, 80)).status,
            StitchStatus::FirstFrame
        );
        assert_eq!(
            stitcher.push_frame_result(vertical_frame(60, 80)).status,
            StitchStatus::Appended
        );

        let mut view = PreviewView::default();
        sync_preview_view(
            &mut view,
            &stitcher,
            StitchResult {
                status: StitchStatus::NoProgress,
                added: 0,
                edge: None,
                position: 30,
                frame_len: 80,
            },
            320,
        );
        assert_eq!(view.capture_pos, 30);
        assert_eq!(view.capture_len, 80);
        assert_eq!(view.scrub_pos, 0);

        sync_preview_view(
            &mut view,
            &stitcher,
            StitchResult {
                status: StitchStatus::NoProgress,
                added: 0,
                edge: Some(Edge::Start),
                position: 44,
                frame_len: 80,
            },
            320,
        );
        assert_eq!(view.capture_pos, 30);
    }

    #[test]
    fn manual_append_forces_preview_to_bottom() {
        let mut stitcher = Stitcher::new();
        assert_eq!(
            stitcher.push_frame_result(vertical_frame(0, 80)).status,
            StitchStatus::FirstFrame
        );
        stitcher.append_without_dedup(vertical_frame(80, 80));

        let mut view = PreviewView {
            following: false,
            scrub_pos: 10,
            capture_pos: 10,
            capture_len: 80,
            ..Default::default()
        };
        force_preview_to_bottom(&mut view, &stitcher, 80);
        assert!(view.following);
        assert_eq!(view.capture_pos, 80);
        assert_eq!(view.capture_len, 0);
        assert_eq!(view.scrub_pos, 80);
        assert_eq!(view.edge, Some(Edge::End));
    }

    #[test]
    fn upward_auto_follow_keeps_preview_at_top() {
        let mut stitcher = Stitcher::new();
        let first = stitcher.push_frame_result(vertical_frame(120, 80));
        let mut view = PreviewView::default();
        sync_preview_view(&mut view, &stitcher, first, 320);

        let second = stitcher.push_frame_result(vertical_frame(60, 80));
        assert_eq!(second.status, StitchStatus::Appended);
        assert_eq!(second.edge, Some(Edge::Start));
        sync_preview_view(&mut view, &stitcher, second, 320);
        assert!(view.following);
        assert_eq!(view.capture_pos, 0);
        assert_eq!(view.scrub_pos, 0);
        assert_eq!(view.edge, Some(Edge::Start));

        let duplicate = StitchResult {
            status: StitchStatus::NoProgress,
            added: 0,
            edge: None,
            position: 0,
            frame_len: 80,
        };
        sync_preview_view(&mut view, &stitcher, duplicate, 320);
        assert_eq!(view.capture_pos, 0);
        assert_eq!(view.scrub_pos, 0);
        assert_eq!(view.edge, Some(Edge::Start));
    }

    #[test]
    fn appends_downward_frames() {
        let mut stitcher = Stitcher::new();
        let first = stitcher.push_frame_result(vertical_frame(0, 80));
        assert_eq!(first.status, StitchStatus::FirstFrame);
        let second = stitcher.push_frame_result(vertical_frame(60, 80));
        assert_eq!(second.status, StitchStatus::Appended);
        assert_eq!(second.edge, Some(Edge::End));
        assert_eq!(second.added, 60);
        let full = stitcher.full.as_ref().expect("stitched image");
        assert_eq!(full.height(), 140);
        assert_eq!(*full.get_pixel(0, 0), row_color(0));
        assert_eq!(*full.get_pixel(0, 139), row_color(139));
    }

    #[test]
    fn prepends_upward_frames() {
        let mut stitcher = Stitcher::new();
        assert_eq!(
            stitcher.push_frame_result(vertical_frame(60, 80)).status,
            StitchStatus::FirstFrame
        );

        let second = stitcher.push_frame_result(vertical_frame(0, 80));
        assert_eq!(second.status, StitchStatus::Appended);
        assert_eq!(second.edge, Some(Edge::Start));
        assert_eq!(second.added, 60);

        let full = stitcher.full.as_ref().expect("stitched image");
        assert_eq!(full.height(), 140);
        assert_eq!(*full.get_pixel(0, 0), row_color(0));
        assert_eq!(*full.get_pixel(0, 139), row_color(139));
    }

    #[test]
    fn can_prepend_after_downward_scroll_then_append_again() {
        let mut stitcher = Stitcher::new();
        assert_eq!(
            stitcher.push_frame_result(vertical_frame(0, 80)).status,
            StitchStatus::FirstFrame
        );
        assert_eq!(
            stitcher.push_frame_result(vertical_frame(60, 80)).status,
            StitchStatus::Appended
        );

        let upward = stitcher.push_frame_result(vertical_frame(-20, 80));
        assert_eq!(upward.status, StitchStatus::Appended);
        assert_eq!(upward.edge, Some(Edge::Start));
        assert_eq!(upward.added, 20);

        let known = stitcher.push_frame_result(vertical_frame(60, 80));
        assert_eq!(known.status, StitchStatus::NoProgress);

        let downward = stitcher.push_frame_result(vertical_frame(100, 80));
        assert_eq!(downward.status, StitchStatus::Appended);
        assert_eq!(downward.edge, Some(Edge::End));
        assert_eq!(downward.added, 40);

        let full = stitcher.full.as_ref().expect("stitched image");
        assert_eq!(full.height(), 200);
        assert_eq!(*full.get_pixel(0, 0), row_color(-20));
        assert_eq!(*full.get_pixel(0, 199), row_color(179));
    }

    #[test]
    fn continuous_upward_scroll_prepends_multiple_times() {
        let mut stitcher = Stitcher::new();
        assert_eq!(
            stitcher.push_frame_result(vertical_frame(120, 80)).status,
            StitchStatus::FirstFrame
        );

        let second = stitcher.push_frame_result(vertical_frame(60, 80));
        assert_eq!(second.status, StitchStatus::Appended);
        assert_eq!(second.edge, Some(Edge::Start));
        assert_eq!(second.added, 60);

        let third = stitcher.push_frame_result(vertical_frame(0, 80));
        assert_eq!(third.status, StitchStatus::Appended);
        assert_eq!(third.edge, Some(Edge::Start));
        assert_eq!(third.added, 60);

        let full = stitcher.full.as_ref().expect("stitched image");
        assert_eq!(full.height(), 200);
        assert_eq!(*full.get_pixel(0, 0), row_color(0));
        assert_eq!(*full.get_pixel(0, 199), row_color(199));
    }

    #[test]
    fn rejected_frame_does_not_poison_next_append() {
        let mut stitcher = Stitcher::new();
        assert_eq!(
            stitcher.push_frame_result(vertical_frame(0, 80)).status,
            StitchStatus::FirstFrame
        );
        assert_eq!(
            stitcher.push_frame_result(vertical_frame(60, 80)).status,
            StitchStatus::Appended
        );

        let bad = stitcher.push_frame_result(vertical_frame(500, 80));
        assert_eq!(bad.status, StitchStatus::NoMatch);

        let next = stitcher.push_frame_result(vertical_frame(100, 80));
        assert_eq!(next.status, StitchStatus::Appended);
        assert_eq!(next.edge, Some(Edge::End));
        assert_eq!(next.added, 40);

        let full = stitcher.full.as_ref().expect("stitched image");
        assert_eq!(full.height(), 180);
        assert_eq!(*full.get_pixel(0, 179), row_color(179));
    }
}
