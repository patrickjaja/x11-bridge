//! Screenshot and region (zoom) capture over X11.
//!
//! Captures the root window with `xproto::get_image` (ZPixmap) over the
//! resolved monitor's rectangle, converts the server's native pixel layout to
//! packed RGB, downscales to the same caps kwin-portal-bridge uses
//! (long edge <= 1568 px, total <= 1_150_000 px), and JPEG-encodes to inline
//! base64.
//!
//! ## Image format decision
//!
//! kwin-portal-bridge emits **JPEG quality 75** (`src/capture.rs`
//! `encode_resized_jpeg_base64`). The JS reader (`js/cu_linux_executor.js`) never
//! inspects a mime type: `screenshotResultFromRust` / `zoom` only read
//! `result.base64`/`width`/`height` and hand the base64 straight to the CU
//! model. To stay a drop-in substitute for the kwin bridge (the reference the
//! JS was written against), we match it exactly: JPEG q75.
//!
//! ## Coordinate semantics for `zoom`
//!
//! `zoom`'s `(x, y, w, h)` are **relative to the selected monitor's top-left**
//! (matching kwin-portal-bridge `crop_logical_region`, which treats the region
//! as an offset within `screen.geometry`, and the JS `zoom` caller which passes
//! `region.x/y/w/h` unchanged). X11 scale is 1.0 (see DESIGN.md), so logical ==
//! physical: we add the monitor origin to get root coordinates and capture that
//! sub-rectangle directly, clamping to the monitor bounds.

use anyhow::{Context, Result, bail};
use base64::Engine;
use image::codecs::jpeg::JpegEncoder;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat, ImageOrder};

use crate::conn::{Conn, Monitor};
use crate::output::{ScreenshotCapture, ScreenshotResult};

/// Long-edge cap on the encoded image, in physical pixels.
/// Mirrors kwin-portal-bridge `MAX_LONG_EDGE`.
pub const MAX_LONG_EDGE: u32 = 1568;
/// Total-pixel cap on the encoded image. Mirrors kwin-portal-bridge `MAX_PIXELS`.
pub const MAX_PIXELS: u32 = 1_150_000;
/// JPEG quality, matching kwin-portal-bridge `SCREENSHOT_JPEG_QUALITY`.
const JPEG_QUALITY: u8 = 75;

/// A packed 8-bit-per-channel RGB image (row-major, no padding).
struct RgbImage {
    width: u32,
    height: u32,
    /// `width * height * 3` bytes, R,G,B per pixel.
    pixels: Vec<u8>,
}

/// Capture a full monitor. `display` selects the monitor (default: primary).
pub fn screenshot(conn: &Conn, display: Option<&str>) -> Result<ScreenshotResult> {
    let monitor = conn.resolve_monitor(display)?;
    // Clamp the monitor rect to the root geometry (a monitor can, in principle,
    // extend past the reported root size on a mid-reconfiguration; GetImage
    // would then error). Bail if nothing is left.
    let rect = clamp_to_root(conn, monitor.x, monitor.y, monitor.width, monitor.height)?;

    let rgb = capture_rect(conn, rect)?;
    let target = compute_target_dims(monitor.width as u32, monitor.height as u32);
    let encoded = encode_resized_jpeg_base64(&rgb, target)?;

    Ok(ScreenshotResult {
        base64: encoded.base64,
        width: encoded.width,
        height: encoded.height,
        display_width: monitor.width.max(0) as u32,
        display_height: monitor.height.max(0) as u32,
        display_id: monitor.name,
        origin_x: monitor.x,
        origin_y: monitor.y,
    })
}

/// Capture a logical region `(x, y, w, h)` within a monitor. Coordinates are
/// relative to the monitor's top-left.
pub fn zoom(
    conn: &Conn,
    display: Option<&str>,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<ScreenshotCapture> {
    if w <= 0 || h <= 0 {
        bail!("zoom region must have positive width and height");
    }

    let monitor = conn.resolve_monitor(display)?;

    // Region is relative to the monitor; translate to root coordinates and
    // clamp to the monitor's rectangle (which is itself clamped to root).
    let region = clamp_region_to_monitor(&monitor, x, y, w, h)?;
    let rect = clamp_to_root(conn, region.0, region.1, region.2, region.3)?;

    let rgb = capture_rect(conn, rect)?;
    let target = compute_target_dims(rect.width as u32, rect.height as u32);
    encode_resized_jpeg_base64(&rgb, target)
}

/// A rectangle in X11 root pixels, already validated non-empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RootRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// Intersect `(x, y, w, h)` with the root window geometry; bail on empty.
fn clamp_to_root(conn: &Conn, x: i32, y: i32, w: i32, h: i32) -> Result<RootRect> {
    let geom = conn
        .conn
        .get_geometry(conn.root)
        .context("failed to request root geometry")?
        .reply()
        .context("failed to read root geometry reply")?;
    let root_w = i32::from(geom.width);
    let root_h = i32::from(geom.height);

    intersect(x, y, w, h, root_w, root_h)
        .ok_or_else(|| anyhow::anyhow!("capture region does not intersect the root window"))
}

/// Clamp a monitor-relative region to the monitor, returning root coordinates.
fn clamp_region_to_monitor(
    monitor: &Monitor,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<(i32, i32, i32, i32)> {
    // Offset within the monitor is clamped to >= 0 (a negative offset would
    // spill onto a neighbouring monitor, which zoom is not meant to do).
    let local_x = x.max(0);
    let local_y = y.max(0);
    let inter =
        intersect(local_x, local_y, w, h, monitor.width, monitor.height).ok_or_else(|| {
            anyhow::anyhow!(
                "zoom region {x},{y} {w}x{h} does not intersect display `{}` ({}x{})",
                monitor.name,
                monitor.width,
                monitor.height
            )
        })?;
    Ok((
        monitor.x + inter.x,
        monitor.y + inter.y,
        inter.width,
        inter.height,
    ))
}

/// Intersect a rect with `0,0 .. bound_w,bound_h`. `None` if empty.
fn intersect(x: i32, y: i32, w: i32, h: i32, bound_w: i32, bound_h: i32) -> Option<RootRect> {
    if w <= 0 || h <= 0 || bound_w <= 0 || bound_h <= 0 {
        return None;
    }
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w).min(bound_w);
    let y1 = (y + h).min(bound_h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(RootRect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

/// GetImage the given root rectangle and convert to packed RGB.
fn capture_rect(conn: &Conn, rect: RootRect) -> Result<RgbImage> {
    let width = u16::try_from(rect.width).context("capture width overflows u16")?;
    let height = u16::try_from(rect.height).context("capture height overflows u16")?;
    let x = i16::try_from(rect.x).context("capture x overflows i16")?;
    let y = i16::try_from(rect.y).context("capture y overflows i16")?;

    let reply = conn
        .conn
        .get_image(ImageFormat::Z_PIXMAP, conn.root, x, y, width, height, !0)
        .context("failed to request GetImage")?
        .reply()
        .context("failed to read GetImage reply")?;

    let bits_per_pixel = bits_per_pixel_for_depth(conn, reply.depth)?;
    let byte_order = conn.conn.setup().image_byte_order;

    decode_zpixmap(
        &reply.data,
        rect.width as u32,
        rect.height as u32,
        reply.depth,
        bits_per_pixel,
        byte_order,
    )
}

/// Look up the server's `bits_per_pixel` for a ZPixmap of the given depth.
fn bits_per_pixel_for_depth(conn: &Conn, depth: u8) -> Result<u8> {
    conn.conn
        .setup()
        .pixmap_formats
        .iter()
        .find(|format| format.depth == depth)
        .map(|format| format.bits_per_pixel)
        .ok_or_else(|| anyhow::anyhow!("no pixmap format advertised for depth {depth}"))
}

/// Convert raw ZPixmap bytes to packed RGB.
///
/// Supports depth 24 and 32 at 32 bits per pixel (the only layouts a modern X
/// server hands out for root captures). Byte order is honoured per the server
/// setup: on `LSB_FIRST` a 32-bit pixel `0x00RRGGBB` lands in memory as
/// `[B, G, R, x]` (BGRx); on `MSB_FIRST` as `[x, R, G, B]` (xRGB).
fn decode_zpixmap(
    data: &[u8],
    width: u32,
    height: u32,
    depth: u8,
    bits_per_pixel: u8,
    byte_order: ImageOrder,
) -> Result<RgbImage> {
    if bits_per_pixel != 32 {
        bail!("unsupported bits_per_pixel {bits_per_pixel} (only 32 is handled)");
    }
    if depth != 24 && depth != 32 {
        bail!("unsupported depth {depth} (only 24 and 32 are handled)");
    }

    let w = width as usize;
    let h = height as usize;
    // ZPixmap rows are padded to scanline_pad bits; at 32 bpp each pixel is 4
    // bytes and width*4 is already a multiple of 4, so the row stride equals
    // width*4 with no extra padding.
    let stride = w * 4;
    let needed = stride.checked_mul(h).context("image dimensions overflow")?;
    if data.len() < needed {
        bail!(
            "GetImage returned {} bytes, expected at least {needed} for {width}x{height}",
            data.len()
        );
    }

    // Byte offsets of R, G, B within each 4-byte pixel.
    let (ro, go, bo) = match byte_order {
        ImageOrder::LSB_FIRST => (2usize, 1usize, 0usize), // BGRx
        ImageOrder::MSB_FIRST => (1usize, 2usize, 3usize), // xRGB
        other => bail!("unexpected image byte order {other:?}"),
    };

    let mut pixels = vec![0u8; w * h * 3];
    for row in 0..h {
        let src_row = &data[row * stride..row * stride + w * 4];
        let dst_row = &mut pixels[row * w * 3..(row + 1) * w * 3];
        for col in 0..w {
            let s = col * 4;
            let d = col * 3;
            dst_row[d] = src_row[s + ro];
            dst_row[d + 1] = src_row[s + go];
            dst_row[d + 2] = src_row[s + bo];
        }
    }

    Ok(RgbImage {
        width,
        height,
        pixels,
    })
}

/// kwin-portal-bridge downscale math: cap the long edge at `MAX_LONG_EDGE` and
/// the total pixel count at `MAX_PIXELS`, keeping aspect ratio, never upscaling.
/// Scale is 1.0 on X11 so logical == physical.
fn compute_target_dims(logical_w: u32, logical_h: u32) -> (u32, u32) {
    let phys_w = (logical_w.max(1)) as f64;
    let phys_h = (logical_h.max(1)) as f64;
    let long_edge_scale = (MAX_LONG_EDGE as f64) / phys_w.max(phys_h);
    let pixel_scale = ((MAX_PIXELS as f64) / (phys_w * phys_h)).sqrt();
    let scale = 1.0_f64.min(long_edge_scale).min(pixel_scale);

    (
        (phys_w * scale).round().max(1.0) as u32,
        (phys_h * scale).round().max(1.0) as u32,
    )
}

/// Downscale (or copy) `rgb` to `target` and JPEG-encode to base64.
fn encode_resized_jpeg_base64(rgb: &RgbImage, target: (u32, u32)) -> Result<ScreenshotCapture> {
    let (tw, th) = target;
    let resized = if tw == rgb.width && th == rgb.height {
        RgbImage {
            width: rgb.width,
            height: rgb.height,
            pixels: rgb.pixels.clone(),
        }
    } else {
        resize_area_average(rgb, tw, th)
    };

    let mut jpeg = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY);
    encoder
        .encode(
            &resized.pixels,
            resized.width,
            resized.height,
            image::ExtendedColorType::Rgb8,
        )
        .context("failed to JPEG-encode screenshot")?;

    Ok(ScreenshotCapture {
        base64: base64::engine::general_purpose::STANDARD.encode(&jpeg),
        width: resized.width,
        height: resized.height,
    })
}

/// Box (area-average) downscale. Each destination pixel averages the source
/// pixels covering its footprint. This is only ever called for downscaling
/// (the caps never upscale), where an area filter gives clean, alias-free
/// results without pulling in a resampling dependency.
fn resize_area_average(src: &RgbImage, dst_w: u32, dst_h: u32) -> RgbImage {
    let dst_w = dst_w.max(1);
    let dst_h = dst_h.max(1);
    let sw = src.width.max(1);
    let sh = src.height.max(1);
    let mut out = vec![0u8; (dst_w as usize) * (dst_h as usize) * 3];

    let x_ratio = sw as f64 / dst_w as f64;
    let y_ratio = sh as f64 / dst_h as f64;

    for dy in 0..dst_h {
        let sy0 = (dy as f64 * y_ratio).floor() as u32;
        let sy1 = (((dy + 1) as f64 * y_ratio).ceil() as u32)
            .min(sh)
            .max(sy0 + 1);
        for dx in 0..dst_w {
            let sx0 = (dx as f64 * x_ratio).floor() as u32;
            let sx1 = (((dx + 1) as f64 * x_ratio).ceil() as u32)
                .min(sw)
                .max(sx0 + 1);

            let mut r = 0u64;
            let mut g = 0u64;
            let mut b = 0u64;
            let mut count = 0u64;
            for sy in sy0..sy1 {
                let row = (sy * sw) as usize * 3;
                for sx in sx0..sx1 {
                    let s = row + sx as usize * 3;
                    r += u64::from(src.pixels[s]);
                    g += u64::from(src.pixels[s + 1]);
                    b += u64::from(src.pixels[s + 2]);
                    count += 1;
                }
            }
            let count = count.max(1);
            let d = ((dy * dst_w + dx) as usize) * 3;
            out[d] = (r / count) as u8;
            out[d + 1] = (g / count) as u8;
            out[d + 2] = (b / count) as u8;
        }
    }

    RgbImage {
        width: dst_w,
        height: dst_h,
        pixels: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsb_first_is_bgrx() {
        // One pixel, stored little-endian as [B, G, R, x].
        let data = [0x10u8, 0x20, 0x30, 0xFF];
        let img = decode_zpixmap(&data, 1, 1, 24, 32, ImageOrder::LSB_FIRST).unwrap();
        assert_eq!(img.pixels, vec![0x30, 0x20, 0x10]); // R, G, B
    }

    #[test]
    fn msb_first_is_xrgb() {
        // One pixel, stored big-endian as [x, R, G, B].
        let data = [0xFFu8, 0x30, 0x20, 0x10];
        let img = decode_zpixmap(&data, 1, 1, 24, 32, ImageOrder::MSB_FIRST).unwrap();
        assert_eq!(img.pixels, vec![0x30, 0x20, 0x10]); // R, G, B
    }

    #[test]
    fn depth_32_decodes_like_depth_24() {
        let data = [0x10u8, 0x20, 0x30, 0x40];
        let img = decode_zpixmap(&data, 1, 1, 32, 32, ImageOrder::LSB_FIRST).unwrap();
        assert_eq!(img.pixels, vec![0x30, 0x20, 0x10]);
    }

    #[test]
    fn multi_pixel_row_layout() {
        // 2x1 image, LSB (BGRx): pixel0 = red, pixel1 = green.
        let data = [
            0x00, 0x00, 0xFF, 0xFF, // R
            0x00, 0xFF, 0x00, 0xFF, // G
        ];
        let img = decode_zpixmap(&data, 2, 1, 24, 32, ImageOrder::LSB_FIRST).unwrap();
        assert_eq!(img.pixels, vec![0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00]);
    }

    #[test]
    fn rejects_unsupported_bpp() {
        let data = [0u8; 4];
        assert!(decode_zpixmap(&data, 1, 1, 24, 16, ImageOrder::LSB_FIRST).is_err());
    }

    #[test]
    fn rejects_short_buffer() {
        let data = [0u8; 3]; // need 4 for one 32bpp pixel
        assert!(decode_zpixmap(&data, 1, 1, 24, 32, ImageOrder::LSB_FIRST).is_err());
    }

    #[test]
    fn target_dims_no_downscale_when_small() {
        assert_eq!(compute_target_dims(800, 600), (800, 600));
    }

    #[test]
    fn target_dims_long_edge_cap() {
        // 3000x1000: long-edge cap 1568 dominates over the pixel cap here?
        // pixels = 3e6 > 1.15e6, so pixel cap applies too. Verify caps hold.
        let (w, h) = compute_target_dims(3000, 1000);
        assert!(w <= MAX_LONG_EDGE && h <= MAX_LONG_EDGE);
        assert!((w * h) <= MAX_PIXELS + 2000); // rounding slack
        // aspect ratio preserved (3:1)
        let ratio = w as f64 / h as f64;
        assert!((ratio - 3.0).abs() < 0.05, "ratio {ratio}");
    }

    #[test]
    fn target_dims_pixel_cap_1080p() {
        // 1920x1080 = 2.07M px > 1.15M, so it must be scaled down.
        let (w, h) = compute_target_dims(1920, 1080);
        assert!((w * h) <= MAX_PIXELS + 2000);
        assert!(w <= MAX_LONG_EDGE);
        let ratio = w as f64 / h as f64;
        assert!((ratio - (1920.0 / 1080.0)).abs() < 0.02, "ratio {ratio}");
    }

    #[test]
    fn resize_half_averages() {
        // 2x2 solid gray -> 1x1 must be the same gray.
        let src = RgbImage {
            width: 2,
            height: 2,
            pixels: vec![100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100],
        };
        let out = resize_area_average(&src, 1, 1);
        assert_eq!(out.width, 1);
        assert_eq!(out.height, 1);
        assert_eq!(out.pixels, vec![100, 100, 100]);
    }

    #[test]
    fn resize_half_mixed_averages() {
        // 2x1: black + white -> 1x1 mid-gray (127).
        let src = RgbImage {
            width: 2,
            height: 1,
            pixels: vec![0, 0, 0, 255, 255, 255],
        };
        let out = resize_area_average(&src, 1, 1);
        assert_eq!(out.pixels, vec![127, 127, 127]);
    }

    #[test]
    fn intersect_clamps_negative_origin() {
        let r = intersect(-10, -10, 30, 30, 100, 100).unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (0, 0, 20, 20));
    }

    #[test]
    fn intersect_clamps_overflow() {
        let r = intersect(90, 90, 30, 30, 100, 100).unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (90, 90, 10, 10));
    }

    #[test]
    fn intersect_empty_outside() {
        assert!(intersect(200, 200, 10, 10, 100, 100).is_none());
    }

    #[test]
    fn zoom_region_translated_to_root() {
        let monitor = Monitor {
            name: "DP-1".into(),
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080,
            primary: false,
        };
        // region (10,20) 100x50 relative to monitor -> root (1930,20) 100x50.
        let (x, y, w, h) = clamp_region_to_monitor(&monitor, 10, 20, 100, 50).unwrap();
        assert_eq!((x, y, w, h), (1930, 20, 100, 50));
    }

    #[test]
    fn zoom_region_clamped_to_monitor_edge() {
        let monitor = Monitor {
            name: "eDP-1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        // Overreaching region clamps to the monitor's bottom-right.
        let (x, y, w, h) = clamp_region_to_monitor(&monitor, 1900, 1060, 200, 200).unwrap();
        assert_eq!((x, y, w, h), (1900, 1060, 20, 20));
    }

    #[test]
    fn zoom_region_outside_monitor_errors() {
        let monitor = Monitor {
            name: "eDP-1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        assert!(clamp_region_to_monitor(&monitor, 5000, 5000, 100, 100).is_err());
    }
}
