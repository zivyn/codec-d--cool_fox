//! A 2560x1440 SDR display for the supported Series X retail image (the title
//! 1.0.2.1): the title presents at 3840x2160 on Series X, its scene drawn at
//! 2560x1440 and scaled up by its own temporal pass. With 1440p asked for
//! (`VICTORY_NATIVE_1440P=1`, or `VICTORY_RENDER_1080P=1` for a 1920x1080
//! scene scaled to 1440p), the image's SDR display mode, the scanout
//! reservation, the temporal scene scale and the Settings viewport load are
//! patched before it is mapped: the title then makes a 1440p output, and no
//! 4K pass runs.
//!
//! Ported from DexNom's fork (DexNom/VIctory, `apps/victory/src/native_1440p.rs`,
//! GPL-3.0-or-later), its native 1440p part only. Every field is checked
//! against the stock bytes first: another build is refused, not patched.

use anyhow::{Context, Result, ensure};
use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};
use victory_loader::PreparedImage;

const RETAIL_BUILD: &str = "b99f876b74d4a77f03af5f878223b27bd647b5c35a79d5decd9f716504239e93";
pub(super) const WIDTH: u32 = 2560;
pub(super) const HEIGHT: u32 = 1440;
const SDR_MODE: u32 = 0x0615_c100;
const STOCK_SDR_MODE: [u8; 16] = [0, 15, 0, 0, 112, 8, 0, 0, 60, 0, 0, 0, 1, 0, 0, 0];
const SCANOUT_RESERVATION: u32 = 0x0247_ad4a;
const TEMPORAL_SCALE: u32 = 0x028f_1e7e;
const UI_VIEWPORT_LOAD: u32 = 0x0580_48d4;

/// Patches `image` for a 2560x1440 output; the scene is drawn at 2560x1440,
/// or at 1920x1080 with `render_1080p`. Returns the scene's size.
pub(super) fn prepare(image: &mut PreparedImage, render_1080p: bool) -> Result<(u32, u32)> {
    ensure!(
        image.original_hash.to_hex().as_str() == RETAIL_BUILD,
        "1440p output is only known for the retail build {RETAIL_BUILD}"
    );

    let mode = field::<16>(image, SDR_MODE)?;
    ensure!(*mode == STOCK_SDR_MODE, "unexpected Series X SDR mode");
    mode[..4].copy_from_slice(&WIDTH.to_le_bytes());
    mode[4..8].copy_from_slice(&HEIGHT.to_le_bytes());

    // Two 64 KiB-aligned, tiled RGBA8 scanout buffers. The reservation is
    // rounded to 2 MiB, matching the title's allocation check.
    let texture = Texture::new(&TextureDesc {
        dimension: Dimension::Tex2d,
        width: WIDTH,
        height: HEIGHT,
        depth_or_array: 1,
        mips: 1,
        format: 28,
        samples: 1,
        swizzle: Swizzle::SW_64KB_D_X,
    })
    .map_err(|error| anyhow::anyhow!("1440p scanout layout: {error:?}"))?;
    let pool_size =
        u32::try_from((texture.size.next_multiple_of(64 * 1024) * 2).next_multiple_of(2 << 20))?;
    let reservation = field::<5>(image, SCANOUT_RESERVATION)?;
    ensure!(*reservation == [0xb9, 0, 0, 0, 4], "unexpected Series X scanout reservation");
    reservation[1..].copy_from_slice(&pool_size.to_le_bytes());

    let scale = field::<4>(image, TEMPORAL_SCALE)?;
    ensure!(*scale == (2.0f32 / 3.0).to_le_bytes(), "unexpected Series X temporal scene scale");
    let (render_width, render_height) = if render_1080p { (1920, 1080) } else { (WIDTH, HEIGHT) };
    *scale = (render_width as f32 / WIDTH as f32).to_le_bytes();

    // Settings lays out its preview panels with a viewport setter of its
    // own that loads the legacy UI size (3840x2160) even when a 1440p UI
    // surface was made: it reads the selected SDR mode instead.
    let viewport = field::<8>(image, UI_VIEWPORT_LOAD)?;
    ensure!(
        *viewport == [0xc5, 0xfb, 0x10, 0x0d, 0x10, 0xf0, 0x13, 0x05],
        "unexpected Settings viewport size load"
    );
    let displacement = (SDR_MODE as i32) - (UI_VIEWPORT_LOAD as i32 + 8);
    viewport[4..].copy_from_slice(&displacement.to_le_bytes());
    Ok((render_width, render_height))
}

fn field<const N: usize>(image: &mut PreparedImage, rva: u32) -> Result<&mut [u8; N]> {
    let offset = image.info.rva_to_file_offset(rva).context("display profile field missing")? as usize;
    image
        .bytes
        .get_mut(offset..offset + N)
        .context("incomplete display profile field")?
        .try_into()
        .context("invalid display profile field length")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With the retail executable named by `VICTORY_DISPLAY_PROFILE_TEST_IMAGE`
    /// (not part of the repository): the four fields hold what a 1440p
    /// output needs, for both scene sizes; without it, nothing is checked.
    #[test]
    fn patches_the_retail_image_for_1440p() {
        let Some(path) = std::env::var_os("VICTORY_DISPLAY_PROFILE_TEST_IMAGE") else {
            return;
        };
        let bytes = std::fs::read(path).unwrap();
        for (render_1080p, scene) in [(false, (2560, 1440)), (true, (1920, 1080))] {
            let mut image = victory_loader::prepare(&bytes).unwrap();
            assert_eq!(prepare(&mut image, render_1080p).unwrap(), scene);
            let mode = *field::<16>(&mut image, SDR_MODE).unwrap();
            assert_eq!(mode[..4], WIDTH.to_le_bytes());
            assert_eq!(mode[4..8], HEIGHT.to_le_bytes());
            let scale = f32::from_le_bytes(*field::<4>(&mut image, TEMPORAL_SCALE).unwrap());
            assert_eq!((WIDTH as f32 * scale) as u32, scene.0);
            let load = *field::<8>(&mut image, UI_VIEWPORT_LOAD).unwrap();
            let disp = i32::from_le_bytes(load[4..].try_into().unwrap());
            assert_eq!(UI_VIEWPORT_LOAD.wrapping_add(8).wrapping_add_signed(disp), SDR_MODE);
            // Patched once, the image is no longer the stock one: refused.
            assert!(prepare(&mut image, render_1080p).is_err());
        }
    }
}
