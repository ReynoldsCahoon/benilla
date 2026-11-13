//! The iOS touch controls' art ([`crate::virtual_stick`], [`crate::jump_button`]), drawn as quads in
//! the player UI's own pass ([`crate::ui_pass`]) rather than as Bevy UI nodes: in the world nothing
//! else is Bevy UI, and its pass alone reloaded and stored the full-size target for one ring
//! (~1 ms of the iPad Air 2's GPU a frame, 2026-11-13).
//!
//! Each piece is a small white texture whose alpha is the piece's coverage, anti-aliased by its
//! distance to the edge, made once at startup.
//!
//! Deviation: benilla's own controls; the reference has no touch input.

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

use crate::ui_pass::{UiQuad, UiQuads, UvRect};

/// The append lane's key above every scripted region, so the controls paint over the interface as
/// the nodes did (they sat at `GlobalZIndex(900)`, under only the loading cover).
pub(crate) const TOUCH_Z: u128 = u128::MAX - 1;

/// The three pieces' textures.
#[derive(Resource, Clone)]
pub(crate) struct TouchArt {
    pub(crate) ring: Handle<Image>,
    pub(crate) knob: Handle<Image>,
    pub(crate) jump: Handle<Image>,
}

/// Texels per logical px: the iPad's Retina factor, so the art is sharp at its drawn size.
const TEXEL_SCALE: f32 = 2.0;

/// A disc of `diameter` logical px: `fill` alpha inside, a `border`-px rim of `rim` alpha, and
/// any `extra(x, y)` coverage (logical px from the centre, y down) drawn at `extra_alpha` on top.
fn disc(
    diameter: f32,
    border: f32,
    fill: f32,
    rim: f32,
    extra: impl Fn(f32, f32) -> f32,
    extra_alpha: f32,
) -> Image {
    let side = (diameter * TEXEL_SCALE).ceil() as u32;
    let r = diameter / 2.0;
    let mut data = Vec::with_capacity((side * side * 4) as usize);
    for ty in 0..side {
        for tx in 0..side {
            let x = (tx as f32 + 0.5) / TEXEL_SCALE - r;
            let y = (ty as f32 + 0.5) / TEXEL_SCALE - r;
            let d = (x * x + y * y).sqrt();
            // Coverage of the disc and of its inner fill, a px of anti-aliasing at each edge.
            let outer = (r - d + 0.5).clamp(0.0, 1.0);
            let inner = (r - border - d + 0.5).clamp(0.0, 1.0);
            let base = fill * inner + rim * (outer - inner);
            let mark = extra(x, y).clamp(0.0, 1.0) * outer;
            let a = base + (extra_alpha - base).max(0.0) * mark;
            data.extend_from_slice(&[255, 255, 255, (a.clamp(0.0, 1.0) * 255.0).round() as u8]);
        }
    }
    Image::new(
        Extent3d {
            width: side,
            height: side,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    )
}

/// Coverage of a capsule from `a` to `b` of half-width `hw`, a px of anti-aliasing.
fn capsule(p: Vec2, a: Vec2, b: Vec2, hw: f32) -> f32 {
    let ab = b - a;
    let t = ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0);
    (hw - p.distance(a + ab * t) + 0.5).clamp(0.0, 1.0)
}

/// Make the textures once.
fn make_touch_art(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    use crate::{jump_button, virtual_stick};
    let ring = images.add(disc(
        2.0 * virtual_stick::RADIUS,
        2.0,
        0.12,
        0.4,
        |_, _| 0.0,
        0.0,
    ));
    let knob = images.add(disc(
        2.0 * virtual_stick::RADIUS * virtual_stick::KNOB_SCALE,
        2.0,
        0.35,
        0.4,
        |_, _| 0.0,
        0.0,
    ));
    // The upward chevron: two 20x4 px strokes at ±45° meeting at the top, as the nodes drew it.
    let jump = images.add(disc(
        2.0 * jump_button::RADIUS,
        2.0,
        0.12,
        0.4,
        |x, y| {
            let p = Vec2::new(x, y);
            // The nodes' strokes: 20 px long, centred at (±6, 3), meeting near (0, -4).
            let apex = Vec2::new(0.0, -4.0);
            capsule(p, apex, Vec2::new(-14.0, 10.0), 2.0).max(capsule(
                p,
                apex,
                Vec2::new(14.0, 10.0),
                2.0,
            ))
        },
        0.5,
    ));
    commands.insert_resource(TouchArt { ring, knob, jump });
}

/// One piece as a quad centred at `center`, `diameter` logical px across.
pub(crate) fn push(quads: &mut UiQuads, texture: &Handle<Image>, center: Vec2, diameter: f32) {
    quads.overlays.push(UiQuad {
        rect: Rect::from_center_size(center, Vec2::splat(diameter)),
        z_key: TOUCH_Z,
        texture: Some(texture.clone()),
        uv: UvRect::FULL,
        ..default()
    });
}

/// Makes the art on iOS, where the controls draw.
pub(crate) struct TouchArtPlugin;

impl Plugin for TouchArtPlugin {
    fn build(&self, app: &mut App) {
        if cfg!(target_os = "ios") {
            app.add_systems(Startup, make_touch_art);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disc_is_fill_inside_rim_at_the_edge_and_clear_outside() {
        let img = disc(20.0, 2.0, 0.12, 0.4, |_, _| 0.0, 0.0);
        let side = img.width() as usize;
        let alpha = |tx: usize, ty: usize| img.data.as_ref().unwrap()[(ty * side + tx) * 4 + 3];
        let c = side / 2;
        assert_eq!(
            alpha(c, c),
            (0.12f32 * 255.0).round() as u8,
            "the fill at the centre"
        );
        assert_eq!(alpha(0, 0), 0, "nothing in the corner");
        // A texel 1 logical px inside the edge lies on the rim.
        let rim = alpha(c, (TEXEL_SCALE * 1.0) as usize);
        assert_eq!(rim, (0.4f32 * 255.0).round() as u8, "the rim");
    }
}
