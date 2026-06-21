mod capture;

use capture::{
    FrameCapturer, OverlayConfig, PreviewConfig, PreviewEdge, PreviewOptions, Rect, parse_color,
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
        copy: false,
        open: false,
        edit: false,
        debug_dir: None,
        debug_timing: false,
        show_border: true,
        border_color: None,
        preview: false,
        preview_width: 320,
        stream: None,
        stream_keep_frames: false,
        stream_every: 1,
        grim_mode: GrimMode::Auto,
        grim_fixed_width: true,
        grim_dedup: true,
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
        "      --preview           Show a live layer-shell preview beside the capture region."
    );
    println!("      --preview-width <n> Preview content width in pixels. Defaults to 320.");
    println!("                         Left-click preview to zoom; use mouse wheel to scroll.");
    println!("      --stream <dir>      Write accepted intermediate PNGs to a stream directory.");
    println!("      --stream-keep-frames  Also keep numbered PNG snapshots under frames/.");
    println!("      --stream-every <n>  Stream every N accepted updates. Defaults to 1.");
    println!("      --grim-mode <mode>  Grim mode: auto, manual. Defaults to auto.");
    println!("      --no-grim-fixed-width  Allow manual grim captures to change width.");
    println!("      --no-grim-dedup     Append manual grim captures without overlap dedup.");
    println!("  -c, --copy              Copy result to clipboard with wl-copy.");
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
    let stop_wake = spawn_enter_stop(stop.clone())?;
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

    if config.preview && config.grim_mode == GrimMode::Auto {
        let stop = Arc::new(AtomicBool::new(false));
        let _stop_wake = spawn_enter_stop(stop.clone())?;
        let frame_interval = if config.fps == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(1.0 / config.fps as f64)
        };
        while !stop.load(Ordering::SeqCst) {
            let started = Instant::now();
            let frame = grim_capture(&geometry)?;
            let outcome = stitcher.push_frame_result(frame);
            let accepted = outcome.accepted();
            sync_preview_view(&mut preview_view, &stitcher, outcome, config.preview_width);
            if accepted {
                write_stream_update(stream.as_mut(), &stitcher)?;
            }
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
            let elapsed = started.elapsed();
            if elapsed < frame_interval {
                thread::sleep(frame_interval - elapsed);
            }
        }

        let image = stitcher
            .full
            .ok_or_else(|| "no frames captured from grim".to_string())?;
        write_png(&image, output)?;
        post_process(config, output, &image)?;
        return Ok(());
    }

    loop {
        eprintln!(
            "Press Enter to capture next, 'f' then Enter to finish, 'q' then Enter to abort."
        );
        let mut input = String::new();
        let bytes = io::stdin()
            .read_line(&mut input)
            .map_err(|error| format!("failed to read input: {error}"))?;
        if bytes == 0 {
            break;
        }
        match input.trim() {
            "f" | "finish" => break,
            "q" | "abort" => return Ok(()),
            _ => {}
        }

        let next_geometry = match config.grim_mode {
            GrimMode::Auto => geometry.clone(),
            GrimMode::Manual => {
                let selected = run_slurp()?;
                if selected.trim().is_empty() {
                    continue;
                }
                if config.grim_fixed_width {
                    let next = Rect::parse(&selected)?;
                    format!(
                        "{},{} {}x{}",
                        base_rect.x, next.y, base_rect.width, next.height
                    )
                } else {
                    selected
                }
            }
        };

        let frame = grim_capture(&next_geometry)?;
        if config.grim_mode == GrimMode::Manual && !config.grim_dedup {
            stitcher.append_without_dedup(frame);
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
        } else if config.grim_mode == GrimMode::Manual && !config.grim_fixed_width {
            if stitcher.push_frame_keep_widths(frame) {
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
        } else {
            if stitcher.push_frame(frame) {
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
        }
    }

    let image = stitcher
        .full
        .ok_or_else(|| "no frames captured from grim".to_string())?;
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

fn spawn_enter_stop(stop: Arc<AtomicBool>) -> Result<UnixStream, String> {
    let (read_end, mut write_end) = UnixStream::pair()
        .map_err(|error| format!("failed to create stop wake socket: {error}"))?;
    eprintln!("Press Enter to stop capturing.");
    thread::spawn(move || {
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
        stop.store(true, Ordering::SeqCst);
        let _ = write_end.write_all(&[1]);
    });
    Ok(read_end)
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
    if !events.toggle_zoom && events.scroll_delta.abs() < f32::EPSILON {
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
        },
    )?;
    Ok(())
}

fn sync_preview_view(
    view: &mut PreviewView,
    stitcher: &Stitcher,
    outcome: StitchResult,
    _preview_width: u32,
) {
    let previous_capture_pos = view.capture_pos;
    let previous_capture_len = view.capture_len;
    let moved = matches!(
        outcome.status,
        StitchStatus::FirstFrame | StitchStatus::Appended | StitchStatus::NoProgress
    ) && (outcome.position.max(0) as u32 != previous_capture_pos
        || outcome.frame_len != previous_capture_len
        || outcome.added > 0);
    if matches!(
        outcome.status,
        StitchStatus::FirstFrame | StitchStatus::Appended | StitchStatus::NoProgress
    ) {
        view.capture_pos = outcome.position.max(0) as u32;
        view.capture_len = outcome.frame_len;
    }
    if !view.following && moved {
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
    if !matches!(
        outcome.status,
        StitchStatus::FirstFrame | StitchStatus::Appended | StitchStatus::NoProgress
    ) {
        return;
    }
    let Some(full) = stitcher.full.as_ref() else {
        return;
    };
    view.scrub_pos = view.capture_pos.min(full.height().saturating_sub(1));
    view.frame_len = outcome.frame_len;
    view.edge = outcome.edge;
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
                        return StitchResult {
                            status: StitchStatus::NoMatch,
                            added: 0,
                            edge: None,
                            position: old_anchor,
                            frame_len,
                        };
                    }
                } else {
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
