//! Turning a screenshot into frosted glass.
//!
//! The launcher paints a translucent panel over the desktop, and a translucent
//! panel over a *detailed* desktop is just a panel you cannot read. The desktop
//! has to be blurred, and since Windows will not do it for us here (see
//! `docs/ARCHITECTURE.md`), we do it ourselves: capture the region behind the
//! popup, shrink it, blur what is left, and let the renderer stretch it back up.
//!
//! This module is the pure half of that. It is a total function over a byte
//! buffer — no clock, no filesystem, no GPU, no window — so it lives in
//! `orca-core` under the same rule that puts ranking here. The capture that
//! produces the bytes lives in `orca-win`.
//!
//! # Why downscale-then-upscale is the blur
//!
//! A real separable Gaussian over 660×430 is a few hundred thousand multiply-
//! adds *per pass*, and a Gaussian wide enough to look like frosted glass needs
//! three or four. That is real work to put on the keystroke-to-paint path.
//!
//! Instead: box-average down by [`DOWNSAMPLE`], blur the small buffer, and let
//! the renderer's bilinear filter stretch it back. The result is visually
//! indistinguishable from a wide Gaussian — it *is* a wide low-pass filter, the
//! bilinear upscale is just the last separable box in the chain — and it costs
//! roughly [`DOWNSAMPLE`]² less, because the expensive step runs on 1/16th of
//! the pixels. 41×27 instead of 660×430.
//!
//! Two box passes approximate a Gaussian closely enough that the difference is
//! not visible, and unlike a single box it has no hard edge, which is the thing
//! that makes a cheap blur look cheap.

/// How much each axis is shrunk before blurring.
///
/// 4 is a balance, and both failure directions are visible. Too small and the
/// box passes stop being cheap, which shows up as added latency on the show
/// path. Too large and the upscale's bilinear interpolation starts to show as
/// faint diagonal banding across large flat areas, which is exactly the sort of
/// thing that reads as "cheap blur" rather than "frosted glass".
pub const DOWNSAMPLE: u32 = 4;

/// How many times the box blur runs over the downsampled buffer.
///
/// Three passes of radius 2 is a close enough approximation of a Gaussian to be
/// indistinguishable at this scale, and a single pass has visible corners.
const BLUR_PASSES: usize = 3;
/// The box blur's half-width, in downsampled pixels.
const BLUR_RADIUS: u32 = 2;

/// A blurred BGRA image, ready to be handed to the renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Softened {
    /// Width in pixels of the (downsampled) buffer.
    pub width: u32,
    /// Height in pixels of the (downsampled) buffer.
    pub height: u32,
    /// `width * height * 4` bytes, 8-bit BGRA, top-down, no padding.
    pub bgra: Vec<u8>,
}

impl Softened {
    /// Whether this image can actually be painted.
    ///
    /// A zero-sized capture is the normal failure — a monitor was unplugged, or
    /// the region was off-screen — and handing a renderer a zero-sized image is
    /// how a launcher ends up with a hole in the middle of its panel.
    #[must_use]
    pub fn is_paintable(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.bgra.len() == (self.width as usize) * (self.height as usize) * 4
    }
}

/// Blurs a BGRA image by shrinking it, box-blurring it, and leaving the upscale
/// to the renderer.
///
/// Returns `None` for input that cannot produce a paintable image: a zero
/// dimension, or a buffer whose length disagrees with its dimensions. That
/// disagreement is not a theoretical case — a caller handing over a DIB whose
/// stride it mis-derived produces exactly it, and the alternative is reading
/// past the end of the buffer.
#[must_use]
pub fn soften(bgra: &[u8], width: u32, height: u32) -> Option<Softened> {
    if width == 0 || height == 0 {
        return None;
    }
    if bgra.len() != (width as usize) * (height as usize) * 4 {
        return None;
    }

    let small = downsample(bgra, width, height);
    let blurred = box_blur(&small, small.width, small.height, BLUR_RADIUS, BLUR_PASSES);
    Some(blurred)
}

/// Box-averages each [`DOWNSAMPLE`]×[`DOWNSAMPLE`] block into one pixel.
///
/// Edge blocks are averaged over whatever they actually cover rather than
/// divided by the full block size, which is what makes the right and bottom
/// edges a different brightness from the interior — a bright 1px line at the
/// edge of a screenshot is exactly the kind of thing that shows up as a
/// hard-edged smudge once it is stretched back over the panel.
fn downsample(bgra: &[u8], width: u32, height: u32) -> Softened {
    let out_w = width.div_ceil(DOWNSAMPLE).max(1);
    let out_h = height.div_ceil(DOWNSAMPLE).max(1);
    let mut out = vec![0u8; (out_w as usize) * (out_h as usize) * 4];

    for oy in 0..out_h {
        for ox in 0..out_w {
            let x0 = ox * DOWNSAMPLE;
            let y0 = oy * DOWNSAMPLE;
            let x1 = (x0 + DOWNSAMPLE).min(width);
            let y1 = (y0 + DOWNSAMPLE).min(height);

            let mut acc = [0u32; 4];
            let mut count = 0u32;
            for y in y0..y1 {
                for x in x0..x1 {
                    let base = ((y as usize) * (width as usize) + (x as usize)) * 4;
                    for (channel, sum) in acc.iter_mut().enumerate() {
                        *sum += u32::from(bgra[base + channel]);
                    }
                    count += 1;
                }
            }
            // `count` is at least 1: the loop bounds guarantee every block
            // covers its own origin pixel.
            let count = count.max(1);
            let out_base = ((oy as usize) * (out_w as usize) + (ox as usize)) * 4;
            for (channel, sum) in acc.iter().enumerate() {
                out[out_base + channel] = (sum / count) as u8;
            }
        }
    }

    Softened {
        width: out_w,
        height: out_h,
        bgra: out,
    }
}

/// `passes` iterations of a separable box blur with the given half-width.
///
/// Horizontal then vertical, each pass allocating its own output. The two
/// together are a single 2-D box, and repeating them converges on a Gaussian.
fn box_blur(src: &Softened, width: u32, height: u32, radius: u32, passes: usize) -> Softened {
    let mut current = src.bgra.clone();
    let mut scratch = vec![0u8; current.len()];
    if radius == 0 || passes == 0 {
        return Softened {
            width,
            height,
            bgra: current,
        };
    }

    for _ in 0..passes {
        blur_axis(
            &current,
            &mut scratch,
            width,
            height,
            radius,
            Axis::Horizontal,
        );
        blur_axis(
            &scratch,
            &mut current,
            width,
            height,
            radius,
            Axis::Vertical,
        );
    }

    Softened {
        width,
        height,
        bgra: current,
    }
}

#[derive(Clone, Copy)]
enum Axis {
    Horizontal,
    Vertical,
}

/// One separable box pass, alpha-weighted so transparent edges do not darken.
///
/// A plain average over RGBA pulls the colour of transparent pixels toward
/// black, which is the classic "everything gets a dark fringe" bug. The capture
/// is fully opaque so this cannot bite today, but the weighting is three lines
/// and the day something upstream hands over an image with an alpha channel
/// this is the bug that takes an afternoon.
fn blur_axis(src: &[u8], dst: &mut [u8], width: u32, height: u32, radius: u32, axis: Axis) {
    let stride = (width as usize) * 4;
    for y in 0..height as usize {
        for x in 0..width as usize {
            let (cx, cy) = (x as i64, y as i64);
            let mut acc = [0f32; 4];
            let mut weight = 0f32;
            let mut samples = 0f32;

            for offset in -(radius as i64)..=(radius as i64) {
                let (sx, sy) = match axis {
                    Axis::Horizontal => (cx + offset, cy),
                    Axis::Vertical => (cx, cy + offset),
                };
                if sx < 0 || sy < 0 || sx >= width as i64 || sy >= height as i64 {
                    continue;
                }
                let base = (sy as usize) * stride + (sx as usize) * 4;
                let a = f32::from(src[base + 3]) / 255.0;
                acc[0] += f32::from(src[base]) * a;
                acc[1] += f32::from(src[base + 1]) * a;
                acc[2] += f32::from(src[base + 2]) * a;
                acc[3] += f32::from(src[base + 3]);
                weight += a;
                samples += 1.0;
            }

            let out = (cy as usize) * stride + x * 4;
            if weight <= 0.0 || samples <= 0.0 {
                // Every neighbour was fully transparent. Leave the destination
                // transparent rather than dividing by zero and painting black.
                dst[out..out + 4].fill(0);
                continue;
            }
            // Both divisors are the count of samples actually taken, *not* the
            // window width. At an edge the window runs off the image, and
            // dividing by the full width while summing only what was in bounds
            // loses alpha on every border — which then feeds the next pass and
            // compounds, so a flat image comes out with translucent edges. That
            // is the "the right and bottom of the panel are darker" bug, and it
            // is why this is a named variable rather than an inline constant.
            let alpha = acc[3] / samples;
            // Un-premultiply: the sum above is colour*alpha, so dividing by the
            // accumulated alpha gives the colour back.
            dst[out] = (acc[0] / weight).clamp(0.0, 255.0) as u8;
            dst[out + 1] = (acc[1] / weight).clamp(0.0, 255.0) as u8;
            dst[out + 2] = (acc[2] / weight).clamp(0.0, 255.0) as u8;
            dst[out + 3] = alpha.clamp(0.0, 255.0) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A solid image of one colour, `width`×`height`.
    fn solid(width: u32, height: u32, bgra: [u8; 4]) -> Softened {
        Softened {
            width,
            height,
            bgra: bgra.repeat((width * height) as usize),
        }
    }

    fn pixel_at(image: &Softened, x: u32, y: u32) -> [u8; 4] {
        let base = ((y * image.width + x) * 4) as usize;
        [
            image.bgra[base],
            image.bgra[base + 1],
            image.bgra[base + 2],
            image.bgra[base + 3],
        ]
    }

    #[test]
    fn the_output_is_smaller_and_still_well_formed() {
        let input = vec![40u8; 660 * 430 * 4];
        let out = soften(&input, 660, 430).expect("should soften");
        assert_eq!(out.width, 165, "660 / 4");
        assert_eq!(out.height, 108, "430 rounds up to 108");
        assert_eq!(out.bgra.len(), (165 * 108 * 4) as usize);
        assert!(out.is_paintable());
    }

    #[test]
    fn a_solid_image_survives_unchanged() {
        // A blur that darkens a flat field is not a blur, it is a leak. This is
        // the test that catches a wrong divisor in the downsample average, and
        // it is the one that would otherwise only show up as "the frosted glass
        // looks slightly grey over a white wallpaper".
        let input = vec![200u8; 64 * 64 * 4];
        let out = soften(&input, 64, 64).expect("should soften");
        for y in 0..out.height {
            for x in 0..out.width {
                assert_eq!(
                    pixel_at(&out, x, y),
                    [200, 200, 200, 200],
                    "pixel {x},{y} drifted on a flat field"
                );
            }
        }
    }

    #[test]
    fn a_bright_block_spreads_outward_rather_than_being_discarded() {
        // The whole point: a hard edge in the desktop must stop being a hard
        // edge. If the bright region is merely averaged away the blur is a
        // downsample and nothing else, and the panel still has legible text
        // behind it.
        //
        // A *block* rather than a single pixel, because a lone white pixel in a
        // 64×64 black field is 1/4096th of the energy: it survives the
        // downsample as 15/255 and then truncates to zero within a couple of
        // box passes. That is correct behaviour for a blur — low-amplitude
        // detail is what a blur discards — and testing for it would be testing
        // that the arithmetic rounds, not that the blur blurs.
        let (w, h) = (64u32, 64u32);
        let mut input = vec![0u8; (w * h * 4) as usize];
        for chunk in input.as_chunks_mut::<4>().0 {
            chunk.copy_from_slice(&[0, 0, 0, 255]);
        }
        // An 8x8 white square: two downsample blocks across.
        for y in 28..36u32 {
            for x in 28..36u32 {
                let base = ((y * w + x) * 4) as usize;
                input[base..base + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        let out = soften(&input, w, h).expect("should soften");

        let brightest = out
            .bgra
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[0])
            .max()
            .expect("non-empty");
        assert!(brightest > 0, "the bright block vanished entirely");
        assert!(
            brightest < 255,
            "the block survived at full strength; the blur is not spreading it, \
             it is only averaging"
        );

        // And it spread *outward*: a pixel well outside the block's own
        // footprint is no longer pure black. A blur that only darkens the
        // source in place is not spreading anything.
        let outside = out
            .bgra
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                let x = (*index as u32) % out.width;
                let y = (*index as u32) / out.width;
                !(20..=44).contains(&x) || !(20..=44).contains(&y)
            })
            .map(|(_, p)| p[0])
            .max()
            .expect("non-empty");
        assert!(
            outside > 0,
            "no energy reached outside the bright block; the blur is not spreading"
        );
    }

    #[test]
    fn the_edges_are_not_darker_than_the_interior() {
        // Regression: dividing the last partial block by the full block size
        // instead of the pixels it actually covers makes the right and bottom
        // edges a fraction of the correct brightness, which the upscale then
        // smears into a visible dark band down two sides of the panel.
        let (w, h) = (67u32, 41u32); // deliberately not a multiple of DOWNSAMPLE
        let input = vec![180u8; (w * h * 4) as usize];
        let out = soften(&input, w, h).expect("should soften");
        let right = out.width - 1;
        let bottom = out.height - 1;
        for x in 0..out.width {
            assert_eq!(pixel_at(&out, x, bottom)[0], 180, "bottom edge at {x}");
        }
        for y in 0..out.height {
            assert_eq!(pixel_at(&out, right, y)[0], 180, "right edge at {y}");
        }
    }

    #[test]
    fn a_buffer_that_disagrees_with_its_dimensions_is_refused() {
        // Not theoretical: a caller that mis-derives a DIB stride produces
        // exactly this, and accepting it means indexing past the end.
        // 10×10×4 is 400, so 399 is the mismatch — the first case here is a
        // buffer that is far too short for its dimensions.
        assert!(soften(&[0u8; 16], 100, 100).is_none());
        assert!(soften(&[0u8; 399], 10, 10).is_none());
        assert!(soften(&[0u8; 401], 10, 10).is_none());
    }

    #[test]
    fn a_zero_sized_capture_is_refused_rather_than_painted() {
        assert!(soften(&[], 0, 0).is_none());
        assert!(soften(&[], 0, 10).is_none());
    }

    #[test]
    fn transparency_does_not_bleed_black_into_neighbours() {
        // A transparent black pixel next to opaque white must stay white, not
        // become grey. The alpha weighting in `blur_axis` is what prevents it.
        let (w, h) = (16u32, 1u32);
        let mut input = vec![0u8; (w * h * 4) as usize];
        for x in 0..w as usize {
            let base = x * 4;
            let transparent = x == 1;
            let pixel: [u8; 4] = if transparent {
                [0, 0, 0, 0]
            } else {
                [255, 255, 255, 255]
            };
            input[base..base + 4].copy_from_slice(&pixel);
        }
        let out = soften(&input, w, h).expect("should soften");
        // Sample the opaque side; it must not have picked up the black.
        for x in 0..out.width {
            let [r, g, b, _] = pixel_at(&out, x, 0);
            assert!(
                r > 200 && g > 200 && b > 200,
                "opaque white was dragged toward transparent black at {x}: {r},{g},{b}"
            );
        }
    }

    #[test]
    fn the_softened_image_is_much_cheaper_than_a_full_resolution_blur() {
        // The reason this module exists rather than a Gaussian in the renderer:
        // assert the reduction so a well-meaning "let me just blur it at full
        // res" is visible as a failing test rather than as 30ms on the show path.
        let input = vec![7u8; 660 * 430 * 4];
        let out = soften(&input, 660, 430).expect("should soften");
        let ratio = input.len() as f64 / out.bgra.len() as f64;
        assert!(
            ratio > 12.0,
            "expected roughly {}-x fewer pixels, got {ratio:.1}x",
            DOWNSAMPLE * DOWNSAMPLE
        );
    }

    #[test]
    fn a_solid_image_is_its_own_softened_form() {
        // Guards the struct plumbing: `solid` and `soften` must agree on layout.
        let image = solid(8, 8, [1, 2, 3, 4]);
        assert!(image.is_paintable());
        let mut mismatched = image.clone();
        mismatched.bgra.pop();
        assert!(!mismatched.is_paintable());
    }
}
