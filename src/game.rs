//! Fixed-step simulation scheduling around Shipyard workloads.

use std::time::Duration;

use x173ge_ecs::{Unique, UniqueView, UniqueViewMut, Workload, World};
use x173ge_platform::InputState;

const FIXED_STEP: Duration = Duration::from_nanos(16_666_667);
const MAX_CATCH_UP: u32 = 4;

/// Simulation clock exposed to Shipyard systems as a unique resource.
#[derive(Unique, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FixedTime {
    delta: Duration,
    elapsed: Duration,
    ticks: u64,
}

/// Input state moved into the ECS for one fixed update without cloning it.
#[derive(Unique, Default)]
pub struct InputFrame(InputState);

impl InputFrame {
    /// Returns held keys/buttons and transitions for the current fixed update.
    pub fn state(&self) -> &InputState {
        &self.0
    }
}

impl FixedTime {
    /// Returns the fixed duration of the current update.
    pub fn delta(&self) -> Duration {
        self.delta
    }

    /// Returns elapsed simulation time, excluding dropped catch-up time.
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// Returns the number of completed fixed updates.
    pub fn ticks(&self) -> u64 {
        self.ticks
    }
}

/// Fixed work to run before the next rendered frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameSchedule {
    pub ticks: u8,
    pub interpolation: f32,
    pub dropped: Duration,
}

/// Accumulates wall time and caps simulation catch-up work.
pub struct FixedClock {
    accumulator: Duration,
}

impl Default for FixedClock {
    /// Starts the engine clock with a 60 Hz simulation step.
    fn default() -> Self {
        Self {
            accumulator: Duration::ZERO,
        }
    }
}

impl FixedClock {
    /// Returns the engine's fixed simulation interval.
    pub const fn step(&self) -> Duration {
        FIXED_STEP
    }

    /// Adds a rendered-frame duration and retains only the fractional remainder.
    pub fn advance(&mut self, wall_delta: Duration) -> FrameSchedule {
        self.accumulator = self.accumulator.saturating_add(wall_delta);
        let total = self.accumulator.as_nanos();
        let step = FIXED_STEP.as_nanos();
        let available = total / step;
        let ticks = available.min(MAX_CATCH_UP as u128) as u8;
        let remainder = total % step;
        let dropped = total - step * ticks as u128 - remainder;
        self.accumulator = duration_from_nanos(if available > MAX_CATCH_UP as u128 {
            remainder
        } else {
            total - step * ticks as u128
        });
        FrameSchedule {
            ticks,
            interpolation: (self.accumulator.as_secs_f64() / FIXED_STEP.as_secs_f64()) as f32,
            dropped: duration_from_nanos(dropped),
        }
    }
}

/// Game world and fixed Shipyard workload.
pub struct Game {
    world: World,
    clock: FixedClock,
}

/// Failure while registering or running a Shipyard workload.
#[derive(Debug, thiserror::Error)]
pub enum GameError {
    #[error("cannot register fixed workload: {0}")]
    Setup(String),
    #[error("fixed workload failed: {0}")]
    Workload(String),
}

impl Game {
    /// Creates a world with a 60 Hz fixed workload and its time resource.
    pub fn new(workload: Workload) -> Result<Self, GameError> {
        let world = World::new();
        world.add_unique(FixedTime::default());
        world.add_unique(InputFrame::default());
        workload
            .add_to_world(&world)
            .map_err(|error| GameError::Setup(error.to_string()))?;
        Ok(Self {
            world,
            clock: FixedClock::default(),
        })
    }

    /// Runs at most four fixed updates for one rendered-frame duration.
    pub fn advance(&mut self, wall_delta: Duration) -> Result<FrameSchedule, GameError> {
        self.advance_with(wall_delta, |_, _| {})
    }

    /// Runs fixed updates and invokes the game hook immediately before each workload.
    pub fn advance_with(
        &mut self,
        wall_delta: Duration,
        mut before_tick: impl FnMut(&World, FixedTime),
    ) -> Result<FrameSchedule, GameError> {
        self.advance_with_input(wall_delta, &mut InputState::default(), |world, time, _| {
            before_tick(world, time)
        })
    }

    /// Moves input into Shipyard for each tick, then consumes its transient values.
    pub fn advance_with_input(
        &mut self,
        wall_delta: Duration,
        input: &mut InputState,
        mut before_tick: impl FnMut(&World, FixedTime, &InputState),
    ) -> Result<FrameSchedule, GameError> {
        let plan = self.clock.advance(wall_delta);
        for _ in 0..plan.ticks {
            let time = {
                let mut time = self
                    .world
                    .borrow::<UniqueViewMut<FixedTime>>()
                    .map_err(|error| GameError::Workload(error.to_string()))?;
                time.delta = FIXED_STEP;
                time.elapsed = time.elapsed.saturating_add(FIXED_STEP);
                time.ticks += 1;
                *time
            };
            {
                let mut frame = self
                    .world
                    .borrow::<UniqueViewMut<InputFrame>>()
                    .map_err(|error| GameError::Workload(error.to_string()))?;
                std::mem::swap(&mut frame.0, input);
            }
            {
                let frame = self
                    .world
                    .borrow::<UniqueView<InputFrame>>()
                    .map_err(|error| GameError::Workload(error.to_string()))?;
                before_tick(&self.world, time, frame.state());
            }
            let workload = self.world.run_default_workload();
            {
                let mut frame = self
                    .world
                    .borrow::<UniqueViewMut<InputFrame>>()
                    .map_err(|error| GameError::Workload(error.to_string()))?;
                frame.0.clear_transient();
                std::mem::swap(&mut frame.0, input);
            }
            workload.map_err(|error| GameError::Workload(error.to_string()))?;
        }
        Ok(plan)
    }

    /// Returns the game-owned Shipyard world for setup and extraction.
    pub fn world(&self) -> &World {
        &self.world
    }

    /// Returns the fixed simulation interval.
    pub const fn fixed_step(&self) -> Duration {
        self.clock.step()
    }
}

fn duration_from_nanos(nanos: u128) -> Duration {
    Duration::new(
        (nanos / 1_000_000_000) as u64,
        (nanos % 1_000_000_000) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_caps_catch_up_and_keeps_fraction_for_interpolation() {
        let mut clock = FixedClock::default();
        let schedule = clock.advance(Duration::from_millis(300));
        assert_eq!(schedule.ticks, 4);
        assert!(schedule.dropped > Duration::ZERO);
        assert!((0.0..1.0).contains(&schedule.interpolation));
    }

    #[test]
    fn clock_does_not_run_before_one_fixed_step() {
        let mut clock = FixedClock::default();
        let schedule = clock.advance(Duration::from_millis(8));
        assert_eq!(schedule.ticks, 0);
        assert!((schedule.interpolation - 0.48).abs() < 0.01);
    }
}
