//! Frame conversion and JPEG encoding.
//!
//! Converts a PipeWire `VideoFrame` (BGRx/BGRA/RGBx/RGBA) to RGB, downscales to
//! the CU caps (1568 long edge / 1.15M pixels), and JPEG-encodes at quality 75.
//! These values match kwin-portal-bridge exactly so screenshots look identical
//! across bridges.

use anyhow::{Context, Result, bail};
use base64::Engine;
use image::RgbImage;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use lamco_pipewire::{FrameBuffer, PixelFormat, VideoFrame};

use crate::output::{Screen, ScreenshotCapture, ScreenshotResult};

const SCREENSHOT_JPEG_QUALITY: u8 = 75;
const MAX_LONG_EDGE: u32 = 1568;
const MAX_PIXELS: u32 = 1_150_000;

/// Build a full-monitor screenshot result from a captured frame.
pub fn screenshot_result_from_frame(
    screen: &Screen,
    frame: &VideoFrame,
) -> Result<ScreenshotResult> {
    let rgb = rgb_from_frame(frame)?;
    let encoded = encode_resized_jpeg_base64(
        &rgb,
        compute_target_dims(
            screen.geometry.width as u32,
            screen.geometry.height as u32,
            screen.scale.unwrap_or(1.0),
        ),
    )?;

    Ok(ScreenshotResult {
        base64: encoded.base64,
        width: encoded.width,
        height: encoded.height,
        display_width: screen.geometry.width as u32,
        display_height: screen.geometry.height as u32,
        display_id: screen.id.clone(),
        origin_x: screen.geometry.x,
        origin_y: screen.geometry.y,
    })
}

/// Build a cropped zoom result from a captured frame.
pub fn zoom_result_from_frame(
    screen: &Screen,
    frame: &VideoFrame,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<ScreenshotCapture> {
    if w <= 0 || h <= 0 {
        bail!("zoom region must have positive width and height");
    }
    let rgb = rgb_from_frame(frame)?;
    let cropped = crop_logical_region(&rgb, screen, x, y, w, h)?;
    // The crop is already in stream (physical) pixels and may have been
    // clamped, so size the output from it rather than the requested region.
    encode_resized_jpeg_base64(
        &cropped,
        compute_target_dims(cropped.width(), cropped.height(), 1.0),
    )
}

/// Convert a memory-backed frame to an `RgbImage`. DMA-BUF frames are rejected
/// (we request memory buffers with `use_dmabuf: false`).
fn rgb_from_frame(frame: &VideoFrame) -> Result<RgbImage> {
    let bytes = match &frame.buffer {
        FrameBuffer::Memory(data) => data.as_ref(),
        FrameBuffer::DmaBuf(_) => bail!("cannot convert DMA-BUF frame without import support"),
    };

    let width = frame.width as usize;
    let height = frame.height as usize;
    let stride = frame.stride as usize;
    let mut rgb = vec![0u8; width * height * 3];

    for y in 0..height {
        let src_row = &bytes[y * stride..y * stride + width * 4];
        let dst_row = &mut rgb[y * width * 3..(y + 1) * width * 3];
        for x in 0..width {
            let s = x * 4;
            let d = x * 3;
            match frame.format {
                PixelFormat::BGRx | PixelFormat::BGRA => {
                    dst_row[d] = src_row[s + 2];
                    dst_row[d + 1] = src_row[s + 1];
                    dst_row[d + 2] = src_row[s];
                }
                PixelFormat::RGBx | PixelFormat::RGBA => {
                    dst_row[d] = src_row[s];
                    dst_row[d + 1] = src_row[s + 1];
                    dst_row[d + 2] = src_row[s + 2];
                }
                other => bail!("unsupported pixel format for screenshot export: {other:?}"),
            }
        }
    }

    RgbImage::from_raw(frame.width, frame.height, rgb)
        .ok_or_else(|| anyhow::anyhow!("failed to construct RGB image buffer"))
}

/// Crop a logical region (in the screen's logical coordinates) out of an RGB
/// frame captured at the stream's pixel resolution.
fn crop_logical_region(
    rgb: &RgbImage,
    screen: &Screen,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<RgbImage> {
    // Clamp to the display instead of refusing: callers round a region out of
    // Electron's logical bounds, and fractional scales make the two sides
    // disagree by a pixel. Only a region with nothing inside the display is
    // an error.
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w).min(screen.geometry.width);
    let y1 = (y + h).min(screen.geometry.height);
    if x1 <= x0 || y1 <= y0 {
        bail!(
            "zoom region {x},{y} {w}x{h} lies outside display `{}` logical bounds {}x{}",
            screen.id,
            screen.geometry.width,
            screen.geometry.height,
        );
    }
    let (x, y, w, h) = (x0, y0, x1 - x0, y1 - y0);

    let scale_x = f64::from(rgb.width()) / f64::from(screen.geometry.width.max(1));
    let scale_y = f64::from(rgb.height()) / f64::from(screen.geometry.height.max(1));

    let crop_x = ((x as f64) * scale_x).round() as u32;
    let crop_y = ((y as f64) * scale_y).round() as u32;
    let crop_w = ((w as f64) * scale_x).round().max(1.0) as u32;
    let crop_h = ((h as f64) * scale_y).round().max(1.0) as u32;

    let max_w = rgb.width().saturating_sub(crop_x);
    let max_h = rgb.height().saturating_sub(crop_y);
    let clamped_w = crop_w.min(max_w).max(1);
    let clamped_h = crop_h.min(max_h).max(1);

    Ok(image::imageops::crop_imm(rgb, crop_x, crop_y, clamped_w, clamped_h).to_image())
}

fn encode_resized_jpeg_base64(rgb: &RgbImage, target: (u32, u32)) -> Result<ScreenshotCapture> {
    let resized = image::imageops::resize(rgb, target.0, target.1, FilterType::CatmullRom);
    let mut jpeg = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut jpeg, SCREENSHOT_JPEG_QUALITY);
    encoder
        .encode_image(&resized)
        .context("failed to JPEG-encode screenshot")?;
    Ok(ScreenshotCapture {
        base64: base64::engine::general_purpose::STANDARD.encode(jpeg),
        width: resized.width(),
        height: resized.height(),
    })
}

/// Compute the downscaled output size honoring the long-edge and pixel caps.
/// Public + tested because the scaling math is the one piece of capture that is
/// verifiable without a live PipeWire stream.
pub fn compute_target_dims(logical_w: u32, logical_h: u32, scale_factor: f64) -> (u32, u32) {
    let safe_scale = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };

    let phys_w = ((logical_w as f64) * safe_scale).round().max(1.0);
    let phys_h = ((logical_h as f64) * safe_scale).round().max(1.0);
    let long_edge_scale = (MAX_LONG_EDGE as f64) / phys_w.max(phys_h);
    let pixel_scale = ((MAX_PIXELS as f64) / (phys_w * phys_h)).sqrt();
    let scale = 1.0_f64.min(long_edge_scale).min(pixel_scale);

    (
        (phys_w * scale).round().max(1.0) as u32,
        (phys_h * scale).round().max(1.0) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_image_is_not_upscaled() {
        assert_eq!(compute_target_dims(800, 600, 1.0), (800, 600));
    }

    #[test]
    fn long_edge_cap_applies() {
        // 4000x1000 -> long edge capped to 1568.
        let (w, h) = compute_target_dims(4000, 1000, 1.0);
        assert_eq!(w, 1568);
        assert_eq!(h, 392);
    }

    #[test]
    fn pixel_cap_applies() {
        // A near-square 2000x2000 (4M px) is limited by the 1.15M pixel cap,
        // not the long-edge cap.
        let (w, h) = compute_target_dims(2000, 2000, 1.0);
        assert!(u64::from(w) * u64::from(h) <= u64::from(MAX_PIXELS) + 4000);
        assert_eq!(w, h);
    }

    #[test]
    fn fractional_scale_expands_physical_size() {
        // 800x600 @ 1.5 scale = 1200x900 physical (1.08M px, long edge 1200),
        // under both caps, so it is emitted at full physical size.
        assert_eq!(compute_target_dims(800, 600, 1.5), (1200, 900));
    }

    #[test]
    fn fractional_scale_over_pixel_cap_downscales() {
        // 1280x720 @ 1.5 = 1920x1080 = 2.07M px, over the 1.15M pixel cap, so
        // it is downscaled proportionally.
        let (w, h) = compute_target_dims(1280, 720, 1.5);
        assert!(u64::from(w) * u64::from(h) <= u64::from(MAX_PIXELS) + 4000);
        // Aspect ratio (16:9) is preserved.
        assert!((f64::from(w) / f64::from(h) - 16.0 / 9.0).abs() < 0.01);
    }

    #[test]
    fn non_finite_scale_falls_back_to_one() {
        assert_eq!(compute_target_dims(800, 600, f64::NAN), (800, 600));
        assert_eq!(compute_target_dims(800, 600, 0.0), (800, 600));
    }
}
