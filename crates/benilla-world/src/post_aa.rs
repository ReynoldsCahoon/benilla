//! Post-process anti-aliasing on the world view: Bevy's FXAA or SMAA, which smooth edges in the
//! finished image, so unlike MSAA they also reach alpha-tested cutouts (leaves, fences, grass).
//! Off by default. The `postAntiAlias` CVar (the Video page's Anti-Aliasing dropdown) sets it, in
//! the spellings [`PostAa::parse`] reads; `$WOW_POST_AA` overrides it for the session.
//!
//! Either pass reads the main texture and writes the other, so a view with one on gives up
//! [`SingleMainTexture`] (one more texture the size of the 3D target; the world view's tonemapping
//! is `None`, so the pass works on the target's `Rgba16Float` straight). The UI camera's FFXGlow
//! combine reads the view's current main texture, which the pass has flipped to its output.
//!
//! Deviation: benilla's own; the reference has no post-process anti-aliasing.

use bevy::anti_alias::fxaa::{Fxaa, FxaaPlugin};
use bevy::anti_alias::smaa::{Smaa, SmaaPlugin, SmaaPreset};
use bevy::prelude::*;
use bevy::render::view::SingleMainTexture;

use crate::view::WorldCamera;

/// The world view's post-process anti-aliasing.
#[derive(Resource, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PostAa {
    #[default]
    Off,
    Fxaa,
    Smaa(SmaaLevel),
}

/// [`SmaaPreset`] without its `Reflect`/`Hash` baggage, so [`PostAa`] stays `Eq` and `Debug`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SmaaLevel {
    Low,
    Medium,
    High,
    Ultra,
}

impl SmaaLevel {
    fn preset(self) -> SmaaPreset {
        match self {
            Self::Low => SmaaPreset::Low,
            Self::Medium => SmaaPreset::Medium,
            Self::High => SmaaPreset::High,
            Self::Ultra => SmaaPreset::Ultra,
        }
    }
}

impl PostAa {
    /// A `$WOW_POST_AA` spelling; `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "off" | "0" | "none" => Self::Off,
            "fxaa" => Self::Fxaa,
            "smaa-low" => Self::Smaa(SmaaLevel::Low),
            "smaa-medium" => Self::Smaa(SmaaLevel::Medium),
            "smaa" | "smaa-high" => Self::Smaa(SmaaLevel::High),
            "smaa-ultra" => Self::Smaa(SmaaLevel::Ultra),
            _ => return None,
        })
    }

    /// The CVar's spelling, as [`Self::parse`] reads it back.
    pub fn spelling(self) -> &'static str {
        match self {
            Self::Off => "0",
            Self::Fxaa => "fxaa",
            Self::Smaa(SmaaLevel::Low) => "smaa-low",
            Self::Smaa(SmaaLevel::Medium) => "smaa-medium",
            Self::Smaa(SmaaLevel::High) => "smaa-high",
            Self::Smaa(SmaaLevel::Ultra) => "smaa-ultra",
        }
    }

    fn from_env() -> Self {
        std::env::var("WOW_POST_AA")
            .ok()
            .and_then(|v| Self::parse(&v))
            .unwrap_or_default()
    }
}

/// A world camera that carried [`SingleMainTexture`] before a pass took it, so Off gives it back
/// (the free-fly viewer's camera never had it).
#[derive(Component)]
struct GaveUpSingleMain;

/// Puts the chosen pass on every world camera, new or when the choice moves.
fn apply_post_aa(
    mut commands: Commands,
    aa: Res<PostAa>,
    all: Query<(Entity, Has<SingleMainTexture>, Has<GaveUpSingleMain>), With<WorldCamera>>,
    added: Query<Entity, Added<WorldCamera>>,
) {
    if !aa.is_changed() && added.is_empty() {
        return;
    }
    for (cam, single, gave_up) in &all {
        if !aa.is_changed() && !added.contains(cam) {
            continue;
        }
        let mut e = commands.entity(cam);
        e.remove::<(Fxaa, Smaa)>();
        let pass = match *aa {
            PostAa::Off => {
                if gave_up {
                    e.remove::<GaveUpSingleMain>().insert(SingleMainTexture);
                }
                continue;
            }
            PostAa::Fxaa => e.insert(Fxaa::default()),
            PostAa::Smaa(level) => e.insert(Smaa {
                preset: level.preset(),
            }),
        };
        if single {
            pass.remove::<SingleMainTexture>().insert(GaveUpSingleMain);
        }
    }
}

/// FXAA and SMAA (bevy's `AntiAliasPlugin` stays off: no TAA, CAS) and the setting.
pub struct PostAaPlugin;

impl Plugin for PostAaPlugin {
    fn build(&self, app: &mut App) {
        let aa = PostAa::from_env();
        if aa != PostAa::Off {
            info!("post-process anti-aliasing: {aa:?}");
        }
        app.add_plugins((FxaaPlugin, SmaaPlugin))
            .insert_resource(aa)
            .add_systems(PostUpdate, apply_post_aa);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_spellings_name_each_pass() {
        assert_eq!(PostAa::parse("off"), Some(PostAa::Off));
        assert_eq!(PostAa::parse("FXAA"), Some(PostAa::Fxaa));
        assert_eq!(PostAa::parse("smaa"), Some(PostAa::Smaa(SmaaLevel::High)));
        assert_eq!(
            PostAa::parse("smaa-ultra"),
            Some(PostAa::Smaa(SmaaLevel::Ultra))
        );
        assert_eq!(PostAa::parse("taa"), None);
        for aa in [
            PostAa::Off,
            PostAa::Fxaa,
            PostAa::Smaa(SmaaLevel::Low),
            PostAa::Smaa(SmaaLevel::Medium),
            PostAa::Smaa(SmaaLevel::High),
            PostAa::Smaa(SmaaLevel::Ultra),
        ] {
            assert_eq!(PostAa::parse(aa.spelling()), Some(aa), "{aa:?}");
        }
    }
}
