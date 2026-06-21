use super::Image;
use image::{ImageBuffer, Rgba};
use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub struct SubtitleConfig {
    pub bottom_ratio: f32,
    pub stable_frames: usize,
    pub min_gap_frames: usize,
    pub calibration_frames: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubtitleRegion {
    pub y: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubtitleStatus {
    Calibrating { frames: usize, needed: usize },
    Ready,
    Added,
    Duplicate,
}

pub struct SubtitleStitcher {
    config: SubtitleConfig,
    region: Option<SubtitleRegion>,
    calibration: Vec<Image>,
    last_feature: Option<Vec<f32>>,
    pending_feature: Option<Vec<f32>>,
    pending_strip: Option<Image>,
    pending_seen: usize,
    frames_since_add: usize,
    output: Option<Image>,
    frames: usize,
    strips: usize,
}

impl SubtitleStitcher {
    pub fn new(config: SubtitleConfig) -> Self {
        Self {
            config,
            region: None,
            calibration: Vec::new(),
            last_feature: None,
            pending_feature: None,
            pending_strip: None,
            pending_seen: 0,
            frames_since_add: usize::MAX / 2,
            output: None,
            frames: 0,
            strips: 0,
        }
    }

    pub fn push_frame(&mut self, frame: &Image, debug_dir: Option<&Path>) -> SubtitleStatus {
        self.frames += 1;
        if self.region.is_none() {
            self.calibration.push(frame.clone());
            if self.calibration.len() < self.config.calibration_frames.max(1) {
                return SubtitleStatus::Calibrating {
                    frames: self.calibration.len(),
                    needed: self.config.calibration_frames.max(1),
                };
            }
            self.region = Some(detect_region(&self.calibration, self.config.bottom_ratio));
            self.calibration.clear();
            return SubtitleStatus::Ready;
        }

        self.frames_since_add = self.frames_since_add.saturating_add(1);
        let region = self.region.expect("region is set");
        let strip = crop_region(frame, region);
        let feature = subtitle_feature(&strip);

        if feature_energy(&feature) < 0.01 {
            return SubtitleStatus::Duplicate;
        }
        if let Some(last) = &self.last_feature {
            if feature_diff(last, &feature) < 0.045 {
                self.pending_feature = None;
                self.pending_strip = None;
                self.pending_seen = 0;
                return SubtitleStatus::Duplicate;
            }
        }
        if self.frames_since_add < self.config.min_gap_frames {
            return SubtitleStatus::Duplicate;
        }

        if let Some(pending) = &self.pending_feature {
            if feature_diff(pending, &feature) < 0.035 {
                self.pending_seen += 1;
                if self.pending_seen >= self.config.stable_frames.max(1) {
                    let strip = self.pending_strip.take().unwrap_or(strip);
                    self.append_strip(&strip);
                    self.last_feature = Some(feature);
                    self.pending_feature = None;
                    self.pending_seen = 0;
                    self.frames_since_add = 0;
                    if let Some(dir) = debug_dir {
                        let _ = std::fs::create_dir_all(dir);
                        let _ = strip.save(dir.join(format!("subtitle_{:05}.png", self.strips)));
                    }
                    return SubtitleStatus::Added;
                }
            } else {
                self.pending_feature = Some(feature);
                self.pending_strip = Some(strip);
                self.pending_seen = 1;
            }
        } else {
            self.pending_feature = Some(feature);
            self.pending_strip = Some(strip);
            self.pending_seen = 1;
        }

        SubtitleStatus::Duplicate
    }

    pub fn image(&self) -> Option<&Image> {
        self.output.as_ref()
    }

    pub fn finish(mut self) -> Option<Image> {
        if self.output.is_none() {
            if let Some(strip) = self.pending_strip.take() {
                self.append_strip(&strip);
            }
        }
        self.output
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn strips(&self) -> usize {
        self.strips
    }

    fn append_strip(&mut self, strip: &Image) {
        const SEPARATOR: u32 = 2;
        let width = self
            .output
            .as_ref()
            .map_or(strip.width(), ImageBuffer::width)
            .max(strip.width());
        let old = self.output.take();
        let old_height = old.as_ref().map_or(0, ImageBuffer::height);
        let sep = if old_height == 0 { 0 } else { SEPARATOR };
        let new_height = old_height + sep + strip.height();
        let mut merged = Image::new(width, new_height);
        if let Some(old) = old {
            copy_image(&old, &mut merged, 0, 0);
            if sep > 0 {
                fill_separator(&mut merged, old_height, sep);
            }
        }
        copy_image(strip, &mut merged, 0, old_height + sep);
        self.output = Some(merged);
        self.strips += 1;
    }
}

fn detect_region(frames: &[Image], bottom_ratio: f32) -> SubtitleRegion {
    let Some(first) = frames.first() else {
        return SubtitleRegion { y: 0, height: 1 };
    };
    let height = first.height().max(1);
    let width = first.width().max(1);
    let search_start = (height as f32 * 0.45).round() as u32;
    let search_end = (height as f32 * 0.96)
        .round()
        .max(search_start as f32 + 1.0) as u32;
    let search_end = search_end.min(height);
    let mut scores = vec![0.0; height as usize];

    for frame in frames {
        for y in search_start..search_end {
            scores[y as usize] += row_text_score(frame, y) / frames.len().max(1) as f32;
        }
    }

    let mut best_y = search_start;
    let mut best_score = 0.0;
    let window = (height / 18).clamp(16, 80).min(search_end - search_start);
    if window > 0 {
        for y in search_start..=search_end.saturating_sub(window) {
            let score: f32 = (y..y + window).map(|row| scores[row as usize]).sum();
            if score > best_score {
                best_score = score;
                best_y = y;
            }
        }
    }

    if best_score <= width as f32 * 0.08 {
        let fallback_h = ((height as f32 * bottom_ratio.clamp(0.12, 0.5)).round() as u32).max(24);
        return SubtitleRegion {
            y: height.saturating_sub(fallback_h),
            height: fallback_h.min(height),
        };
    }

    let padding = (window / 3).max(8);
    let y = best_y.saturating_sub(padding);
    let end = (best_y + window + padding).min(height);
    SubtitleRegion {
        y,
        height: (end - y).max(1),
    }
}

fn row_text_score(image: &Image, y: u32) -> f32 {
    let mut score = 0.0;
    let step = (image.width() / 240).max(1);
    let prev_y = y.saturating_sub(1);
    let mut x = 1;
    while x < image.width() {
        let g = gray(image.get_pixel(x, y));
        let left = gray(image.get_pixel(x - 1, y));
        let up = gray(image.get_pixel(x, prev_y));
        let edge = (g - left).abs().max((g - up).abs());
        if g > 175.0 {
            score += 1.0;
        }
        if edge > 35.0 {
            score += 1.5;
        }
        x += step;
    }
    score
}

fn crop_region(image: &Image, region: SubtitleRegion) -> Image {
    let y = region.y.min(image.height());
    let h = region.height.min(image.height().saturating_sub(y)).max(1);
    let mut out = Image::new(image.width(), h);
    let row_bytes = image.width() as usize * 4;
    for row in 0..h as usize {
        let src = (y as usize + row) * row_bytes;
        let dst = row * row_bytes;
        out.as_mut()[dst..dst + row_bytes].copy_from_slice(&image.as_raw()[src..src + row_bytes]);
    }
    out
}

fn subtitle_feature(strip: &Image) -> Vec<f32> {
    let cols = 32;
    let rows = 12;
    let mut feature = vec![0.0; cols * rows];
    let mut counts = vec![0u32; cols * rows];
    for y in 0..strip.height() {
        let by = ((y as usize * rows) / strip.height().max(1) as usize).min(rows - 1);
        for x in 1..strip.width() {
            let bx = ((x as usize * cols) / strip.width().max(1) as usize).min(cols - 1);
            let g = gray(strip.get_pixel(x, y));
            let left = gray(strip.get_pixel(x - 1, y));
            let edge = (g - left).abs();
            let value = if g > 185.0 { 0.7 } else { 0.0 } + if edge > 35.0 { 0.9 } else { 0.0 };
            let index = by * cols + bx;
            feature[index] += value;
            counts[index] += 1;
        }
    }
    for (value, count) in feature.iter_mut().zip(counts) {
        if count > 0 {
            *value = (*value / count as f32).min(1.0);
        }
    }
    feature
}

fn feature_diff(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return f32::INFINITY;
    }
    a.iter()
        .zip(b)
        .map(|(left, right)| (left - right).abs())
        .sum::<f32>()
        / a.len() as f32
}

fn feature_energy(feature: &[f32]) -> f32 {
    if feature.is_empty() {
        0.0
    } else {
        feature.iter().copied().sum::<f32>() / feature.len() as f32
    }
}

fn gray(pixel: &Rgba<u8>) -> f32 {
    0.299 * pixel[0] as f32 + 0.587 * pixel[1] as f32 + 0.114 * pixel[2] as f32
}

fn copy_image(source: &Image, dest: &mut Image, x: u32, y: u32) {
    for py in 0..source.height() {
        for px in 0..source.width() {
            if x + px < dest.width() && y + py < dest.height() {
                dest.put_pixel(x + px, y + py, *source.get_pixel(px, py));
            }
        }
    }
}

fn fill_separator(image: &mut Image, y: u32, height: u32) {
    for py in y..(y + height).min(image.height()) {
        for px in 0..image.width() {
            image.put_pixel(px, py, Rgba([210, 201, 115, 255]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_bottom_subtitle_region() {
        let mut frame = Image::from_pixel(200, 120, Rgba([20, 20, 20, 255]));
        for y in 84..96 {
            for x in (30..170).step_by(6) {
                frame.put_pixel(x, y, Rgba([240, 240, 240, 255]));
            }
        }
        let region = detect_region(&vec![frame; 4], 0.25);
        assert!(region.y <= 84);
        assert!(region.y + region.height >= 96);
    }
}
