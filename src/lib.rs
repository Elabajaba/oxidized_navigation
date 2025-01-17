//! Tiled **Runtime** Nav-mesh Generation for 3D worlds in [Bevy].
//!
//! Takes in [Bevy Rapier3D] colliders from entities with the [NavMeshAffector] component and **asynchronously** generates tiles of navigation meshes based on [NavMeshSettings]. Nav-meshes can then be queried using [query::find_path].
//!
//! ## Quick Start:
//! **Nav-mesh generation:**
//! 1. Add [OxidizedNavigationPlugin] as a plugin.
//! 2. Attach a [NavMeshAffector] component and a rapier collider to any entity you want to affect the nav-mesh.
//!
//! *At this point nav-meshes will be automatically generated whenever the collider or [GlobalTransform] of any entity with a [NavMeshAffector] is changed.*
//!
//! **Querying the nav-mesh / Pathfinding:**
//! 1. Your system needs to take in the [NavMesh] resource.
//! 2. Get the underlying data from the nav-mesh using [NavMesh::get]. This data is wrapped in an [RwLock].
//! 3. To access the data call [RwLock::read]. *This will block until you get read acces on the lock. If a task is already writing to the lock it may take time.*
//! 4. Call [query::find_path] with the [NavMeshTiles] returned from the [RwLock].
//!
//! *Also see the [examples] for how to run pathfinding in an async task which may be preferable.*
//!
//! [Bevy]: https://crates.io/crates/bevy
//! [Bevy Rapier3D]: https://crates.io/crates/bevy_rapier3d
//! [examples]: https://github.com/TheGrimsey/oxidized_navigation/blob/master/examples

use std::sync::{Arc, RwLock};

use bevy::tasks::{AsyncComputeTaskPool, Task};
use bevy::{
    ecs::system::Resource,
    prelude::*,
    utils::{HashMap, HashSet},
};
use bevy_rapier3d::prelude::ColliderView;
use bevy_rapier3d::rapier::prelude::HeightField;
use bevy_rapier3d::{na::Vector3, prelude::Collider, rapier::prelude::Isometry};
use contour::build_contours;
use conversion::{GeometryToConvert, ColliderType, convert_geometry_collections, GeometryCollection};
use heightfields::{
    build_heightfield_tile, build_open_heightfield_tile, calculate_distance_field,
    erode_walkable_area, HeightFieldCollection,
};
use mesher::build_poly_mesh;
use regions::build_regions;
use smallvec::SmallVec;
use tiles::{create_nav_mesh_tile_from_poly_mesh, NavMeshTiles};

mod conversion;
mod contour;
mod heightfields;
mod mesher;
pub mod query;
mod regions;
pub mod tiles;

/// System sets containing the crate's systems.
#[derive(SystemSet, Debug, PartialEq, Eq, Hash, Clone)]
pub enum OxidizedNavigation {
    /// Systems handling dirty marking when a NavMeshAffector component is removed.
    /// Separated to make sure that even if Main is throttled the removal events will be caught.
    RemovedComponent,
    /// Main systems, this creates the tile generation tasks & handles reacting to NavMeshAffector changes.
    Main,
}

pub struct OxidizedNavigationPlugin {
    pub settings: NavMeshSettings
}

impl Plugin for OxidizedNavigationPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.settings.clone());

        app.init_resource::<TileAffectors>()
            .init_resource::<DirtyTiles>()
            .init_resource::<NavMesh>()
            .init_resource::<GenerationTicker>()
            .init_resource::<NavMeshAffectorRelations>()
            .init_resource::<ActiveGenerationTasks>();

        app.add_system(
            handle_removed_affectors_system
                .before(send_tile_rebuild_tasks_system)
                .in_set(OxidizedNavigation::RemovedComponent)
        );

        app.add_system(
            remove_finished_tasks.in_set(OxidizedNavigation::Main).before(send_tile_rebuild_tasks_system),
        );

        app.add_systems(
            (
                update_navmesh_affectors_system,
                send_tile_rebuild_tasks_system.run_if(can_generate_new_tiles),
            )
                .chain()
                .in_set(OxidizedNavigation::Main),
        );
    }
}

const FLAG_BORDER_VERTEX: u32 = 0x10000;
const MASK_CONTOUR_REGION: u32 = 0xffff; // Masks out the above value.

#[derive(Resource, Default)]
struct NavMeshAffectorRelations(HashMap<Entity, SmallVec<[UVec2; 4]>>);

#[derive(Resource, Default)]
struct ActiveGenerationTasks(Vec<Task<()>>);

/// Component for entities that should affect the nav-mesh.
#[derive(Component)]
pub struct NavMeshAffector;

/// Optional component to define the area type of an entity. Setting this to ``None`` means that the entity isn't walkable.
///
/// Any part of the nav-mesh generated from this entity will have this area type. Overlapping areas will prefer the higher area type.
#[derive(Component)]
pub struct NavMeshAreaType(Option<u16>);

/*
*   Neighbours:
*   0: (-1, 0),
*   1: (0, 1),
*   2: (1, 0),
*   3: (0, -1)
*/

/// Generation ticker for tiles.
///
/// Used to keep track of if the existing tile is newer than the one we are trying to insert in [build_tile]. This could happen if we go from having a lot of triangles to very few.
#[derive(Default, Resource)]
struct GenerationTicker(u64);

#[derive(Default, Resource, Deref, DerefMut)]
struct TileAffectors(HashMap<UVec2, HashSet<Entity>>);

/// Set of all tiles that need to be rebuilt.
#[derive(Default, Resource)]
struct DirtyTiles(HashSet<UVec2>);

/// Settings for nav-mesh generation.
#[derive(Resource, Clone)]
pub struct NavMeshSettings {
    /// The horizontal resolution of the voxelized tile.
    ///
    /// **Suggested value**: 1/2 of character radius.
    ///
    /// Smaller values will increase tile generation times with diminishing returns in nav-mesh detail.
    pub cell_width: f32,
    /// The vertical resolution of the voxelized tile.
    ///
    /// **Suggested value**: 1/2 of cell_width.
    ///
    /// Smaller values will increase tile generation times with diminishing returns in nav-mesh detail.
    pub cell_height: f32,

    /// Length of a tile's side in cells. Resulting size in world units is ``tile_width * cell_width``.
    ///
    /// **Suggested value**: ???
    ///
    /// Higher means more to update each time something within the tile changes, smaller means you will have more overhead from connecting the edges to other tiles & generating the tile itself.
    pub tile_width: u16,

    /// Extents of the world as measured from the world origin (0.0, 0.0) on the XZ-plane.
    ///
    /// **Suggested value**: As small as possible whilst still keeping the entire world within it.
    ///
    /// This exists because figuring out which tile we are in around the world origin would not work without it.
    pub world_half_extents: f32,
    /// Bottom extents of the world on the Y-axis. The top extents is capped by ``world_bottom_bound + cell_height * u16::MAX``.
    ///
    /// **Suggested value**: Minium Y position of anything in the world that should be covered by the nav mesh.
    pub world_bottom_bound: f32,

    /// Maximum incline/slope traversable when navigating in radians.
    pub max_traversable_slope_radians: f32,
    /// Minimum open height for an area to be considered walkable in cell_height(s).
    ///
    /// **Suggested value**: The height of character * ``cell_height``, rounded up.
    pub walkable_height: u16,
    /// This will "pull-back" the nav-mesh from edges, meaning anywhere on the nav-mesh will be walkable for a character with a radius of ``walkable_radius * cell_width``.
    ///
    /// **Suggested value**: ``ceil(character_radius / cell_width)`` (2-3 if `cell_width`` is 1/2 of ``character_radius``)  
    pub walkable_radius: u16,
    /// Maximum height difference that is still considered traversable in cell_height(s). (Think, stair steps)
    pub step_height: u16,

    /// Minimum size of a region, anything smaller than this will be removed. This is used to filter out smaller regions that might appear on tables.
    pub min_region_area: usize,
    /// Maximum size of a region to merge other regions into.
    pub merge_region_area: usize,

    /// Maximum length of an edge before it's split.
    ///
    /// **Suggested value**: Start high and reduce if there are issues.
    pub max_edge_length: u32,
    /// Maximum difference allowed for simplified contour generation on the XZ-plane in cell_width(s).
    ///
    /// **Suggested value range**: [1.1, 1.5]
    pub max_contour_simplification_error: f32,

    /// Optional max tiles to generate at once. A value of ``None`` will result in no limit.
    /// 
    /// Adjust this to control memory & CPU usage. More tiles generating at once will have a higher memory footprint.
    pub max_tile_generation_tasks: Option<u16>,
}
impl NavMeshSettings {
    /// Returns the length of a tile's side in world units.
    #[inline]
    pub fn get_tile_size(&self) -> f32 {
        self.cell_width * f32::from(self.tile_width)
    }
    #[inline]
    pub fn get_border_size(&self) -> f32 {
        f32::from(self.walkable_radius) * self.cell_width
    }

    /// Returns the tile coordinate that contains the supplied ``world_position``.
    #[inline]
    pub fn get_tile_containing_position(&self, world_position: Vec2) -> UVec2 {
        let offset_world = world_position + self.world_half_extents;

        (offset_world / self.get_tile_size()).as_uvec2()
    }

    /// Returns the minimum bound of a tile on the XZ-plane.
    #[inline]
    pub fn get_tile_origin(&self, tile: UVec2) -> Vec2 {
        tile.as_vec2() * self.get_tile_size() - self.world_half_extents
    }

    /// Returns the origin of a tile on the XZ-plane including the border area.
    #[inline]
    pub fn get_tile_origin_with_border(&self, tile: UVec2) -> Vec2 {
        self.get_tile_origin(tile) - self.get_border_size()
    }

    #[inline]
    pub fn get_tile_side_with_border(&self) -> usize {
        usize::from(self.tile_width) + usize::from(self.walkable_radius) * 2
    }
    #[inline]
    pub fn get_border_side(&self) -> usize {
        // Not technically useful currently but in case.
        self.walkable_radius.into()
    }

    /// Returns the minimum & maximum bound of a tile on the XZ-plane.
    #[inline]
    pub fn get_tile_bounds(&self, tile: UVec2) -> (Vec2, Vec2) {
        let tile_size = self.get_tile_size();

        let min_bound = tile.as_vec2() * tile_size - self.world_half_extents;
        let max_bound = min_bound + tile_size;

        (min_bound, max_bound)
    }
}

/// Wrapper around the nav-mesh data.
///
/// The underlying [NavMeshTiles] must be retrieved using [NavMesh::get]
#[derive(Default, Resource)]
pub struct NavMesh(Arc<RwLock<NavMeshTiles>>);

impl NavMesh {
    pub fn get(&self) -> Arc<RwLock<NavMeshTiles>> {
        self.0.clone()
    }
}

fn update_navmesh_affectors_system(
    nav_mesh_settings: Res<NavMeshSettings>,
    mut tile_affectors: ResMut<TileAffectors>,
    mut affector_relations: ResMut<NavMeshAffectorRelations>,
    mut dirty_tiles: ResMut<DirtyTiles>,
    mut query: Query<
        (Entity, &Collider, &GlobalTransform),
        (Or<(Changed<GlobalTransform>, Changed<Collider>, Changed<NavMeshAffector>)>, With<NavMeshAffector>)
    >,
) {
    // Expand by 2 * walkable_radius to match with erode_walkable_area.
    let border_expansion =
        f32::from(nav_mesh_settings.walkable_radius * 2) * nav_mesh_settings.cell_width;
    
    query.for_each_mut(|(e, collider, global_transform)| {
        let transform = global_transform.compute_transform();
        let iso = Isometry::new(
            transform.translation.into(),
            transform.rotation.to_scaled_axis().into(),
        );
        let local_aabb = collider.raw.compute_local_aabb();
        let aabb = local_aabb
            .scaled(&Vector3::new(
                transform.scale.x,
                transform.scale.y,
                transform.scale.z,
            ))
            .transform_by(&iso);

        let min_vec = Vec2::new(
            aabb.mins.x - border_expansion,
            aabb.mins.z - border_expansion,
        );
        let min_tile = nav_mesh_settings.get_tile_containing_position(min_vec);

        let max_vec = Vec2::new(
            aabb.maxs.x + border_expansion,
            aabb.maxs.z + border_expansion,
        );
        let max_tile = nav_mesh_settings.get_tile_containing_position(max_vec);

        let relation = if let Some(relation) = affector_relations.0.get_mut(&e) {
            // Remove from previous.
            for old_tile in relation.iter().filter(|tile_coord| {
                min_tile.x > tile_coord.x
                    || min_tile.y > tile_coord.y
                    || max_tile.x < tile_coord.x
                    || max_tile.y < tile_coord.y
            }) {
                if let Some(affectors) = tile_affectors.get_mut(old_tile) {
                    affectors.remove(&e);
                    dirty_tiles.0.insert(*old_tile);
                }
            }
            relation.clear();

            relation
        } else {
            affector_relations.0.insert_unique_unchecked(e, SmallVec::default()).1
        };
        
        for x in min_tile.x..=max_tile.x {
            for y in min_tile.y..=max_tile.y {
                let tile_coord = UVec2::new(x, y);

                let affectors = if let Some(affectors) = tile_affectors.get_mut(&tile_coord) {
                    affectors
                } else {
                    tile_affectors.insert_unique_unchecked(tile_coord, HashSet::default()).1
                };
                affectors.insert(e);

                relation.push(tile_coord);
                dirty_tiles.0.insert(tile_coord);
            }
        }
    });
}

fn handle_removed_affectors_system(
    mut removed_affectors: RemovedComponents<NavMeshAffector>,
    mut affector_relations: ResMut<NavMeshAffectorRelations>,
    mut dirty_tiles: ResMut<DirtyTiles>,
) {
    for relations in removed_affectors.iter().filter_map(|removed| affector_relations.0.remove(&removed)) {
        for tile in relations {
            dirty_tiles.0.insert(tile);
        }
    }
}

fn can_generate_new_tiles(
    active_generation_tasks: Res<ActiveGenerationTasks>,
    dirty_tiles: Res<DirtyTiles>,
    nav_mesh_settings: Res<NavMeshSettings>,
) -> bool {
    nav_mesh_settings.max_tile_generation_tasks.map_or(true, |max_tile_generation_tasks| active_generation_tasks.0.len() < max_tile_generation_tasks.into())
        && !dirty_tiles.0.is_empty()
}

fn send_tile_rebuild_tasks_system(
    mut active_generation_tasks: ResMut<ActiveGenerationTasks>,
    mut generation_ticker: ResMut<GenerationTicker>,
    mut dirty_tiles: ResMut<DirtyTiles>,
    mut tiles_to_generate: Local<Vec<UVec2>>,
    mut heightfields: Local<HashMap<Entity, Arc<HeightField>>>,
    nav_mesh_settings: Res<NavMeshSettings>,
    nav_mesh: Res<NavMesh>,
    tile_affectors: Res<TileAffectors>,
    collider_query: Query<
        (Entity, &Collider, &GlobalTransform, Option<&NavMeshAreaType>),
        With<NavMeshAffector>,
    >,
) {
    let thread_pool = AsyncComputeTaskPool::get();
    
    let max_task_count = nav_mesh_settings.max_tile_generation_tasks.unwrap_or(u16::MAX) as usize - active_generation_tasks.0.len();
    tiles_to_generate.extend(dirty_tiles.0.iter().take(max_task_count));
    
    for tile_coord in tiles_to_generate.drain(..) {
        dirty_tiles.0.remove(&tile_coord);

        generation_ticker.0 += 1;

        let Some(affectors) = tile_affectors.get(&tile_coord) else {
            // Spawn task to remove tile.
            thread_pool.spawn(remove_tile(generation_ticker.0, tile_coord, nav_mesh.0.clone())).detach();
            continue;
        };
        if affectors.is_empty() {
            // Spawn task to remove tile.
            thread_pool
                .spawn(remove_tile(
                    generation_ticker.0,
                    tile_coord,
                    nav_mesh.0.clone(),
                ))
                .detach();
            continue;
        }

        // Step 1: Gather data.
        let mut geometry_collections = Vec::with_capacity(affectors.len());
        // Storing heightfields separately because they are massive.
        let mut heightfield_collections = Vec::new();

        let mut collider_iter = collider_query.iter_many(affectors.iter());
        while let Some((entity, collider, global_transform, nav_mesh_affector)) = collider_iter.fetch_next() {
            let area = nav_mesh_affector.map_or(Some(0), |area_type| area_type.0);

            let type_to_convert = match collider.as_typed_shape() {
                ColliderView::Ball(ball) => GeometryToConvert::Collider(ColliderType::Ball(*ball.raw)),
                ColliderView::Cuboid(cuboid) => GeometryToConvert::Collider(ColliderType::Cuboid(*cuboid.raw)),
                ColliderView::Capsule(capsule) => GeometryToConvert::Collider(ColliderType::Capsule(*capsule.raw)),
                ColliderView::TriMesh(trimesh) => GeometryToConvert::RapierTriMesh(trimesh.raw.vertices().to_vec(), trimesh.indices().to_vec()),
                ColliderView::HeightField(heightfield) => {
                    // Deduplicate heightfields.
                    let heightfield = if let Some(heightfield) = heightfields.get(&entity) {
                        heightfield.clone()
                    } else {
                        let heightfield = Arc::new(heightfield.raw.clone());

                        heightfields.insert(entity, heightfield.clone());

                        heightfield
                    };

                    heightfield_collections.push(HeightFieldCollection {
                        transform: global_transform.compute_transform(),
                        heightfield,
                        area,
                    });

                    continue;
                },
                ColliderView::ConvexPolyhedron(polyhedron) => {
                    let tri = polyhedron.raw.to_trimesh();

                    GeometryToConvert::RapierTriMesh(tri.0, tri.1)
                },
                ColliderView::Cylinder(cylinder) => GeometryToConvert::Collider(ColliderType::Cylinder(*cylinder.raw)),
                ColliderView::Cone(cone) => GeometryToConvert::Collider(ColliderType::Cone(*cone.raw)),
                ColliderView::RoundCuboid(round_cuboid) => GeometryToConvert::Collider(ColliderType::Cuboid(round_cuboid.raw.inner_shape)),
                ColliderView::RoundCylinder(round_cylinder) => GeometryToConvert::Collider(ColliderType::Cylinder(round_cylinder.raw.inner_shape)),
                ColliderView::RoundCone(round_cone) => GeometryToConvert::Collider(ColliderType::Cone(round_cone.raw.inner_shape)),
                ColliderView::RoundConvexPolyhedron(round_polyhedron) => {
                    let tri = round_polyhedron.inner_shape().raw.to_trimesh();

                    GeometryToConvert::RapierTriMesh(tri.0, tri.1)
                }
                ColliderView::Triangle(triangle) => GeometryToConvert::Collider(ColliderType::Triangle(*triangle.raw)),
                ColliderView::RoundTriangle(triangle) => {
                    let inner_shape = triangle.inner_shape();

                    GeometryToConvert::Collider(ColliderType::Triangle(*inner_shape.raw))
                }
                // TODO: This one requires me to think.
                ColliderView::Compound(_) => {
                    warn!("Compound colliders are not yet supported for nav-mesh generation, skipping for now..");
                    continue;
                }
                // These ones do not make sense in this.
                ColliderView::HalfSpace(_) => continue, /* This is like an infinite plane? We don't care. */
                ColliderView::Polyline(_) => continue,  /* This is a line. */
                ColliderView::Segment(_) => continue,   /* This is a line segment. */
            };

            geometry_collections.push(GeometryCollection {
                transform: global_transform.compute_transform(),
                geometry_to_convert: type_to_convert,
                area,
            });
        }

        // Step 2: Acquire nav_mesh lock
        let nav_mesh = nav_mesh.0.clone();

        // Step 3: Make it a task.
        let task = thread_pool.spawn(build_tile(
            generation_ticker.0,
            tile_coord,
            nav_mesh_settings.clone(),
            geometry_collections,
            heightfield_collections,
            nav_mesh,
        ));

        active_generation_tasks.0.push(task);
    }
    heightfields.clear();
}

fn remove_finished_tasks(
    mut active_generation_tasks: ResMut<ActiveGenerationTasks> 
) {
    active_generation_tasks.0.retain(|task| !task.is_finished());
}

async fn remove_tile(
    generation: u64, // This is the max generation we remove. Should we somehow strangely be executing this after a new tile has arrived we won't remove it.
    tile_coord: UVec2,
    nav_mesh: Arc<RwLock<NavMeshTiles>>,
) {
    let Ok(mut nav_mesh) = nav_mesh.write() else {
        error!("Nav-Mesh lock has been poisoned. Generation can no longer be continued.");
        return;
    };

    if nav_mesh.tile_generations.get(&tile_coord).unwrap_or(&0) < &generation {
        nav_mesh.tile_generations.insert(tile_coord, generation);
        nav_mesh.remove_tile(tile_coord);
    }
}
async fn build_tile(
    generation: u64,
    tile_coord: UVec2,
    nav_mesh_settings: NavMeshSettings,
    geometry_collections: Vec<GeometryCollection>,
    heightfields: Vec<HeightFieldCollection>,
    nav_mesh: Arc<RwLock<NavMeshTiles>>,
) {
    let triangle_collection = convert_geometry_collections(geometry_collections);

    let voxelized_tile =
        build_heightfield_tile(tile_coord, triangle_collection, heightfields, &nav_mesh_settings);

    let mut open_tile = build_open_heightfield_tile(voxelized_tile, &nav_mesh_settings);

    // Remove areas that are too close to a wall.
    erode_walkable_area(&mut open_tile, &nav_mesh_settings);

    calculate_distance_field(&mut open_tile, &nav_mesh_settings);
    build_regions(&mut open_tile, &nav_mesh_settings);

    let contour_set = build_contours(open_tile, &nav_mesh_settings);

    let poly_mesh = build_poly_mesh(contour_set, &nav_mesh_settings);

    let nav_mesh_tile =
        create_nav_mesh_tile_from_poly_mesh(poly_mesh, tile_coord, &nav_mesh_settings);

    let Ok(mut nav_mesh) = nav_mesh.write() else {
        error!("Nav-Mesh lock has been poisoned. Generation can no longer be continued.");
        return;
    };

    if nav_mesh.tile_generations.get(&tile_coord).unwrap_or(&0) < &generation {
        nav_mesh.tile_generations.insert(tile_coord, generation);

        nav_mesh.add_tile(tile_coord, nav_mesh_tile, &nav_mesh_settings);
    }
}

/*
*   Lots of math stuff.
*   Don't know where else to put it.
*/

fn get_neighbour_index(nav_mesh_settings: &NavMeshSettings, index: usize, dir: usize) -> usize {
    match dir {
        0 => index - 1,
        1 => index + nav_mesh_settings.get_tile_side_with_border(),
        2 => index + 1,
        3 => index - nav_mesh_settings.get_tile_side_with_border(),
        _ => panic!("Not a valid direction"),
    }
}

fn intersect_prop(a: IVec4, b: IVec4, c: IVec4, d: IVec4) -> bool {
    if collinear(a, b, c) || collinear(a, b, d) || collinear(c, d, a) || collinear(c, d, b) {
        return false;
    }

    (left(a, b, c) ^ left(a, b, d)) && (left(c, d, a) ^ left(c, d, b))
}

fn between(a: IVec4, b: IVec4, c: IVec4) -> bool {
    if !collinear(a, b, c) {
        return false;
    }

    if a.x != b.x {
        return (a.x <= c.x && c.x <= b.x) || (a.x >= c.x && c.x >= b.x);
    }

    (a.z <= c.z && c.z <= b.z) || (a.z >= c.z && c.z >= b.z)
}

fn intersect(a: IVec4, b: IVec4, c: IVec4, d: IVec4) -> bool {
    intersect_prop(a, b, c, d)
        || between(a, b, c)
        || between(a, b, d)
        || between(c, d, a)
        || between(c, d, b)
}

fn area_sqr(a: IVec4, b: IVec4, c: IVec4) -> i32 {
    (b.x - a.x) * (c.z - a.z) - (c.x - a.x) * (b.z - a.z)
}

fn collinear(a: IVec4, b: IVec4, c: IVec4) -> bool {
    area_sqr(a, b, c) == 0
}

fn left(a: IVec4, b: IVec4, c: IVec4) -> bool {
    area_sqr(a, b, c) < 0
}
fn left_on(a: IVec4, b: IVec4, c: IVec4) -> bool {
    area_sqr(a, b, c) <= 0
}

fn in_cone(i: usize, outline_vertices: &[UVec4], point: UVec4) -> bool {
    let point_i = outline_vertices[i];
    let point_next = outline_vertices[(i + 1) % outline_vertices.len()];
    let point_previous =
        outline_vertices[(outline_vertices.len() + i - 1) % outline_vertices.len()];

    if left_on(point_i.as_ivec4(), point.as_ivec4(), point_next.as_ivec4()) {
        return left(
            point_i.as_ivec4(),
            point.as_ivec4(),
            point_previous.as_ivec4(),
        ) && left(point.as_ivec4(), point_i.as_ivec4(), point_next.as_ivec4());
    }

    !left_on(point_i.as_ivec4(), point.as_ivec4(), point_next.as_ivec4())
        && left_on(
            point.as_ivec4(),
            point_i.as_ivec4(),
            point_previous.as_ivec4(),
        )
}
