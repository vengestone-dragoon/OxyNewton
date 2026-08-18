use image::RgbaImage;
use rapier2d::prelude::*;

/// Safety cap on how many times a single body can be reflected off the world
/// bounds in one frame. Bounded so a body that's absurdly far out of bounds
/// (or has degenerate restitution/friction values) can't loop forever.
const MAX_BOUNDARY_BOUNCES: u32 = 16;

/// Thickness of the static boundary walls, and how far each wall extends
/// past the world's corners so adjoining walls overlap there instead of
/// leaving a diagonal gap a fast body could sneak through.
const WALL_THICKNESS: f32 = 64.0;

/// Default gravity magnitude (px/s^2), regardless of direction.
pub const GRAVITY_MAGNITUDE: f32 = 981.0;

/// Roughly how long a grabbed icon takes to close most of the gap to the
/// pointer, in seconds. Small = tight/snappy tracking with little visible
/// lag; the closer to 0, the more instantly it follows. See
/// `smooth_damp_velocity` for why this one knob is enough to stay
/// non-overshooting at any value.
const GRAB_SMOOTH_TIME: f32 = 0.04;

/// A single sprite's position and rotation in an engine's world space, as of
/// the last `Engine::snapshot()` call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpriteTransform {
    pub position: Vector,
    pub rotation: f32,
}

/// Every icon's and taskbar slice's current transform, in the same order as
/// `Engine::icons()`/`Engine::icon_sizes()` and
/// `Engine::taskbar_slices()`/`Engine::taskbar_sizes()` respectively. Produced
/// by the simulation thread each tick and handed to the render thread.
#[derive(Debug, Clone, Default)]
pub struct EngineSnapshot {
    pub icons: Vec<SpriteTransform>,
    pub taskbar: Vec<SpriteTransform>,
}

pub struct Engine {
    // The desktop wallpaper for this engine's monitor.
    background: RgbaImage,
    icons: Box<[RgbaImage]>,
    icon_sizes: Box<[Vector]>,
    icon_objects: Box<[RigidBodyHandle]>,
    taskbar_slices: Box<[RgbaImage]>,
    taskbar_sizes: Box<[Vector]>,
    taskbar_objects: Box<[RigidBodyHandle]>,
    // Static colliders just outside the world border (left, right, top,
    // bottom) that give rapier's own solver a normal wall to bounce off of;
    // resolve_out_of_bounds() only has to step in for bodies that get past
    // these (tunneling at very high speed, or a direct position write).
    wall_objects: Box<[RigidBodyHandle]>,
    // Owns its own bodies/colliders/pipeline/etc, so each Engine instance
    // simulates completely independently of any other Engine instance.
    physics: PhysicsWorld,
    // World bounds are [0, world_size] on each axis, set once at construction.
    world_size: Vector,
    // Fraction of the impact-normal speed kept after bouncing off a bound.
    bounds_restitution: f32,
    // Multiplicative drag applied to velocity for every leg of travel spent
    // outside the bounds (air resistance during the excursion / bounce).
    bounds_air_friction: f32,
    // The icon body currently being dragged, if any, and the offset from
    // that body's center to the pointer at the moment it was grabbed (kept
    // constant for the life of the grab so the icon doesn't jump to be
    // centered under the pointer).
    grabbed: Option<RigidBodyHandle>,
    grab_offset: Vector,
    // Latest pointer position in this engine's world space, updated
    // independently of whether anything is currently grabbed.
    pointer: Vector,
}

impl Engine {
    /// `icon_sizes`/`taskbar_sizes` are full (width, height) extents, not
    /// half-extents. Every `icons`/`icon_positions`/`icon_sizes` triple (and
    /// likewise for the taskbar slices) must be the same length and in the
    /// same order — index `i` of each vec describes the same object.
    pub fn new(
        background: RgbaImage,
        icons: Vec<RgbaImage>,
        icon_positions: Vec<Vector>,
        icon_sizes: Vec<Vector>,
        taskbar_slices: Vec<RgbaImage>,
        taskbar_positions: Vec<Vector>,
        taskbar_sizes: Vec<Vector>,
        world_size: Vector,
    ) -> Self {
        debug_assert_eq!(icons.len(), icon_positions.len());
        debug_assert_eq!(icons.len(), icon_sizes.len());
        debug_assert_eq!(taskbar_slices.len(), taskbar_positions.len());
        debug_assert_eq!(taskbar_slices.len(), taskbar_sizes.len());

        let icons = icons.into_boxed_slice();
        let taskbar_slices = taskbar_slices.into_boxed_slice();
        let icon_sizes = icon_sizes.into_boxed_slice();
        let taskbar_sizes = taskbar_sizes.into_boxed_slice();

        let mut physics = PhysicsWorld::new();
        physics.gravity = Vector::new(0.0, GRAVITY_MAGNITUDE); // straight down by default
        // rapier's default thresholds (including its ~400 units/s hard cap on
        // linear velocity) assume ~1 world unit = 1 meter. This world runs in
        // raw pixels with ~100px icons, so `length_unit` needs to reflect
        // that — otherwise everything (falling, bouncing, and especially the
        // drag spring) is silently capped far below any speed that looks
        // right at pixel scale.
        physics.integration_parameters.length_unit = 100.0;

        let icon_objects = icon_positions
            .into_iter()
            .zip(icon_sizes.iter().copied())
            .map(|(position, size)| {
                let (body, _collider) = physics.insert(
                    // `can_sleep(false)`: a sleeping body needs its wake-up
                    // to be correctly threaded through island bookkeeping
                    // before it'll respond to anything again, and that's
                    // proven fragile here (mouse-grab interaction would
                    // silently stop working). Never sleeping sidesteps that
                    // class of bug entirely — there are only a handful of
                    // these bodies, so the extra simulation cost is trivial.
                    RigidBodyBuilder::dynamic().translation(position).can_sleep(false),
                    // Circular, not rectangular like the taskbar — sized to
                    // the smaller dimension so it stays inscribed within the
                    // icon's sprite rather than poking out past a narrow
                    // edge.
                    ColliderBuilder::ball(size.x.min(size.y) / 2.0).restitution(0.3),
                );
                body
            })
            .collect();

        let taskbar_objects = taskbar_positions
            .into_iter()
            .zip(taskbar_sizes.iter().copied())
            .map(|(position, size)| {
                let (body, _collider) = physics.insert(
                    RigidBodyBuilder::dynamic().translation(position).can_sleep(false),
                    ColliderBuilder::cuboid(size.x / 2.0, size.y / 2.0).restitution(0.3),
                );
                body
            })
            .collect();

        let wall_objects = build_walls(&mut physics, world_size);

        Self {
            background,
            icons,
            icon_sizes,
            icon_objects,
            taskbar_slices,
            taskbar_sizes,
            taskbar_objects,
            wall_objects,
            physics,
            world_size,
            bounds_restitution: 0.6,
            bounds_air_friction: 0.98,
            grabbed: None,
            grab_offset: Vector::ZERO,
            pointer: Vector::ZERO,
        }
    }

    pub fn set_bounds_restitution(&mut self, restitution: f32) {
        self.bounds_restitution = restitution;
    }

    pub fn set_bounds_air_friction(&mut self, air_friction: f32) {
        self.bounds_air_friction = air_friction;
    }

    /// Overwrites gravity outright (not additively) — callers own deciding
    /// the full direction/magnitude each time, so repeated calls (e.g. from
    /// a held key firing repeat events) can't drift the force by re-applying
    /// a delta on top of what's already there.
    pub fn set_gravity(&mut self, gravity: Vector) {
        self.physics.gravity = gravity;
    }

    /// Advance this engine's physics world by `dt` seconds.
    pub fn process(&mut self, dt: f32) {
        self.drive_grabbed_body(dt);
        self.physics.integration_parameters.dt = dt;
        self.physics.step();
        self.resolve_out_of_bounds();
    }

    /// Attempts to grab whichever body (icon or taskbar slice, if any) sits
    /// under `point`. Only dynamic bodies are considered
    /// (`QueryFilter::exclude_fixed`), so the boundary walls are never
    /// grabbable. Returns whether anything was grabbed.
    pub fn try_grab(&mut self, point: Vector) -> bool {
        self.pointer = point;

        // `max_dist` is a search-radius cutoff applied before the solid/inside
        // check, not "how far outside is still a hit" — 0.0 there was
        // rejecting points that were genuinely inside a collider. `f32::MAX`
        // removes the limit entirely; `projection.is_inside` below is what
        // actually decides whether the click landed on something.
        let hit = self
            .physics
            .project_point(point, f32::MAX, true, QueryFilter::exclude_fixed());
        let Some((collider_handle, projection)) = hit else {
            return false;
        };
        if !projection.is_inside {
            return false;
        }
        let Some(body_handle) = self.physics.colliders.get(collider_handle).and_then(|c| c.parent()) else {
            return false;
        };
        let Some(body) = self.physics.bodies.get(body_handle) else {
            return false;
        };

        self.grab_offset = point - body.translation();
        self.grabbed = Some(body_handle);
        true
    }

    /// Updates the pointer's current position in this engine's world space.
    /// Only affects simulation while a body is grabbed (see
    /// `drive_grabbed_body`); otherwise it's just tracked for the next
    /// `try_grab` call.
    pub fn move_pointer(&mut self, point: Vector) {
        self.pointer = point;
    }

    pub fn release_grab(&mut self) {
        self.grabbed = None;
    }

    /// A snapshot of every icon's and taskbar slice's current position and
    /// rotation, in the same order as `icons()`/`taskbar_slices()`. Cheap
    /// enough to call every simulation tick and hand off to a render thread.
    pub fn snapshot(&self) -> EngineSnapshot {
        EngineSnapshot {
            icons: self.icon_objects.iter().map(|&h| self.sprite_transform(h)).collect(),
            taskbar: self.taskbar_objects.iter().map(|&h| self.sprite_transform(h)).collect(),
        }
    }

    fn sprite_transform(&self, handle: RigidBodyHandle) -> SpriteTransform {
        let body = &self.physics.bodies[handle];
        SpriteTransform {
            position: body.translation(),
            rotation: body.rotation().angle(),
        }
    }

    /// While a body is grabbed, pulls it toward the pointer with a
    /// critically-damped spring (see `smooth_damp_velocity`) and damps its
    /// spin, rather than snapping it straight there (which flings it
    /// violently for a fast mouse move / small `dt`) or using an actual
    /// rapier joint (which needs the grabbed body to stay awake for the
    /// solver to touch it every tick — icons here settle and sleep
    /// constantly, and a joint to a sleeping body silently does nothing
    /// until something else wakes it up). Driving velocity directly avoids
    /// that: `set_linvel`/`set_angvel` below always pass `wake_up = true`,
    /// so a grabbed body can never go to sleep out from under the drag.
    /// No-op if nothing's grabbed.
    fn drive_grabbed_body(&mut self, dt: f32) {
        let Some(handle) = self.grabbed else { return };
        let Some(body) = self.physics.bodies.get_mut(handle) else {
            self.grabbed = None;
            return;
        };

        if dt > 0.0 {
            let target = self.pointer - self.grab_offset;
            let velocity = smooth_damp_velocity(body.translation(), target, body.linvel(), GRAB_SMOOTH_TIME, dt);
            body.set_linvel(velocity, true);
        }
        body.set_angvel(body.angvel() * 0.9, true);
    }

    pub fn background(&self) -> &RgbaImage {
        &self.background
    }

    pub fn icons(&self) -> &[RgbaImage] {
        &self.icons
    }

    pub fn icon_sizes(&self) -> &[Vector] {
        &self.icon_sizes
    }

    pub fn taskbar_slices(&self) -> &[RgbaImage] {
        &self.taskbar_slices
    }

    pub fn taskbar_sizes(&self) -> &[Vector] {
        &self.taskbar_sizes
    }

    /// Fold every icon and taskbar body back inside `world_size`, bouncing it
    /// off each edge it crosses. A body that ended up far outside the bounds
    /// in a single step (e.g. a large `dt`, or a body nudged there directly)
    /// bounces edge-to-edge repeatedly in this same call until it lands back
    /// in bounds, losing speed to air friction and to the bounds impact
    /// itself each time, rather than just being clamped in place.
    fn resolve_out_of_bounds(&mut self) {
        let handles: Vec<RigidBodyHandle> =
            self.icon_objects.iter().chain(self.taskbar_objects.iter()).copied().collect();
        for handle in handles {
            let half_extents = self.body_half_extents(handle);

            // Bounds for the body's *center*, shrunk by its own half-size so
            // the whole body (not just its center point) stays on-screen.
            let min = half_extents;
            let max = self.world_size - half_extents;
            if min.x > max.x || min.y > max.y {
                continue; // body is bigger than the world on this axis
            }

            let body = &mut self.physics.bodies[handle];
            let mut pos = body.translation();
            let mut vel = body.linvel();

            reflect_axis(
                &mut pos.x,
                &mut vel.x,
                &mut vel.y,
                min.x,
                max.x,
                self.bounds_restitution,
                self.bounds_air_friction,
            );
            reflect_axis(
                &mut pos.y,
                &mut vel.y,
                &mut vel.x,
                min.y,
                max.y,
                self.bounds_restitution,
                self.bounds_air_friction,
            );

            body.set_translation(pos, true);
            body.set_linvel(vel, true);
        }
    }

    fn body_half_extents(&self, handle: RigidBodyHandle) -> Vector {
        self.physics.bodies[handle]
            .colliders()
            .iter()
            .filter_map(|&handle| self.physics.colliders.get(handle))
            .map(|collider| collider.compute_aabb().half_extents())
            .next()
            .unwrap_or(Vector::ZERO)
    }
}

/// Builds the four static walls that ring `world_size`. Each wall sits
/// entirely outside the world border — its inner face is flush with the
/// border, and it extends outward from there — and each is stretched by
/// `WALL_THICKNESS` past both of its ends so it overlaps the perpendicular
/// wall at the corner instead of leaving a gap where the two thin rectangles
/// would otherwise only touch at a single point.
fn build_walls(physics: &mut PhysicsWorld, world_size: Vector) -> Box<[RigidBodyHandle]> {
    let t = WALL_THICKNESS;
    let half = t / 2.0;

    let walls = [
        // left
        (
            Vector::new(-half, world_size.y / 2.0),
            Vector::new(half, world_size.y / 2.0 + t),
        ),
        // right
        (
            Vector::new(world_size.x + half, world_size.y / 2.0),
            Vector::new(half, world_size.y / 2.0 + t),
        ),
        // top
        (
            Vector::new(world_size.x / 2.0, -half),
            Vector::new(world_size.x / 2.0 + t, half),
        ),
        // bottom
        (
            Vector::new(world_size.x / 2.0, world_size.y + half),
            Vector::new(world_size.x / 2.0 + t, half),
        ),
    ];

    walls
        .into_iter()
        .map(|(center, half_extents)| {
            physics
                .insert(
                    RigidBodyBuilder::fixed().translation(center),
                    ColliderBuilder::cuboid(half_extents.x, half_extents.y),
                )
                .0
        })
        .collect()
}

/// Returns the velocity that carries `current` toward `target` under a
/// critically-damped spring — high acceleration while far away, smoothly
/// decelerating on approach so it settles at the target without
/// overshooting or oscillating. `smooth_time` is roughly how long it takes
/// to close most of the gap; the returned velocity is stable for any
/// `smooth_time`/`dt` (including a large `dt` spike), which a naive
/// semi-implicit spring (`accel = k*x - c*v`) isn't — push `k` high enough
/// for tight tracking and it can overshoot or blow up between one tick and
/// the next instead of just easing in faster.
///
/// This is the standard fast critically-damped-spring approximation (Ryan
/// Juckett; the same one behind Unity's `Vector2.SmoothDamp`). Position
/// integration is deliberately left to rapier's own step rather than
/// applied here, so the body still collides with everything else along the
/// way instead of teleporting.
fn smooth_damp_velocity(current: Vector, target: Vector, velocity: Vector, smooth_time: f32, dt: f32) -> Vector {
    let omega = 2.0 / smooth_time;
    let x = omega * dt;
    let exp = 1.0 / (1.0 + x + 0.48 * x * x + 0.235 * x * x * x);
    let change = current - target;
    let temp = (velocity + change * omega) * dt;
    (velocity - temp * omega) * exp
}

/// Reflects `pos` back into `[min, max]` on one axis, bouncing as many times
/// as needed (capped by [`MAX_BOUNDARY_BOUNCES`]) if it started far outside
/// — e.g. far past `min` reflects off `min`, may still be past `max`,
/// reflects off `max`, and so on until it settles inside the range.
///
/// A naive mirror (`pos = min + (min - pos)`) preserves the raw overshoot
/// distance every bounce even though the velocity that produced it just got
/// scaled down — position and velocity would disagree, and a large initial
/// overshoot could ping-pong for a while before the (unscaled) distance
/// happens to land inside `[min, max]`. Instead the overshoot itself is
/// scaled down by the same per-bounce loss (`restitution * air_friction`)
/// applied to the velocity, so the excess distance shrinks geometrically
/// with the speed that's carrying it — one or two bounces is normally
/// enough even for a body launched far outside at high speed, and the
/// result stays physically consistent with the returned velocity.
///
/// `normal_vel` is the velocity component along this axis (flipped and
/// scaled by the loss on each bounce — energy lost in the impact plus drag).
/// `tangent_vel` is the velocity component along the other axis, which only
/// picks up the `air_friction` part since it doesn't participate in the
/// impact itself.
fn reflect_axis(
    pos: &mut f32,
    normal_vel: &mut f32,
    tangent_vel: &mut f32,
    min: f32,
    max: f32,
    restitution: f32,
    air_friction: f32,
) {
    let loss = restitution * air_friction;

    for _ in 0..MAX_BOUNDARY_BOUNCES {
        if *pos >= min && *pos <= max {
            break;
        }

        *tangent_vel *= air_friction;

        let overshoot = if *pos < min { min - *pos } else { *pos - max };
        let folded = overshoot * loss;

        if *pos < min {
            *pos = min + folded; // mirror across the min edge, shrunk by the bounce loss
        } else {
            *pos = max - folded; // mirror across the max edge, shrunk by the bounce loss
        }
        *normal_vel = -*normal_vel * loss;
    }

    // Safety net in case the bounce budget ran out (e.g. a degenerate span).
    *pos = pos.clamp(min, max);
}