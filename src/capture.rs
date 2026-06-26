use image::{ImageBuffer, Rgba};
use memmap2::{MmapMut, MmapOptions};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::env;
use std::fs;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;
use tempfile::tempfile;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

pub type Image = ImageBuffer<Rgba<u8>, Vec<u8>>;
const APP_ID: &str = "wl-longshot";

#[derive(Clone, Copy, Debug)]
pub struct OverlayConfig {
    pub enabled: bool,
    pub color: Option<[u8; 3]>,
    pub preview: PreviewConfig,
}

#[derive(Clone, Copy, Debug)]
pub struct PreviewConfig {
    pub enabled: bool,
    pub width: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct PreviewOptions {
    pub width: u32,
    pub zoomed: bool,
    pub source_pos: u32,
    pub frame_len: u32,
    pub edge: PreviewEdge,
    pub capture_pos: u32,
    pub capture_len: u32,
    pub crop_enabled: bool,
    pub crop_top: u32,
    pub crop_bottom: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewEdge {
    None,
    Start,
    End,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PreviewEvents {
    pub toggle_zoom: bool,
    pub scroll_delta: f32,
    pub crop_drag: Option<PreviewCropDrag>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewCropEdge {
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug)]
pub struct PreviewCropDrag {
    pub edge: PreviewCropEdge,
    pub source_y: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Debug)]
struct OutputInfo {
    wl_output: wl_output::WlOutput,
    xdg_output: Option<zxdg_output_v1::ZxdgOutputV1>,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    has_geometry: bool,
}

#[derive(Debug)]
struct BufferState {
    buffer: wl_buffer::WlBuffer,
    mmap: MmapMut,
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
}

#[derive(Debug)]
struct OverlayState {
    surface: wl_surface::WlSurface,
    layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    buffer: Option<BufferState>,
}

#[derive(Debug)]
struct PreviewState {
    surface: wl_surface::WlSurface,
    layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    buffer: Option<BufferState>,
    width: u32,
    height: u32,
}

#[derive(Debug)]
struct PreviewLayout {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    visible: bool,
}

#[derive(Clone, Copy, Debug)]
enum PreviewSpace {
    Right(i32),
    Left(i32),
    Bottom(i32),
    Top(i32),
}

#[derive(Clone, Copy, Debug)]
struct PreviewInteraction {
    image_height: u32,
    padding: u32,
    content_width: u32,
    content_height: u32,
    viewport: PreviewViewport,
    crop_top: u32,
    crop_bottom: u32,
    crop_enabled: bool,
}

#[derive(Debug)]
struct CaptureState {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    screencopy: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    xdg_output_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    outputs: Vec<OutputInfo>,
    geometry: Rect,
    target_index: Option<usize>,
    frame: Option<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1>,
    buffer: Option<BufferState>,
    frame_done: bool,
    frame_failed: bool,
    frame_flags: u32,
    overlay: Option<OverlayState>,
    preview: Option<PreviewState>,
    preview_width: u32,
    pointer_on_preview: bool,
    pointer_x: f64,
    pointer_y: f64,
    preview_toggle_zoom: bool,
    preview_scroll_delta: f32,
    preview_crop_drag: Option<PreviewCropDrag>,
    active_crop_drag: Option<PreviewCropEdge>,
    preview_interaction: Option<PreviewInteraction>,
    accent_color: [u8; 3],
}

pub struct FrameCapturer {
    connection: Connection,
    event_queue: wayland_client::EventQueue<CaptureState>,
    state: CaptureState,
    fps: u32,
}

impl Rect {
    pub fn parse(text: &str) -> Result<Self, String> {
        let (position, size) = text
            .split_once(' ')
            .ok_or_else(|| "geometry must look like 'x,y WIDTHxHEIGHT'".to_string())?;
        let (x, y) = position
            .split_once(',')
            .ok_or_else(|| "geometry must include x,y position".to_string())?;
        let (width, height) = size
            .split_once('x')
            .ok_or_else(|| "geometry must include WIDTHxHEIGHT size".to_string())?;
        let rect = Self {
            x: x.parse().map_err(|_| "invalid geometry x".to_string())?,
            y: y.parse().map_err(|_| "invalid geometry y".to_string())?,
            width: width
                .parse()
                .map_err(|_| "invalid geometry width".to_string())?,
            height: height
                .parse()
                .map_err(|_| "invalid geometry height".to_string())?,
        };
        if rect.width <= 0 || rect.height <= 0 {
            return Err("geometry width and height must be positive".to_string());
        }
        Ok(rect)
    }
}

impl FrameCapturer {
    pub fn new(geometry: Rect, fps: u32, overlay_config: OverlayConfig) -> Result<Self, String> {
        let connection = Connection::connect_to_env()
            .map_err(|error| format!("failed to connect to Wayland display: {error}"))?;
        let mut event_queue = connection.new_event_queue();
        let qh = event_queue.handle();
        let display = connection.display();

        let mut state = CaptureState {
            compositor: None,
            shm: None,
            screencopy: None,
            layer_shell: None,
            xdg_output_manager: None,
            seat: None,
            pointer: None,
            outputs: Vec::new(),
            geometry,
            target_index: None,
            frame: None,
            buffer: None,
            frame_done: false,
            frame_failed: false,
            frame_flags: 0,
            overlay: None,
            preview: None,
            preview_width: overlay_config.preview.width,
            pointer_on_preview: false,
            pointer_x: 0.0,
            pointer_y: 0.0,
            preview_toggle_zoom: false,
            preview_scroll_delta: 0.0,
            preview_crop_drag: None,
            active_crop_drag: None,
            preview_interaction: None,
            accent_color: overlay_config.color.unwrap_or_else(resolve_accent_color),
        };

        display.get_registry(&qh, ());
        event_queue
            .roundtrip(&mut state)
            .map_err(|error| format!("failed to read Wayland globals: {error}"))?;

        let manager = state
            .xdg_output_manager
            .clone()
            .ok_or_else(|| "required Wayland protocol is missing: xdg-output".to_string())?;
        for index in 0..state.outputs.len() {
            let xdg_output = manager.get_xdg_output(&state.outputs[index].wl_output, &qh, index);
            state.outputs[index].xdg_output = Some(xdg_output);
        }
        event_queue
            .roundtrip(&mut state)
            .map_err(|error| format!("failed to read Wayland output geometry: {error}"))?;

        if state.shm.is_none() {
            return Err("required Wayland protocol is missing: wl_shm".to_string());
        }
        if state.screencopy.is_none() {
            return Err("required Wayland protocol is missing: wlr-screencopy".to_string());
        }
        state.target_index = state.find_target_output();
        if state.target_index.is_none() {
            return Err("failed to resolve target output".to_string());
        }
        if overlay_config.enabled {
            state.init_overlay(&qh)?;
        }
        if overlay_config.preview.enabled {
            state.init_preview(&qh)?;
        }
        if overlay_config.enabled || overlay_config.preview.enabled {
            event_queue
                .roundtrip(&mut state)
                .map_err(|error| format!("failed to show capture overlays: {error}"))?;
            event_queue
                .flush()
                .map_err(|error| format!("failed to flush capture overlays: {error}"))?;
        }

        Ok(Self {
            connection,
            event_queue,
            state,
            fps,
        })
    }

    pub fn capture_frame_interruptible<F, S>(
        &mut self,
        mut should_stop: F,
        stop_wake: Option<S>,
    ) -> Result<Option<Image>, String>
    where
        F: FnMut() -> bool,
        S: AsFd,
    {
        let _ = self.connection.flush();
        let qh = self.event_queue.handle();
        self.state.request_frame(&qh)?;
        while !self.state.frame_done {
            if should_stop() {
                self.state.frame = None;
                return Ok(None);
            }
            let dispatched = self
                .event_queue
                .dispatch_pending(&mut self.state)
                .map_err(|error| format!("failed to dispatch Wayland events: {error}"))?;
            if dispatched > 0 || self.state.frame_done {
                continue;
            }
            self.event_queue
                .flush()
                .map_err(|error| format!("failed to flush Wayland requests: {error}"))?;
            let Some(guard) = self.event_queue.prepare_read() else {
                continue;
            };
            let wayland_fd = guard.connection_fd();
            let stop_fd = stop_wake.as_ref().map(|stream| stream.as_fd());
            let mut fds = if let Some(stop_fd) = stop_fd {
                vec![
                    PollFd::from_borrowed_fd(wayland_fd, PollFlags::IN | PollFlags::ERR),
                    PollFd::from_borrowed_fd(stop_fd, PollFlags::IN | PollFlags::ERR),
                ]
            } else {
                vec![PollFd::from_borrowed_fd(
                    wayland_fd,
                    PollFlags::IN | PollFlags::ERR,
                )]
            };
            poll(&mut fds, None)
                .map_err(|error| format!("failed to poll Wayland socket: {error}"))?;
            let stop_ready = fds
                .get(1)
                .is_some_and(|fd| fd.revents().intersects(PollFlags::IN | PollFlags::ERR));
            if stop_ready || should_stop() {
                drop(guard);
                self.state.frame = None;
                return Ok(None);
            }
            guard
                .read()
                .map_err(|error| format!("failed to read Wayland events: {error}"))?;
            self.event_queue
                .dispatch_pending(&mut self.state)
                .map_err(|error| format!("failed to dispatch Wayland events: {error}"))?;
        }
        if self.state.frame_failed {
            return Ok(None);
        }
        let image = self.state.take_image()?;
        Ok(Some(image))
    }

    pub fn sleep_frame_interval<F>(&self, mut should_stop: F)
    where
        F: FnMut() -> bool,
    {
        if self.fps == 0 {
            return;
        }
        let total = Duration::from_nanos(1_000_000_000 / self.fps as u64);
        let step = Duration::from_millis(5);
        let mut slept = Duration::ZERO;
        while slept < total && !should_stop() {
            let remaining = total.saturating_sub(slept);
            let delay = remaining.min(step);
            thread::sleep(delay);
            slept += delay;
        }
    }

    pub fn update_preview(&mut self, image: &Image, options: PreviewOptions) -> Result<(), String> {
        self.state.preview_width = options.width;
        self.state
            .draw_preview(&self.event_queue.handle(), image, options)?;
        self.event_queue
            .dispatch_pending(&mut self.state)
            .map_err(|error| format!("failed to dispatch Wayland events: {error}"))?;
        self.event_queue
            .flush()
            .map_err(|error| format!("failed to flush preview overlay: {error}"))
    }

    pub fn take_preview_events(&mut self) -> Result<PreviewEvents, String> {
        self.pump_pending_events()?;
        Ok(PreviewEvents {
            toggle_zoom: std::mem::take(&mut self.state.preview_toggle_zoom),
            scroll_delta: std::mem::take(&mut self.state.preview_scroll_delta),
            crop_drag: self.state.preview_crop_drag.take(),
        })
    }

    fn pump_pending_events(&mut self) -> Result<(), String> {
        loop {
            let dispatched = self
                .event_queue
                .dispatch_pending(&mut self.state)
                .map_err(|error| format!("failed to dispatch Wayland events: {error}"))?;
            if dispatched > 0 {
                continue;
            }
            self.event_queue
                .flush()
                .map_err(|error| format!("failed to flush Wayland requests: {error}"))?;
            let Some(guard) = self.event_queue.prepare_read() else {
                continue;
            };
            let wayland_fd = guard.connection_fd();
            let mut fds = [PollFd::from_borrowed_fd(
                wayland_fd,
                PollFlags::IN | PollFlags::ERR,
            )];
            let timeout = Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            poll(&mut fds, Some(&timeout))
                .map_err(|error| format!("failed to poll Wayland socket: {error}"))?;
            let ready = fds[0].revents().intersects(PollFlags::IN | PollFlags::ERR);
            if !ready {
                drop(guard);
                break;
            }
            guard
                .read()
                .map_err(|error| format!("failed to read Wayland events: {error}"))?;
        }
        self.event_queue
            .dispatch_pending(&mut self.state)
            .map_err(|error| format!("failed to dispatch Wayland events: {error}"))?;
        Ok(())
    }
}

impl CaptureState {
    fn crop_edge_at_pointer(&self) -> Option<PreviewCropEdge> {
        let interaction = self.preview_interaction?;
        if !interaction.crop_enabled {
            return None;
        }
        let x = self.pointer_x.round() as i32;
        let y = self.pointer_y.round() as i32;
        let content_x = interaction.padding as i32;
        let content_y = interaction.padding as i32;
        if x < content_x || x >= content_x + interaction.content_width as i32 {
            return None;
        }
        let handle_radius = 8;
        if let Some(top_y) = crop_source_to_pointer_y(interaction, interaction.crop_top) {
            if (y - (content_y + top_y as i32)).abs() <= handle_radius {
                return Some(PreviewCropEdge::Top);
            }
        }
        let bottom_source =
            bottom_source_from_crop(interaction.crop_top, interaction.crop_bottom, interaction);
        if let Some(bottom_y) = crop_source_to_pointer_y(interaction, bottom_source) {
            if (y - (content_y + bottom_y as i32)).abs() <= handle_radius {
                return Some(PreviewCropEdge::Bottom);
            }
        }
        None
    }

    fn update_crop_drag(&mut self) {
        let Some(edge) = self.active_crop_drag else {
            return;
        };
        let Some(interaction) = self.preview_interaction else {
            return;
        };
        if !interaction.crop_enabled {
            return;
        }
        let relative_y = (self.pointer_y - interaction.padding as f64)
            .round()
            .clamp(0.0, interaction.content_height.saturating_sub(1) as f64)
            as u32;
        let source_y = interaction.viewport.source_start.saturating_add(
            ((relative_y as u64 * interaction.viewport.source_len as u64)
                / interaction.content_height.max(1) as u64) as u32,
        );
        self.preview_crop_drag = Some(PreviewCropDrag { edge, source_y });
    }

    fn find_target_output(&self) -> Option<usize> {
        let center_x = self.geometry.x + self.geometry.width / 2;
        let center_y = self.geometry.y + self.geometry.height / 2;
        self.outputs
            .iter()
            .position(|output| {
                output.has_geometry
                    && center_x >= output.x
                    && center_x < output.x + output.width
                    && center_y >= output.y
                    && center_y < output.y + output.height
            })
            .or_else(|| (!self.outputs.is_empty()).then_some(0))
    }

    fn request_frame(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        self.frame_done = false;
        self.frame_failed = false;
        self.frame_flags = 0;
        self.buffer = None;
        self.frame = None;

        let manager = self
            .screencopy
            .as_ref()
            .ok_or_else(|| "wlr-screencopy is unavailable".to_string())?;
        let target = self
            .target_index
            .and_then(|index| self.outputs.get(index))
            .ok_or_else(|| "target output is unavailable".to_string())?;
        let local_x = self.geometry.x - target.x;
        let local_y = self.geometry.y - target.y;
        let frame = manager.capture_output_region(
            0,
            &target.wl_output,
            local_x,
            local_y,
            self.geometry.width,
            self.geometry.height,
            qh,
            (),
        );
        self.frame = Some(frame);
        Ok(())
    }

    fn init_overlay(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        let Some(compositor) = &self.compositor else {
            return Ok(());
        };
        let Some(layer_shell) = &self.layer_shell else {
            return Ok(());
        };
        let target = self
            .target_index
            .and_then(|index| self.outputs.get(index))
            .ok_or_else(|| "target output is unavailable".to_string())?;
        if !target.has_geometry || target.width <= 0 || target.height <= 0 {
            return Ok(());
        }

        let surface = compositor.create_surface(qh, ());
        let empty_region = compositor.create_region(qh, ());
        surface.set_input_region(Some(&empty_region));
        empty_region.destroy();
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            Some(&target.wl_output),
            zwlr_layer_shell_v1::Layer::Overlay,
            APP_ID.to_string(),
            qh,
            (),
        );
        layer_surface.set_size(target.width as u32, target.height as u32);
        layer_surface.set_anchor(
            zwlr_layer_surface_v1::Anchor::Top
                | zwlr_layer_surface_v1::Anchor::Bottom
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right,
        );
        layer_surface.set_exclusive_zone(-1);
        layer_surface
            .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
        surface.commit();

        self.overlay = Some(OverlayState {
            surface,
            layer_surface,
            buffer: None,
        });
        Ok(())
    }

    fn init_preview(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        let Some(compositor) = &self.compositor else {
            return Ok(());
        };
        let Some(layer_shell) = &self.layer_shell else {
            return Ok(());
        };
        let target = self
            .target_index
            .and_then(|index| self.outputs.get(index))
            .ok_or_else(|| "target output is unavailable".to_string())?;
        if !target.has_geometry || target.width <= 0 || target.height <= 0 {
            return Ok(());
        }

        let layout = self.preview_layout(
            target,
            self.preview_width,
            self.preview_width,
            PreviewOptions {
                width: self.preview_width,
                zoomed: false,
                source_pos: 0,
                frame_len: 0,
                edge: PreviewEdge::None,
                capture_pos: 0,
                capture_len: 0,
                crop_enabled: false,
                crop_top: 0,
                crop_bottom: 0,
            },
        );
        let surface = compositor.create_surface(qh, ());
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            Some(&target.wl_output),
            zwlr_layer_shell_v1::Layer::Overlay,
            APP_ID.to_string(),
            qh,
            (),
        );
        layer_surface.set_size(layout.width, layout.height);
        layer_surface
            .set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left);
        layer_surface.set_margin(layout.y, 0, 0, layout.x);
        layer_surface.set_exclusive_zone(-1);
        layer_surface
            .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
        surface.commit();

        self.preview = Some(PreviewState {
            surface,
            layer_surface,
            buffer: None,
            width: layout.width,
            height: layout.height,
        });
        Ok(())
    }

    fn draw_overlay(
        &mut self,
        qh: &QueueHandle<Self>,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let target = self
            .target_index
            .and_then(|index| self.outputs.get(index))
            .ok_or_else(|| "target output is unavailable".to_string())?;
        let local_x = (self.geometry.x - target.x).max(0) as u32;
        let local_y = (self.geometry.y - target.y).max(0) as u32;
        let rect_width = self.geometry.width.max(1) as u32;
        let rect_height = self.geometry.height.max(1) as u32;

        let mut buffer = self.create_argb_buffer(qh, width, height)?;
        draw_box(
            &mut buffer.mmap,
            width,
            height,
            buffer.stride as usize,
            local_x,
            local_y,
            rect_width,
            rect_height,
            self.accent_color,
        );
        let overlay = self
            .overlay
            .as_mut()
            .ok_or_else(|| "overlay is unavailable".to_string())?;
        let _keep_layer_surface_alive = &overlay.layer_surface;
        overlay.surface.attach(Some(&buffer.buffer), 0, 0);
        overlay
            .surface
            .damage_buffer(0, 0, width as i32, height as i32);
        overlay.surface.commit();
        overlay.buffer = Some(buffer);
        Ok(())
    }

    fn draw_preview(
        &mut self,
        qh: &QueueHandle<Self>,
        image: &Image,
        options: PreviewOptions,
    ) -> Result<(), String> {
        if self.preview.is_none() {
            return Ok(());
        }
        let target = self
            .target_index
            .and_then(|index| self.outputs.get(index))
            .ok_or_else(|| "target output is unavailable".to_string())?;
        let layout = self.preview_layout(target, image.width(), image.height(), options);
        if !layout.visible {
            if let Some(preview) = self.preview.as_mut() {
                preview.surface.attach(None, 0, 0);
                preview.surface.commit();
                preview.buffer = None;
            }
            self.preview_interaction = None;
            return Ok(());
        }

        {
            let preview = self.preview.as_mut().expect("preview exists");
            if preview.width != layout.width || preview.height != layout.height {
                preview.width = layout.width;
                preview.height = layout.height;
                preview.layer_surface.set_size(layout.width, layout.height);
            }
            preview.layer_surface.set_margin(layout.y, 0, 0, layout.x);
            let _keep_layer_surface_alive = &preview.layer_surface;
        }

        let mut buffer = self.create_argb_buffer(qh, layout.width, layout.height)?;
        draw_preview_image(
            &mut buffer.mmap,
            layout.width,
            layout.height,
            buffer.stride as usize,
            image,
            self.accent_color,
            options.source_pos,
            options.frame_len,
            options.edge,
            options.capture_pos,
            options.capture_len,
            options.crop_enabled,
            options.crop_top,
            options.crop_bottom,
        );
        self.preview_interaction = Some(preview_interaction(
            image,
            layout.width,
            layout.height,
            options,
        ));
        let preview = self.preview.as_mut().expect("preview exists");
        preview.surface.attach(Some(&buffer.buffer), 0, 0);
        preview
            .surface
            .damage_buffer(0, 0, layout.width as i32, layout.height as i32);
        preview.surface.commit();
        preview.buffer = Some(buffer);
        Ok(())
    }

    fn preview_layout(
        &self,
        target: &OutputInfo,
        image_width: u32,
        image_height: u32,
        options: PreviewOptions,
    ) -> PreviewLayout {
        let gap = 10;
        let padding = 8;
        let desired_content_width = if options.zoomed {
            let output_limit = ((target.width.max(1) as u32) * 45 / 100).clamp(240, 720);
            options.width.max(output_limit).min(output_limit)
        } else {
            options.width.max(1)
        };
        let local_x = self.geometry.x - target.x;
        let local_y = self.geometry.y - target.y;
        let rect_right = local_x + self.geometry.width;
        let rect_bottom = local_y + self.geometry.height;
        let target_width = target.width.max(1);
        let target_height = target.height.max(1);
        let min_content_width = 80;
        let spaces = [
            PreviewSpace::Right(target_width.saturating_sub(rect_right).saturating_sub(gap)),
            PreviewSpace::Left(local_x.saturating_sub(gap)),
            PreviewSpace::Bottom(
                target_height
                    .saturating_sub(rect_bottom)
                    .saturating_sub(gap),
            ),
            PreviewSpace::Top(local_y.saturating_sub(gap)),
        ];

        for space in spaces {
            let available_width = match space {
                PreviewSpace::Right(value) | PreviewSpace::Left(value) => value.max(0) as u32,
                PreviewSpace::Bottom(_) | PreviewSpace::Top(_) => target_width as u32,
            };
            let available_height = match space {
                PreviewSpace::Right(_) | PreviewSpace::Left(_) => {
                    self.geometry.height.max(1) as u32
                }
                PreviewSpace::Bottom(value) | PreviewSpace::Top(value) => value.max(0) as u32,
            };
            let max_content_width = available_width.saturating_sub(padding * 2);
            let content_width = desired_content_width.min(max_content_width);
            if content_width < min_content_width {
                continue;
            }
            let scaled_height = scale_height_for_width(image_width, image_height, content_width);
            let max_content_height = available_height.saturating_sub(padding * 2);
            if max_content_height < 32 {
                continue;
            }
            let content_height = scaled_height.min(max_content_height).max(1);
            let width = content_width + padding * 2;
            let height = content_height + padding * 2;
            let (x, y) = match space {
                PreviewSpace::Right(_) => (
                    rect_right + gap,
                    local_y.clamp(0, target_height - height as i32),
                ),
                PreviewSpace::Left(_) => (
                    local_x - width as i32 - gap,
                    local_y.clamp(0, target_height - height as i32),
                ),
                PreviewSpace::Bottom(_) => (
                    local_x.clamp(0, target_width - width as i32),
                    rect_bottom + gap,
                ),
                PreviewSpace::Top(_) => (
                    local_x.clamp(0, target_width - width as i32),
                    local_y - height as i32 - gap,
                ),
            };
            return PreviewLayout {
                x,
                y,
                width,
                height,
                visible: true,
            };
        }

        PreviewLayout {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            visible: false,
        }
    }

    fn create_argb_buffer(
        &self,
        qh: &QueueHandle<Self>,
        width: u32,
        height: u32,
    ) -> Result<BufferState, String> {
        let shm = self
            .shm
            .as_ref()
            .ok_or_else(|| "wl_shm is unavailable".to_string())?;
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| "overlay stride overflow".to_string())?;
        let size = stride
            .checked_mul(height)
            .ok_or_else(|| "overlay buffer size overflow".to_string())?;
        let file = tempfile().map_err(|error| format!("failed to create shm file: {error}"))?;
        file.set_len(size as u64)
            .map_err(|error| format!("failed to resize shm file: {error}"))?;
        let mmap = unsafe { MmapOptions::new().len(size as usize).map_mut(&file) }
            .map_err(|error| format!("failed to mmap shm file: {error}"))?;
        let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        pool.destroy();
        Ok(BufferState {
            buffer,
            mmap,
            width,
            height,
            stride,
            format: wl_shm::Format::Argb8888 as u32,
        })
    }

    fn create_buffer(
        &mut self,
        qh: &QueueHandle<Self>,
        format: WEnum<wl_shm::Format>,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<(), String> {
        let shm = self
            .shm
            .as_ref()
            .ok_or_else(|| "wl_shm is unavailable".to_string())?;
        let size = stride
            .checked_mul(height)
            .ok_or_else(|| "capture buffer size overflow".to_string())?;
        let file = tempfile().map_err(|error| format!("failed to create shm file: {error}"))?;
        file.set_len(size as u64)
            .map_err(|error| format!("failed to resize shm file: {error}"))?;
        let mmap = unsafe { MmapOptions::new().len(size as usize).map_mut(&file) }
            .map_err(|error| format!("failed to mmap shm file: {error}"))?;
        let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
        let wl_format = match format {
            WEnum::Value(value) => value,
            WEnum::Unknown(value) => {
                return Err(format!("unsupported wl_shm format: 0x{value:08x}"));
            }
        };
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            wl_format,
            qh,
            (),
        );
        pool.destroy();
        self.buffer = Some(BufferState {
            buffer,
            mmap,
            width,
            height,
            stride,
            format: wl_format as u32,
        });
        Ok(())
    }

    fn take_image(&mut self) -> Result<Image, String> {
        let buffer = self
            .buffer
            .take()
            .ok_or_else(|| "capture finished without a buffer".to_string())?;
        decode_wl_shm_frame(
            &buffer.mmap,
            buffer.width,
            buffer.height,
            buffer.stride as usize,
            buffer.format,
            self.frame_flags,
        )
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for CaptureState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_shm" => state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ())),
                "wl_compositor" => {
                    state.compositor = Some(registry.bind::<wl_compositor::WlCompositor, _, _>(
                        name,
                        version.min(4),
                        qh,
                        (),
                    ))
                }
                "wl_output" => {
                    let output =
                        registry.bind::<wl_output::WlOutput, _, _>(name, version.min(2), qh, ());
                    state.outputs.push(OutputInfo {
                        wl_output: output,
                        xdg_output: None,
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0,
                        has_geometry: false,
                    });
                }
                "zwlr_screencopy_manager_v1" => {
                    state.screencopy = Some(
                        registry.bind::<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, _, _>(
                            name,
                            version.min(3),
                            qh,
                            (),
                        ),
                    )
                }
                "zwlr_layer_shell_v1" => {
                    state.layer_shell = Some(
                        registry.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1, _, _>(
                            name,
                            version.min(4),
                            qh,
                            (),
                        ),
                    )
                }
                "zxdg_output_manager_v1" => {
                    state.xdg_output_manager = Some(
                        registry.bind::<zxdg_output_manager_v1::ZxdgOutputManagerV1, _, _>(
                            name,
                            version.min(3),
                            qh,
                            (),
                        ),
                    )
                }
                "wl_seat" => {
                    state.seat =
                        Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(8), qh, ()))
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for CaptureState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let has_pointer = match capabilities {
                WEnum::Value(value) => value.contains(wl_seat::Capability::Pointer),
                WEnum::Unknown(_) => false,
            };
            if has_pointer && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()))
            } else if !has_pointer {
                state.pointer = None;
                state.pointer_on_preview = false;
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_on_preview = state
                    .preview
                    .as_ref()
                    .is_some_and(|preview| preview.surface == surface);
                state.pointer_x = surface_x;
                state.pointer_y = surface_y;
            }
            wl_pointer::Event::Leave { surface, .. } => {
                if state
                    .preview
                    .as_ref()
                    .is_some_and(|preview| preview.surface == surface)
                {
                    state.pointer_on_preview = false;
                    state.active_crop_drag = None;
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_x = surface_x;
                state.pointer_y = surface_y;
                if state.pointer_on_preview {
                    state.update_crop_drag();
                }
            }
            wl_pointer::Event::Button {
                button,
                state: button_state,
                ..
            } => {
                let pressed =
                    matches!(button_state, WEnum::Value(wl_pointer::ButtonState::Pressed));
                if state.pointer_on_preview && button == 0x110 {
                    if pressed {
                        state.active_crop_drag = state.crop_edge_at_pointer();
                        if state.active_crop_drag.is_some() {
                            state.update_crop_drag();
                        } else {
                            state.preview_toggle_zoom = true;
                        }
                    } else {
                        state.active_crop_drag = None;
                    }
                }
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let vertical = matches!(axis, WEnum::Value(wl_pointer::Axis::VerticalScroll));
                if state.pointer_on_preview && vertical {
                    state.preview_scroll_delta += value as f32;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &zxdg_output_manager_v1::ZxdgOutputManagerV1,
        _: zxdg_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, usize> for CaptureState {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(*index) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                output.x = x;
                output.y = y;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output.width = width;
                output.height = height;
                output.has_geometry = width > 0 && height > 0;
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
        _: zwlr_screencopy_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _: zwlr_layer_shell_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                surface.ack_configure(serial);
                let is_border = state
                    .overlay
                    .as_ref()
                    .is_some_and(|overlay| &overlay.layer_surface == surface);
                if is_border && width > 0 && height > 0 {
                    let _ = state.draw_overlay(qh, width, height);
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                if state
                    .overlay
                    .as_ref()
                    .is_some_and(|overlay| &overlay.layer_surface == surface)
                {
                    state.overlay = None;
                }
                if state
                    .preview
                    .as_ref()
                    .is_some_and(|preview| &preview.layer_surface == surface)
                {
                    state.preview = None;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                if state
                    .create_buffer(qh, format, width, height, stride)
                    .is_err()
                {
                    state.frame_failed = true;
                    state.frame_done = true;
                }
            }
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                state.frame_flags = match flags {
                    WEnum::Value(value) => value.bits(),
                    WEnum::Unknown(value) => value,
                };
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.frame_done = true;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.frame_failed = true;
                state.frame_done = true;
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                if let Some(buffer) = &state.buffer {
                    frame.copy(&buffer.buffer);
                } else {
                    state.frame_failed = true;
                    state.frame_done = true;
                }
            }
            _ => {}
        }
    }
}

delegate_noop!(CaptureState: ignore wl_shm::WlShm);
delegate_noop!(CaptureState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(CaptureState: ignore wl_buffer::WlBuffer);
delegate_noop!(CaptureState: ignore wl_output::WlOutput);
delegate_noop!(CaptureState: ignore wl_compositor::WlCompositor);
delegate_noop!(CaptureState: ignore wl_surface::WlSurface);
delegate_noop!(CaptureState: ignore wl_region::WlRegion);

fn decode_wl_shm_frame(
    payload: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    format: u32,
    flags: u32,
) -> Result<Image, String> {
    const WL_SHM_FORMAT_ARGB8888: u32 = 0;
    const WL_SHM_FORMAT_XRGB8888: u32 = 1;
    const WL_SHM_FORMAT_ABGR8888: u32 = 0x3432_4241;
    const WL_SHM_FORMAT_XBGR8888: u32 = 0x3432_4258;

    let mut image = Image::new(width, height);
    for y in 0..height {
        let source_y = if flags & 1 != 0 { height - 1 - y } else { y };
        let row_start = source_y as usize * stride;
        for x in 0..width {
            let offset = row_start + x as usize * 4;
            let pixel = &payload[offset..offset + 4];
            let rgba = match format {
                WL_SHM_FORMAT_ARGB8888 | WL_SHM_FORMAT_XRGB8888 => {
                    [pixel[2], pixel[1], pixel[0], 255]
                }
                WL_SHM_FORMAT_ABGR8888 | WL_SHM_FORMAT_XBGR8888 => {
                    [pixel[0], pixel[1], pixel[2], 255]
                }
                _ => return Err(format!("unsupported wl_shm format: 0x{format:08x}")),
            };
            image.put_pixel(x, y, Rgba(rgba));
        }
    }
    Ok(image)
}

fn resolve_accent_color() -> [u8; 3] {
    if let Ok(value) = env::var("WL_LONGSHOT_OVERLAY_COLOR") {
        if let Some(color) = parse_color(&value) {
            return color;
        }
    }

    let mut paths = Vec::new();
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
        let config_home = PathBuf::from(config_home);
        paths.push(config_home.join("gtk-4.0/gtk.css"));
        paths.push(config_home.join("gtk-3.0/gtk.css"));
        paths.push(config_home.join("waybar/colors.css"));
        paths.push(config_home.join("swayosd/colors.css"));
    } else if let Some(home) = env::var_os("HOME") {
        let config_home = PathBuf::from(home).join(".config");
        paths.push(config_home.join("gtk-4.0/gtk.css"));
        paths.push(config_home.join("gtk-3.0/gtk.css"));
        paths.push(config_home.join("waybar/colors.css"));
        paths.push(config_home.join("swayosd/colors.css"));
    }

    let names = ["accent_color", "theme_selected_bg_color", "primary"];
    for path in paths {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        for name in names {
            if let Some(color) = find_css_color(&content, name) {
                return color;
            }
        }
    }

    [255, 255, 255]
}

fn find_css_color(content: &str, name: &str) -> Option<[u8; 3]> {
    for line in content.lines() {
        let line = line.trim();
        if !line.contains(name) {
            continue;
        }
        if line.starts_with("@define-color") {
            let mut parts = line.trim_end_matches(';').split_whitespace();
            let _directive = parts.next();
            let css_name = parts.next();
            let value = parts.next();
            if css_name == Some(name) {
                if let Some(value) = value {
                    return parse_color(value);
                }
            }
        }
        if let Some((_, value)) = line.split_once(':') {
            return parse_color(value.trim().trim_end_matches(';'));
        }
    }
    None
}

pub fn parse_color(value: &str) -> Option<[u8; 3]> {
    let value = value.trim().trim_matches('"').trim_matches('\'');
    if let Some(hex) = value.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    if let Some(inner) = value
        .strip_prefix("rgb(")
        .and_then(|text| text.strip_suffix(')'))
    {
        let mut channels = inner.split(',').map(|part| part.trim().parse::<u8>().ok());
        return Some([channels.next()??, channels.next()??, channels.next()??]);
    }
    None
}

fn parse_hex_color(hex: &str) -> Option<[u8; 3]> {
    match hex.len() {
        6 | 8 => Some([
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
        ]),
        3 | 4 => {
            let mut chars = hex.chars();
            let r = chars.next()?;
            let g = chars.next()?;
            let b = chars.next()?;
            Some([
                u8::from_str_radix(&format!("{r}{r}"), 16).ok()?,
                u8::from_str_radix(&format!("{g}{g}"), 16).ok()?,
                u8::from_str_radix(&format!("{b}{b}"), 16).ok()?,
            ])
        }
        _ => None,
    }
}

fn scale_height_for_width(width: u32, height: u32, target_width: u32) -> u32 {
    if width == 0 || height == 0 {
        return 1;
    }
    ((height as u64 * target_width.max(1) as u64) / width as u64).max(1) as u32
}

fn draw_preview_image(
    buffer: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    image: &Image,
    accent: [u8; 3],
    source_pos: u32,
    frame_len: u32,
    edge: PreviewEdge,
    capture_pos: u32,
    capture_len: u32,
    crop_enabled: bool,
    crop_top: u32,
    crop_bottom: u32,
) {
    buffer.fill(0);
    fill_rect(
        buffer,
        stride,
        width,
        height,
        1,
        1,
        width.saturating_sub(2),
        height.saturating_sub(2),
        [accent[0], accent[1], accent[2], 28],
    );

    let padding = 8;
    let content_width = width.saturating_sub(padding * 2).max(1);
    let content_height = height.saturating_sub(padding * 2).max(1);
    let viewport = preview_viewport(
        image,
        content_width,
        content_height,
        source_pos,
        frame_len,
        edge,
    );
    blit_latest_viewport(
        buffer,
        stride,
        width,
        height,
        image,
        padding,
        padding,
        content_width,
        content_height,
        viewport,
    );
    draw_preview_position_indicator(
        buffer,
        stride,
        width,
        height,
        image,
        padding,
        padding,
        content_width,
        content_height,
        viewport,
        capture_pos,
        capture_len,
        accent,
    );
    if crop_enabled {
        draw_crop_overlay(
            buffer,
            stride,
            width,
            height,
            image,
            padding,
            padding,
            content_width,
            content_height,
            viewport,
            crop_top,
            crop_bottom,
            accent,
        );
    }

    stroke_rect(
        buffer,
        stride,
        width,
        height,
        1.0,
        1.0,
        width.saturating_sub(3) as f32,
        height.saturating_sub(3) as f32,
        2,
        [accent[0], accent[1], accent[2], 77],
    );
}

fn preview_interaction(
    image: &Image,
    width: u32,
    height: u32,
    options: PreviewOptions,
) -> PreviewInteraction {
    let padding = 8;
    let content_width = width.saturating_sub(padding * 2).max(1);
    let content_height = height.saturating_sub(padding * 2).max(1);
    let viewport = preview_viewport(
        image,
        content_width,
        content_height,
        options.source_pos,
        options.frame_len,
        options.edge,
    );
    PreviewInteraction {
        image_height: image.height(),
        padding,
        content_width,
        content_height,
        viewport,
        crop_top: options.crop_top,
        crop_bottom: options.crop_bottom,
        crop_enabled: options.crop_enabled,
    }
}

fn bottom_source_from_crop(
    crop_top: u32,
    crop_bottom: u32,
    interaction: PreviewInteraction,
) -> u32 {
    interaction
        .image_height
        .saturating_sub(crop_bottom)
        .max(crop_top.saturating_add(1))
}

fn crop_source_to_pointer_y(interaction: PreviewInteraction, source_y: u32) -> Option<u32> {
    let start = interaction.viewport.source_start;
    let end = start.saturating_add(interaction.viewport.source_len);
    if source_y < start || source_y > end {
        return None;
    }
    Some(source_to_draw_y(
        source_y,
        start,
        interaction.viewport.source_len,
        interaction.content_height,
    ))
}

fn draw_crop_overlay(
    buffer: &mut [u8],
    stride: usize,
    buffer_width: u32,
    buffer_height: u32,
    image: &Image,
    x: u32,
    y: u32,
    draw_width: u32,
    draw_height: u32,
    viewport: PreviewViewport,
    crop_top: u32,
    crop_bottom: u32,
    accent: [u8; 3],
) {
    if image.height() == 0 || draw_width == 0 || draw_height == 0 {
        return;
    }
    let max_crop_top = image.height().saturating_sub(1);
    let crop_top = crop_top.min(max_crop_top);
    let crop_bottom = crop_bottom.min(image.height().saturating_sub(crop_top + 1));
    let bottom_source = image.height().saturating_sub(crop_bottom);

    if crop_top > viewport.source_start {
        let hidden_end = crop_top.min(viewport.source_start.saturating_add(viewport.source_len));
        if hidden_end > viewport.source_start {
            let hidden_height = source_span_to_draw_len(
                viewport.source_start,
                hidden_end,
                viewport.source_start,
                viewport.source_len,
                draw_height,
            );
            fill_rect(
                buffer,
                stride,
                buffer_width,
                buffer_height,
                x,
                y,
                draw_width,
                hidden_height,
                [0, 0, 0, 140],
            );
        }
    }
    if bottom_source < viewport.source_start.saturating_add(viewport.source_len) {
        let hidden_start = bottom_source.max(viewport.source_start);
        if hidden_start < viewport.source_start.saturating_add(viewport.source_len) {
            let line_y = source_to_draw_y(
                hidden_start,
                viewport.source_start,
                viewport.source_len,
                draw_height,
            );
            fill_rect(
                buffer,
                stride,
                buffer_width,
                buffer_height,
                x,
                y + line_y,
                draw_width,
                draw_height.saturating_sub(line_y),
                [0, 0, 0, 140],
            );
        }
    }

    if (viewport.source_start..=viewport.source_start.saturating_add(viewport.source_len))
        .contains(&crop_top)
    {
        draw_crop_handle(
            buffer,
            stride,
            buffer_width,
            buffer_height,
            x,
            y + source_to_draw_y(
                crop_top,
                viewport.source_start,
                viewport.source_len,
                draw_height,
            ),
            draw_width,
            accent,
        );
    }
    if (viewport.source_start..=viewport.source_start.saturating_add(viewport.source_len))
        .contains(&bottom_source)
    {
        draw_crop_handle(
            buffer,
            stride,
            buffer_width,
            buffer_height,
            x,
            y + source_to_draw_y(
                bottom_source,
                viewport.source_start,
                viewport.source_len,
                draw_height,
            ),
            draw_width,
            accent,
        );
    }
}

fn draw_crop_handle(
    buffer: &mut [u8],
    stride: usize,
    buffer_width: u32,
    buffer_height: u32,
    x: u32,
    y: u32,
    width: u32,
    accent: [u8; 3],
) {
    const HANDLE_BG: [u8; 4] = [18, 18, 18, 220];
    const HANDLE_HIGHLIGHT: [u8; 4] = [255, 255, 255, 210];
    let crop_line = [accent[0], accent[1], accent[2], 180];

    let line_y = y.saturating_sub(1);
    fill_rect(
        buffer,
        stride,
        buffer_width,
        buffer_height,
        x,
        line_y,
        width,
        3,
        crop_line,
    );

    let handle_width = width.clamp(44, 76);
    let handle_height = 13;
    let handle_x = x + width.saturating_sub(handle_width) / 2;
    let handle_y = y.saturating_sub(handle_height / 2);
    fill_rounded_rect(
        buffer,
        stride,
        buffer_width,
        buffer_height,
        handle_x,
        handle_y,
        handle_width,
        handle_height,
        handle_height / 2,
        HANDLE_BG,
    );
    stroke_rounded_rect(
        buffer,
        stride,
        buffer_width,
        buffer_height,
        handle_x,
        handle_y,
        handle_width,
        handle_height,
        2,
        handle_height / 2,
        crop_line,
    );

    let grip_x = handle_x + handle_width / 2;
    for offset in [-8_i32, 0, 8] {
        fill_rect(
            buffer,
            stride,
            buffer_width,
            buffer_height,
            (grip_x as i32 + offset).max(0) as u32,
            handle_y + 4,
            3,
            5,
            HANDLE_HIGHLIGHT,
        );
    }

    fill_rect(
        buffer,
        stride,
        buffer_width,
        buffer_height,
        x,
        line_y,
        width,
        1,
        [accent[0], accent[1], accent[2], 110],
    );
}

fn fill_rounded_rect(
    buffer: &mut [u8],
    stride: usize,
    buffer_width: u32,
    buffer_height: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    radius: u32,
    rgba: [u8; 4],
) {
    if width == 0 || height == 0 {
        return;
    }
    let radius = radius.min(width / 2).min(height / 2);
    let right = x.saturating_add(width);
    let bottom = y.saturating_add(height);
    for py in y..bottom.min(buffer_height) {
        for px in x..right.min(buffer_width) {
            if rounded_rect_contains(px - x, py - y, width, height, radius) {
                put_argb_pixel(buffer, stride, px, py, rgba);
            }
        }
    }
}

fn stroke_rounded_rect(
    buffer: &mut [u8],
    stride: usize,
    buffer_width: u32,
    buffer_height: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    line_width: u32,
    radius: u32,
    rgba: [u8; 4],
) {
    for inset in 0..line_width.max(1) {
        let inset_width = width.saturating_sub(inset * 2);
        let inset_height = height.saturating_sub(inset * 2);
        if inset_width == 0 || inset_height == 0 {
            break;
        }
        draw_rounded_rect_outline(
            buffer,
            stride,
            buffer_width,
            buffer_height,
            x + inset,
            y + inset,
            inset_width,
            inset_height,
            radius.saturating_sub(inset),
            rgba,
        );
    }
}

fn draw_rounded_rect_outline(
    buffer: &mut [u8],
    stride: usize,
    buffer_width: u32,
    buffer_height: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    radius: u32,
    rgba: [u8; 4],
) {
    if width == 0 || height == 0 {
        return;
    }
    let radius = radius.min(width / 2).min(height / 2);
    let right = x.saturating_add(width);
    let bottom = y.saturating_add(height);
    for py in y..bottom.min(buffer_height) {
        for px in x..right.min(buffer_width) {
            let local_x = px - x;
            let local_y = py - y;
            if rounded_rect_contains(local_x, local_y, width, height, radius)
                && is_rounded_rect_edge(local_x, local_y, width, height, radius)
            {
                put_argb_pixel(buffer, stride, px, py, rgba);
            }
        }
    }
}

fn rounded_rect_contains(x: u32, y: u32, width: u32, height: u32, radius: u32) -> bool {
    if radius == 0 {
        return x < width && y < height;
    }
    let left = radius;
    let right = width.saturating_sub(radius + 1);
    let top = radius;
    let bottom = height.saturating_sub(radius + 1);
    if (left..=right).contains(&x) || (top..=bottom).contains(&y) {
        return true;
    }
    let cx = if x < left { left } else { right } as i32;
    let cy = if y < top { top } else { bottom } as i32;
    let dx = x as i32 - cx;
    let dy = y as i32 - cy;
    dx * dx + dy * dy <= radius as i32 * radius as i32
}

fn is_rounded_rect_edge(x: u32, y: u32, width: u32, height: u32, radius: u32) -> bool {
    if width <= 2 || height <= 2 {
        return true;
    }
    x == 0
        || y == 0
        || x + 1 == width
        || y + 1 == height
        || !rounded_rect_contains(x.saturating_sub(1), y, width, height, radius)
        || !rounded_rect_contains(x.saturating_add(1), y, width, height, radius)
        || !rounded_rect_contains(x, y.saturating_sub(1), width, height, radius)
        || !rounded_rect_contains(x, y.saturating_add(1), width, height, radius)
}

fn source_span_to_draw_len(
    start: u32,
    end: u32,
    viewport_start: u32,
    viewport_len: u32,
    draw_height: u32,
) -> u32 {
    let start_y = source_to_draw_y(start, viewport_start, viewport_len, draw_height);
    let end_y = source_to_draw_y(end, viewport_start, viewport_len, draw_height);
    end_y.saturating_sub(start_y).max(1)
}

fn source_to_draw_y(
    source_y: u32,
    viewport_start: u32,
    viewport_len: u32,
    draw_height: u32,
) -> u32 {
    if viewport_len == 0 {
        return 0;
    }
    let relative = source_y.saturating_sub(viewport_start).min(viewport_len);
    ((relative as u64 * draw_height as u64) / viewport_len as u64)
        .min(draw_height.saturating_sub(1) as u64) as u32
}

fn draw_preview_position_indicator(
    buffer: &mut [u8],
    stride: usize,
    buffer_width: u32,
    buffer_height: u32,
    image: &Image,
    x: u32,
    y: u32,
    draw_width: u32,
    draw_height: u32,
    viewport: PreviewViewport,
    capture_pos: u32,
    capture_len: u32,
    accent: [u8; 3],
) {
    if image.height() == 0 || capture_len == 0 || draw_height < 12 || draw_width < 8 {
        return;
    }
    let track_width = 3;
    let track_x = x + draw_width.saturating_sub(track_width + 2);
    fill_rect(
        buffer,
        stride,
        buffer_width,
        buffer_height,
        track_x,
        y + 2,
        track_width,
        draw_height.saturating_sub(4),
        [255, 255, 255, 38],
    );

    let usable_height = draw_height.saturating_sub(4);
    let marker_height = ((capture_len as u64 * usable_height as u64 + image.height() as u64 / 2)
        / image.height() as u64)
        .max(4)
        .min(usable_height as u64) as u32;
    let capture_center = capture_pos
        .saturating_add(capture_len / 2)
        .min(image.height());
    let center_y = y
        + 2
        + ((capture_center as u64 * usable_height as u64 + image.height() as u64 / 2)
            / image.height() as u64) as u32;
    let marker_y = center_y.saturating_sub(marker_height / 2);
    if viewport.max_source_start > 0 {
        let thumb_height = ((viewport.source_len as u64 * usable_height as u64)
            / image.height() as u64)
            .max(10)
            .min(usable_height as u64) as u32;
        let thumb_y = y
            + 2
            + ((viewport.source_start as u64 * usable_height.saturating_sub(thumb_height) as u64)
                / viewport.max_source_start as u64) as u32;

        fill_rect(
            buffer,
            stride,
            buffer_width,
            buffer_height,
            track_x,
            thumb_y,
            track_width,
            thumb_height,
            [255, 255, 255, 92],
        );
    }

    fill_rect(
        buffer,
        stride,
        buffer_width,
        buffer_height,
        track_x.saturating_sub(1),
        marker_y.min(y + 2 + usable_height.saturating_sub(marker_height)),
        track_width + 2,
        marker_height,
        [accent[0], accent[1], accent[2], 205],
    );
}

fn blit_latest_viewport(
    buffer: &mut [u8],
    stride: usize,
    buffer_width: u32,
    buffer_height: u32,
    image: &Image,
    x: u32,
    y: u32,
    draw_width: u32,
    draw_height: u32,
    viewport: PreviewViewport,
) {
    if image.width() == 0 || image.height() == 0 || draw_width == 0 || draw_height == 0 {
        return;
    }
    for dy in 0..draw_height {
        let sy = (((viewport.scaled_start + dy) as u64 * image.height() as u64)
            / viewport.scaled_height as u64) as u32;
        for dx in 0..draw_width {
            let sx = (dx as u64 * image.width() as u64 / draw_width as u64) as u32;
            let pixel = image.get_pixel(sx.min(image.width() - 1), sy.min(image.height() - 1));
            put_argb_pixel_checked(
                buffer,
                stride,
                buffer_width,
                buffer_height,
                (x + dx) as i32,
                (y + dy) as i32,
                [pixel[0], pixel[1], pixel[2], pixel[3]],
            );
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PreviewViewport {
    scaled_height: u32,
    scaled_start: u32,
    source_start: u32,
    source_len: u32,
    max_source_start: u32,
}

fn preview_viewport(
    image: &Image,
    draw_width: u32,
    draw_height: u32,
    source_pos: u32,
    frame_len: u32,
    edge: PreviewEdge,
) -> PreviewViewport {
    let scaled_height = scale_height_for_width(image.width(), image.height(), draw_width);
    let max_scaled_start = scaled_height.saturating_sub(draw_height);
    let visible_scaled = draw_height.min(scaled_height).max(1);
    let source_len = ((visible_scaled as u64 * image.height() as u64) / scaled_height as u64)
        .max(1)
        .min(image.height() as u64) as u32;
    let max_source_start = image.height().saturating_sub(source_len);
    let source_start = match edge {
        PreviewEdge::End => source_pos
            .saturating_add(frame_len)
            .saturating_sub(source_len),
        PreviewEdge::Start | PreviewEdge::None => source_pos,
    }
    .min(max_source_start);
    let scaled_start = ((source_start as u64 * scaled_height as u64) / image.height() as u64)
        .min(max_scaled_start as u64) as u32;
    PreviewViewport {
        scaled_height,
        scaled_start,
        source_start,
        source_len,
        max_source_start,
    }
}

fn fill_rect(
    buffer: &mut [u8],
    stride: usize,
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    rect_width: u32,
    rect_height: u32,
    rgba: [u8; 4],
) {
    let end_y = (y + rect_height).min(height);
    let end_x = (x + rect_width).min(width);
    for py in y..end_y {
        for px in x..end_x {
            put_argb_pixel(buffer, stride, px, py, rgba);
        }
    }
}

fn draw_box(
    buffer: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    x: u32,
    y: u32,
    rect_width: u32,
    rect_height: u32,
    accent: [u8; 3],
) {
    buffer.fill(0);
    let safe_gap = 2.0_f32;
    let cx = x as f32 - safe_gap;
    let cy = y as f32 - safe_gap;
    let cw = rect_width as f32 + safe_gap * 2.0;
    let ch = rect_height as f32 + safe_gap * 2.0;
    stroke_rect(
        buffer,
        stride,
        width,
        height,
        cx - 1.0,
        cy - 1.0,
        cw + 2.0,
        ch + 2.0,
        2,
        [accent[0], accent[1], accent[2], 77],
    );

    let corner_len = 20.0_f32.min(cw / 4.0).min(ch / 4.0);
    let bx = cx - 1.5;
    let by = cy - 1.5;
    let bw = cw + 3.0;
    let bh = ch + 3.0;
    let color = [accent[0], accent[1], accent[2], 255];

    stroke_line(
        buffer,
        stride,
        width,
        height,
        bx,
        by + corner_len,
        bx,
        by,
        3,
        color,
    );
    stroke_line(
        buffer,
        stride,
        width,
        height,
        bx,
        by,
        bx + corner_len,
        by,
        3,
        color,
    );
    stroke_line(
        buffer,
        stride,
        width,
        height,
        bx + bw - corner_len,
        by,
        bx + bw,
        by,
        3,
        color,
    );
    stroke_line(
        buffer,
        stride,
        width,
        height,
        bx + bw,
        by,
        bx + bw,
        by + corner_len,
        3,
        color,
    );
    stroke_line(
        buffer,
        stride,
        width,
        height,
        bx + bw,
        by + bh - corner_len,
        bx + bw,
        by + bh,
        3,
        color,
    );
    stroke_line(
        buffer,
        stride,
        width,
        height,
        bx + bw,
        by + bh,
        bx + bw - corner_len,
        by + bh,
        3,
        color,
    );
    stroke_line(
        buffer,
        stride,
        width,
        height,
        bx + corner_len,
        by + bh,
        bx,
        by + bh,
        3,
        color,
    );
    stroke_line(
        buffer,
        stride,
        width,
        height,
        bx,
        by + bh,
        bx,
        by + bh - corner_len,
        3,
        color,
    );
}

fn stroke_rect(
    buffer: &mut [u8],
    stride: usize,
    width: u32,
    height: u32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    line_width: u32,
    rgba: [u8; 4],
) {
    stroke_line(
        buffer,
        stride,
        width,
        height,
        x,
        y,
        x + w,
        y,
        line_width,
        rgba,
    );
    stroke_line(
        buffer,
        stride,
        width,
        height,
        x + w,
        y,
        x + w,
        y + h,
        line_width,
        rgba,
    );
    stroke_line(
        buffer,
        stride,
        width,
        height,
        x + w,
        y + h,
        x,
        y + h,
        line_width,
        rgba,
    );
    stroke_line(
        buffer,
        stride,
        width,
        height,
        x,
        y + h,
        x,
        y,
        line_width,
        rgba,
    );
}

fn stroke_line(
    buffer: &mut [u8],
    stride: usize,
    width: u32,
    height: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    line_width: u32,
    rgba: [u8; 4],
) {
    if (x0 - x1).abs() < 0.001 {
        let x = x0.round() as i32;
        let start = y0.min(y1).round() as i32;
        let end = y0.max(y1).round() as i32;
        let radius = line_width as i32 / 2;
        for dx in -radius..=radius {
            for y in start..=end {
                put_argb_pixel_checked(buffer, stride, width, height, x + dx, y, rgba);
            }
        }
        return;
    }
    if (y0 - y1).abs() < 0.001 {
        let y = y0.round() as i32;
        let start = x0.min(x1).round() as i32;
        let end = x0.max(x1).round() as i32;
        let radius = line_width as i32 / 2;
        for dy in -radius..=radius {
            for x in start..=end {
                put_argb_pixel_checked(buffer, stride, width, height, x, y + dy, rgba);
            }
        }
    }
}

fn put_argb_pixel(buffer: &mut [u8], stride: usize, x: u32, y: u32, rgba: [u8; 4]) {
    let offset = y as usize * stride + x as usize * 4;
    if offset + 3 >= buffer.len() {
        return;
    }
    let alpha = rgba[3] as u16;
    buffer[offset] = ((rgba[2] as u16 * alpha) / 255) as u8;
    buffer[offset + 1] = ((rgba[1] as u16 * alpha) / 255) as u8;
    buffer[offset + 2] = ((rgba[0] as u16 * alpha) / 255) as u8;
    buffer[offset + 3] = rgba[3];
}

fn put_argb_pixel_checked(
    buffer: &mut [u8],
    stride: usize,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    rgba: [u8; 4],
) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    put_argb_pixel(buffer, stride, x as u32, y as u32, rgba);
}
