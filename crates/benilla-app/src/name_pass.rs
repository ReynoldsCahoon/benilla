//! Overhead names drawn at the window's resolution in the player-UI pass, not in the world pass,
//! whose resolution `renderScale` and the dynamic scale lower: a name stays legible at any scale.
//!
//! - The block is the world name's glyph mesh ([`crate::nameplates`]), seated each frame on screen
//!   at the anchor's projection and sized by the anchor's pixels per world unit, so a name shrinks
//!   with distance as the reference's world-space batch does. Below every interface quad (the
//!   batches span z −450 to 450) and over the world backdrop, the pass's first draw.
//! - Depth-tested like the reference's name batch (`0x6c7470` calls `0x5c1d60(1,1)`): the world
//!   camera's depth is resolved at the end of its main pass into [`WorldDepth`], an image at the
//!   world's render size, and `name_text.wgsl` drops a fragment the world puts nearer than the
//!   anchor, whose depth rides the plate's `MeshTag`.
//!
//! Deviation: each name tests at its anchor's one depth, where the reference's camera-facing quad
//! tests per pixel; the block stands upright at the anchor, so the two differ only by its height.

use bevy::core_pipeline::core_3d::graph::{Core3d, Node3d};
use bevy::core_pipeline::FullscreenShader;
use bevy::ecs::query::QueryItem;
use bevy::prelude::*;
use bevy::render::extract_component::{ExtractComponent, ExtractComponentPlugin};
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_graph::{
    NodeRunError, RenderGraphContext, RenderGraphExt, RenderLabel, ViewNode, ViewNodeRunner,
};
use bevy::render::render_resource::binding_types::{
    texture_depth_2d, texture_depth_2d_multisampled,
};
use bevy::render::render_resource::*;
use bevy::render::renderer::RenderContext;
use bevy::render::texture::GpuImage;
use bevy::render::view::ViewDepthTexture;
use bevy::render::{Render, RenderApp, RenderStartup, RenderSystems};
use bevy::shader::ShaderRef;
use bevy::sprite_render::{Material2d, Material2dKey, Material2dPlugin};

use benilla_world::view::WorldCamera;

/// The plate's z in the player-UI camera: under every interface batch (−450 to 450).
pub(crate) const NAME_Z: f32 = -500.0;

/// The world camera's resolved depth, an `R32Float` image at the world's render size, which the
/// names' material samples. Resized with the world target.
#[derive(Resource)]
pub(crate) struct WorldDepth {
    pub(crate) image: Handle<Image>,
    size: UVec2,
}

/// On the world camera: the image its depth resolves into.
#[derive(Component, Clone, ExtractComponent)]
struct WorldDepthTarget(Handle<Image>);

/// A name's paint: the line colour and the glyph sheet, with the world depth to test against.
#[derive(Asset, TypePath, AsBindGroup, Clone)]
pub(crate) struct NameMaterial {
    /// The paint as gamma bytes over 255, the value the world pass wrote raw.
    #[uniform(0)]
    pub(crate) color: Vec4,
    #[texture(1)]
    #[sampler(2)]
    pub(crate) glyphs: Handle<Image>,
    #[texture(3, sample_type = "float", filterable = false)]
    pub(crate) world_depth: Handle<Image>,
}

impl Material2d for NameMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://benilla_app/shaders/name_text.wgsl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "embedded://benilla_app/shaders/name_text.wgsl".into()
    }

    fn alpha_mode(&self) -> bevy::sprite_render::AlphaMode2d {
        bevy::sprite_render::AlphaMode2d::Blend
    }

    fn specialize(
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &bevy::mesh::MeshVertexBufferLayoutRef,
        _key: Material2dKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        // The UI pass's premultiplied gamma blend (`ui_quad.wgsl`).
        if let Some(target) = descriptor
            .fragment
            .as_mut()
            .and_then(|f| f.targets.first_mut())
            .and_then(|t| t.as_mut())
        {
            let premultiplied = BlendComponent {
                src_factor: BlendFactor::One,
                dst_factor: BlendFactor::OneMinusSrcAlpha,
                operation: BlendOperation::Add,
            };
            target.blend = Some(BlendState {
                color: premultiplied,
                alpha: premultiplied,
            });
        }
        Ok(())
    }
}

/// A depth image of `size` physical px, cleared to the far plane (reverse Z's 0).
fn new_depth_image(size: UVec2) -> Image {
    let mut image = Image::new_fill(
        Extent3d {
            width: size.x.max(1),
            height: size.y.max(1),
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        &0f32.to_le_bytes(),
        TextureFormat::R32Float,
        bevy::asset::RenderAssetUsages::RENDER_WORLD,
    );
    image.texture_descriptor.usage =
        TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT;
    image
}

fn setup_world_depth(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    commands.insert_resource(WorldDepth {
        image: images.add(new_depth_image(UVec2::ONE)),
        size: UVec2::ONE,
    });
}

/// Keep [`WorldDepth`] at the world's render size and on the world camera. A new size is a new
/// image, so the names' materials rebind to it.
fn track_world_depth(
    mut commands: Commands,
    backdrop: Res<crate::world_backdrop::WorldBackdrop>,
    mut depth: ResMut<WorldDepth>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<NameMaterial>>,
    cameras: Query<(Entity, Option<&WorldDepthTarget>), With<WorldCamera>>,
) {
    let size = backdrop.render_size();
    if size != depth.size && size.x > 0 && size.y > 0 {
        let old = std::mem::replace(&mut depth.image, images.add(new_depth_image(size)));
        images.remove(&old);
        depth.size = size;
        for (_, material) in materials.iter_mut() {
            material.world_depth = depth.image.clone();
        }
    }
    for (camera, target) in &cameras {
        if target.is_none_or(|t| t.0 != depth.image) {
            commands
                .entity(camera)
                .insert(WorldDepthTarget(depth.image.clone()));
        }
    }
}

#[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)]
struct NameDepthLabel;

/// The resolve's layouts and shader, one pipeline per depth sample count.
#[derive(Resource)]
struct NameDepthPipeline {
    single: BindGroupLayoutDescriptor,
    multi: BindGroupLayoutDescriptor,
    shader: Handle<Shader>,
    fullscreen: FullscreenShader,
}

impl SpecializedRenderPipeline for NameDepthPipeline {
    /// The world depth's sample count.
    type Key = u32;

    fn specialize(&self, samples: u32) -> RenderPipelineDescriptor {
        let multi = samples > 1;
        RenderPipelineDescriptor {
            label: Some("name_depth_resolve".into()),
            layout: vec![if multi {
                self.multi.clone()
            } else {
                self.single.clone()
            }],
            vertex: self.fullscreen.to_vertex_state(),
            fragment: Some(FragmentState {
                shader: self.shader.clone(),
                shader_defs: if multi {
                    vec!["MULTISAMPLED".into()]
                } else {
                    vec![]
                },
                entry_point: Some("fragment".into()),
                targets: vec![Some(ColorTargetState {
                    format: TextureFormat::R32Float,
                    blend: None,
                    write_mask: ColorWrites::ALL,
                })],
            }),
            ..default()
        }
    }
}

fn init_depth_pipeline(
    mut commands: Commands,
    fullscreen: Res<FullscreenShader>,
    asset_server: Res<AssetServer>,
) {
    let layout = |label, entry| {
        BindGroupLayoutDescriptor::new(
            label,
            &BindGroupLayoutEntries::single(ShaderStages::FRAGMENT, entry),
        )
    };
    commands.insert_resource(NameDepthPipeline {
        single: layout("name_depth_single", texture_depth_2d()),
        multi: layout("name_depth_multi", texture_depth_2d_multisampled()),
        shader: asset_server.load("embedded://benilla_app/shaders/name_depth_resolve.wgsl"),
        fullscreen: fullscreen.clone(),
    });
}

/// The resolve pipeline this view's depth needs.
#[derive(Component)]
struct NameDepthResolve(CachedRenderPipelineId);

fn prepare_depth_pipelines(
    mut commands: Commands,
    pipeline_cache: Res<PipelineCache>,
    pipeline: Res<NameDepthPipeline>,
    mut pipelines: ResMut<SpecializedRenderPipelines<NameDepthPipeline>>,
    views: Query<(Entity, &Msaa), With<WorldDepthTarget>>,
) {
    for (view, msaa) in &views {
        let id = pipelines.specialize(&pipeline_cache, &pipeline, msaa.samples());
        commands.entity(view).insert(NameDepthResolve(id));
    }
}

/// After the world's main pass: its depth into [`WorldDepth`].
#[derive(Default)]
struct NameDepthNode;

impl ViewNode for NameDepthNode {
    type ViewQuery = (
        &'static ViewDepthTexture,
        &'static WorldDepthTarget,
        &'static NameDepthResolve,
        &'static Msaa,
    );

    fn run<'w>(
        &self,
        _graph: &mut RenderGraphContext,
        render_context: &mut RenderContext<'w>,
        (depth, target, resolve, msaa): QueryItem<'w, '_, Self::ViewQuery>,
        world: &'w World,
    ) -> Result<(), NodeRunError> {
        let pipeline_cache = world.resource::<PipelineCache>();
        let (Some(render_pipeline), Some(image)) = (
            pipeline_cache.get_render_pipeline(resolve.0),
            world.resource::<RenderAssets<GpuImage>>().get(&target.0),
        ) else {
            return Ok(());
        };
        // The image follows the world's size a frame behind a resize; skip the mismatched frame.
        if image.size_2d() != UVec2::new(depth.texture.width(), depth.texture.height()) {
            return Ok(());
        }
        let pipeline = world.resource::<NameDepthPipeline>();
        let layout = if msaa.samples() > 1 {
            &pipeline.multi
        } else {
            &pipeline.single
        };
        let bind = render_context.render_device().create_bind_group(
            "name_depth_resolve",
            &pipeline_cache.get_bind_group_layout(layout),
            &BindGroupEntries::single(depth.view()),
        );
        let mut pass = render_context.begin_tracked_render_pass(RenderPassDescriptor {
            label: Some("name_depth_resolve"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: &image.texture_view,
                resolve_target: None,
                ops: Operations {
                    load: LoadOp::Clear(Default::default()),
                    store: StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_render_pipeline(render_pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.draw(0..3, 0..1);
        Ok(())
    }
}

/// The names' material and the world depth that feeds it.
pub(crate) struct NamePassPlugin;

impl Plugin for NamePassPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            Material2dPlugin::<NameMaterial>::default(),
            ExtractComponentPlugin::<WorldDepthTarget>::default(),
        ))
        .add_systems(Startup, setup_world_depth)
        .add_systems(PostUpdate, track_world_depth);
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app
            .init_resource::<SpecializedRenderPipelines<NameDepthPipeline>>()
            .add_systems(RenderStartup, init_depth_pipeline)
            .add_systems(
                Render,
                prepare_depth_pipelines.in_set(RenderSystems::PrepareResources),
            )
            .add_render_graph_node::<ViewNodeRunner<NameDepthNode>>(Core3d, NameDepthLabel)
            .add_render_graph_edges(
                Core3d,
                (
                    Node3d::EndMainPass,
                    NameDepthLabel,
                    Node3d::StartMainPassPostProcessing,
                ),
            );
    }
}
