//! Nav-mesh set up example for both blocking & async pathfinding.
//!
//! Press A to run async path finding.
//!
//! Press B to run blocking path finding.
//!

use std::sync::{Arc, RwLock};

use bevy::{
    prelude::*,
    tasks::{AsyncComputeTaskPool, Task},
    DefaultPlugins,
};
// use bevy_editor_pls::EditorPlugin;
use bevy_rapier3d::prelude::{Collider, NoUserData, RapierConfiguration, RapierPhysicsPlugin};
use futures_lite::future;
use oxidized_navigation::{
    query::{find_path, find_polygon_path, perform_string_pulling_on_path},
    tiles::NavMeshTiles,
    NavMesh, NavMeshAffector, NavMeshSettings, OxidizedNavigationPlugin,
};

fn main() {
    App::new()
        // Default Plugins
        .add_plugins((
            DefaultPlugins,
            OxidizedNavigationPlugin {
                settings: NavMeshSettings {
                    cell_width: 0.25,
                    cell_height: 0.1,
                    tile_width: 100,
                    world_half_extents: 250.0,
                    world_bottom_bound: -100.0,
                    max_traversable_slope_radians: (40.0_f32 - 0.1).to_radians(),
                    walkable_height: 20,
                    walkable_radius: 1,
                    step_height: 3,
                    min_region_area: 100,
                    merge_region_area: 500,
                    max_contour_simplification_error: 1.1,
                    max_edge_length: 80,
                    max_tile_generation_tasks: Some(9),
                },
            },
            // The rapier plugin needs to be added for the scales of colliders to be correct if the scale of the entity is not uniformly 1.
            // An example of this is the "Thin Wall" in [setup_world_system]. If you remove this plugin, it will not appear correctly.
            RapierPhysicsPlugin::<NoUserData>::default(),
            // EditorPlugin::default(),
        ))
        .insert_resource(RapierConfiguration {
            physics_pipeline_active: false,
            ..Default::default()
        })
        .insert_resource(AsyncPathfindingTasks::default())
        .insert_resource(DrawNavMesh(false))
        .add_systems(Startup, (setup_world_system, info_system))
        .add_systems(
            Update,
            (
                run_blocking_pathfinding,
                run_async_pathfinding,
                poll_pathfinding_tasks_system,
                draw_nav_mesh_system,
                spawn_or_despawn_affector_system,
            ),
        )
        .add_systems(PostUpdate, draw_path)
        .run();
}

//
//  Blocking Pathfinding.
//  Press B to run.
//
//  Running pathfinding in a system.
//
fn run_blocking_pathfinding(
    mut commands: Commands,
    keys: Res<Input<KeyCode>>,
    nav_mesh_settings: Res<NavMeshSettings>,
    nav_mesh: Res<NavMesh>,
) {
    if !keys.just_pressed(KeyCode::B) {
        return;
    }

    // Get the underlying nav_mesh.
    if let Ok(nav_mesh) = nav_mesh.get().read() {
        let start_pos = Vec3::new(5.0, 1.0, 5.0);
        let end_pos = Vec3::new(-15.0, 1.0, -15.0);

        // Run pathfinding to get a polygon path.
        match find_polygon_path(
            &nav_mesh,
            &nav_mesh_settings,
            start_pos,
            end_pos,
            None,
            Some(&[1.0, 0.5]),
        ) {
            Ok(path) => {
                info!("Path found (BLOCKING): {:?}", path);

                // Convert polygon path to a path of Vec3s.
                match perform_string_pulling_on_path(&nav_mesh, start_pos, end_pos, &path) {
                    Ok(string_path) => {
                        info!("String path (BLOCKING): {:?}", string_path);
                        commands.spawn(DrawPath {
                            timer: Timer::from_seconds(4.0, TimerMode::Once),
                            pulled_path: string_path,
                            color: Color::RED,
                        });
                    }
                    Err(error) => error!("Error with string path: {:?}", error),
                };
            }
            Err(error) => error!("Error with pathfinding: {:?}", error),
        }
    }
}

//
//  Async Pathfinding.
//  Press A to run.
//
//  Running pathfinding in a task without blocking the frame.
//  Also check out Bevy's async compute example.
//  https://github.com/bevyengine/bevy/blob/main/examples/async_tasks/async_compute.rs
//

// Holder resource for tasks.
#[derive(Default, Resource)]
struct AsyncPathfindingTasks {
    tasks: Vec<Task<Option<Vec<Vec3>>>>,
}

// Queue up pathfinding tasks.
fn run_async_pathfinding(
    keys: Res<Input<KeyCode>>,
    nav_mesh_settings: Res<NavMeshSettings>,
    nav_mesh: Res<NavMesh>,
    mut pathfinding_task: ResMut<AsyncPathfindingTasks>,
) {
    if !keys.just_pressed(KeyCode::A) {
        return;
    }

    let thread_pool = AsyncComputeTaskPool::get();

    let nav_mesh_lock = nav_mesh.get();
    let start_pos = Vec3::new(5.0, 1.0, 5.0);
    let end_pos = Vec3::new(-15.0, 1.0, -15.0);

    let task = thread_pool.spawn(async_path_find(
        nav_mesh_lock,
        nav_mesh_settings.clone(),
        start_pos,
        end_pos,
        None,
    ));

    pathfinding_task.tasks.push(task);
}

// Poll existing tasks.
fn poll_pathfinding_tasks_system(
    mut commands: Commands,
    mut pathfinding_task: ResMut<AsyncPathfindingTasks>,
) {
    // Go through and remove completed tasks.
    pathfinding_task.tasks.retain_mut(|task| {
        if let Some(string_path) = future::block_on(future::poll_once(task)).unwrap_or(None) {
            info!("Async path task finished with result: {:?}", string_path);
            commands.spawn(DrawPath {
                timer: Timer::from_seconds(4.0, TimerMode::Once),
                pulled_path: string_path,
                color: Color::BLUE,
            });

            false
        } else {
            true
        }
    });
}

/// Async wrapper function for path finding.
async fn async_path_find(
    nav_mesh_lock: Arc<RwLock<NavMeshTiles>>,
    nav_mesh_settings: NavMeshSettings,
    start_pos: Vec3,
    end_pos: Vec3,
    position_search_radius: Option<f32>,
) -> Option<Vec<Vec3>> {
    // Get the underlying nav_mesh.
    let Ok(nav_mesh) = nav_mesh_lock.read() else {
        return None;
    };

    // Run pathfinding to get a path.
    match find_path(
        &nav_mesh,
        &nav_mesh_settings,
        start_pos,
        end_pos,
        position_search_radius,
        Some(&[1.0, 0.5]),
    ) {
        Ok(path) => {
            info!("Found path (ASYNC): {:?}", path);
            return Some(path);
        }
        Err(error) => error!("Error with pathfinding: {:?}", error),
    }

    None
}

#[derive(Component)]
struct DrawPath {
    timer: Timer,
    pulled_path: Vec<Vec3>,
    color: Color,
}

// Helper function to draw a path for the timer's duration.
fn draw_path(
    mut commands: Commands,
    mut path_query: Query<(Entity, &mut DrawPath)>,
    time: Res<Time>,
    mut gizmos: Gizmos,
) {
    for (entity, mut draw_path) in path_query.iter_mut() {
        if draw_path.timer.tick(time.delta()).just_finished() {
            commands.entity(entity).despawn();
        } else {
            gizmos.linestrip(draw_path.pulled_path.clone(), draw_path.color);
        }
    }
}

#[derive(Resource)]
struct DrawNavMesh(bool);

//
//  Draw Nav-mesh.
//  Press M to run.
//
fn draw_nav_mesh_system(
    keys: Res<Input<KeyCode>>,
    nav_mesh: Res<NavMesh>,
    mut gizmos: Gizmos,
    mut show_navmesh: ResMut<DrawNavMesh>,
) {
    if keys.just_pressed(KeyCode::M) {
        show_navmesh.0 = !show_navmesh.0;
    }

    if show_navmesh.0 {
        if let Ok(nav_mesh) = nav_mesh.get().read() {
            for (tile_coord, tile) in nav_mesh.get_tiles().iter() {
                let tile_color = Color::Rgba {
                    red: 0.0,
                    green: (tile_coord.x % 10) as f32 / 10.0,
                    blue: (tile_coord.y % 10) as f32 / 10.0,
                    alpha: 1.0,
                };
                // Draw polygons.
                for poly in tile.polygons.iter() {
                    let indices = &poly.indices;
                    for i in 0..indices.len() {
                        let a = tile.vertices[indices[i] as usize];
                        let b = tile.vertices[indices[(i + 1) % indices.len()] as usize];
                        gizmos.line(a, b, tile_color);
                    }
                }

                // Draw vertex points.
                for vertex in tile.vertices.iter() {
                    gizmos.line(*vertex, *vertex + Vec3::Y, tile_color);
                }
            }
        }
    }
}

fn setup_world_system(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // Plane
    commands.spawn((
        PbrBundle {
            mesh: meshes.add(Mesh::from(bevy::prelude::shape::Plane {
                size: 50.0,
                subdivisions: 0,
            })),
            material: materials.add(Color::rgb(0.3, 0.5, 0.3).into()),
            transform: Transform::IDENTITY,
            ..default()
        },
        Collider::cuboid(25.0, 0.1, 25.0),
        NavMeshAffector, // Only entities with a NavMeshAffector component will contribute to the nav-mesh.
    ));

    // Cube
    commands.spawn((
        PbrBundle {
            mesh: meshes.add(Mesh::from(bevy::prelude::shape::Cube { size: 2.5 })),
            material: materials.add(Color::rgb(0.1, 0.1, 0.5).into()),
            transform: Transform::from_xyz(-5.0, 0.8, -5.0),
            ..default()
        },
        Collider::cuboid(1.25, 1.25, 1.25),
        NavMeshAffector, // Only entities with a NavMeshAffector component will contribute to the nav-mesh.
    ));

    // Tall Cube
    commands.spawn((
        PbrBundle {
            mesh: meshes.add(Mesh::from(bevy::prelude::shape::Cube { size: 2.5 })),
            material: materials.add(Color::rgb(0.1, 0.75, 0.5).into()),
            transform: Transform::from_xyz(-0.179, 18.419, -27.744)
                .with_scale(Vec3::new(15.0, 15.0, 15.0)),
            ..default()
        },
        Collider::cuboid(1.25, 1.25, 1.25),
        NavMeshAffector, // Only entities with a NavMeshAffector component will contribute to the nav-mesh.
    ));

    // Thin wall
    commands.spawn((
        PbrBundle {
            mesh: meshes.add(Mesh::from(bevy::prelude::shape::Cube { size: 0.1 })),
            material: materials.add(Color::rgb(0.1, 0.1, 0.5).into()),
            transform: Transform::from_xyz(-3.0, 0.8, 5.0).with_scale(Vec3::new(50.0, 15.0, 1.0)),
            ..default()
        },
        Collider::cuboid(0.05, 0.05, 0.05),
        NavMeshAffector, // Only entities with a NavMeshAffector component will contribute to the nav-mesh.
    ));

    // light
    commands.spawn(PointLightBundle {
        transform: Transform::from_xyz(4.0, 8.0, 4.0),
        ..default()
    });

    // Camera
    commands.spawn(Camera3dBundle {
        transform: Transform::from_xyz(10.0, 10.0, 15.0)
            .looking_at(Vec3::new(0.0, 2.0, 0.0), Vec3::Y),
        ..default()
    });
}

fn spawn_or_despawn_affector_system(
    keys: Res<Input<KeyCode>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut spawned_entity: Local<Option<Entity>>,
) {
    if !keys.just_pressed(KeyCode::X) {
        return;
    }

    if let Some(entity) = *spawned_entity {
        commands.entity(entity).despawn_recursive();
        *spawned_entity = None;
    } else {
        let entity = commands
            .spawn((
                PbrBundle {
                    mesh: meshes.add(Mesh::from(bevy::prelude::shape::Cube { size: 2.5 })),
                    material: materials.add(Color::rgb(1.0, 0.1, 0.5).into()),
                    transform: Transform::from_xyz(5.0, 0.8, -5.0),
                    ..default()
                },
                Collider::cuboid(1.25, 1.25, 1.25),
                NavMeshAffector, // Only entities with a NavMeshAffector component will contribute to the nav-mesh.
            ))
            .id();

        *spawned_entity = Some(entity);
    }
}

fn info_system() {
    info!("=========================================");
    info!("| Press A to run ASYNC path finding.    |");
    info!("| Press B to run BLOCKING path finding. |");
    info!("| Press M to draw nav-mesh.             |");
    info!("| Press X to spawn or despawn red cube. |");
    info!("=========================================");
}
