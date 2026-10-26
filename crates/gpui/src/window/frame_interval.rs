use crate::{RequestFrameOptions, ThermalState};
use std::time::Duration;

const INACTIVE_FRAME_INTERVAL: Duration = Duration::from_micros(33333);
const THROTTLED_FRAME_INTERVAL: Duration = Duration::from_micros(16667);

/// How long a frame requested by the platform must wait after the previous one, or
/// `None` to draw as soon as it is asked for.
///
/// `thermal_state` reads the system through the app context, so it runs only when the
/// answer depends on it: not for the idle requests an undirtied window receives on
/// every vsync.
pub(crate) fn min_frame_interval(
    options: RequestFrameOptions,
    has_next_frame_callbacks: bool,
    window_active: bool,
    thermal_state: impl FnOnce() -> Option<ThermalState>,
) -> Option<Duration> {
    if !options.force_render && !options.require_presentation && !has_next_frame_callbacks {
        None
    } else if !window_active {
        Some(INACTIVE_FRAME_INTERVAL)
    } else if let Some(ThermalState::Critical | ThermalState::Serious) = thermal_state() {
        Some(THROTTLED_FRAME_INTERVAL)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn options(force_render: bool, require_presentation: bool) -> RequestFrameOptions {
        RequestFrameOptions {
            require_presentation,
            force_render,
        }
    }

    /// The interval for a request, and how many times the thermal state was read.
    fn interval(
        options: RequestFrameOptions,
        has_next_frame_callbacks: bool,
        window_active: bool,
        thermal: Option<ThermalState>,
    ) -> (Option<Duration>, usize) {
        let reads = Cell::new(0);
        let interval = min_frame_interval(options, has_next_frame_callbacks, window_active, || {
            reads.set(reads.get() + 1);
            thermal
        });
        (interval, reads.get())
    }

    #[test]
    fn an_idle_vsync_request_never_reads_the_thermal_state() {
        for active in [true, false] {
            assert_eq!(
                interval(
                    options(false, false),
                    false,
                    active,
                    Some(ThermalState::Critical)
                ),
                (None, 0)
            );
        }
    }

    #[test]
    fn an_inactive_window_is_held_to_30_fps_without_reading_the_thermal_state() {
        assert_eq!(
            interval(
                options(true, false),
                false,
                false,
                Some(ThermalState::Critical)
            ),
            (Some(INACTIVE_FRAME_INTERVAL), 0)
        );
    }

    #[test]
    fn an_active_window_under_thermal_pressure_is_held_to_60_fps() {
        for thermal in [ThermalState::Serious, ThermalState::Critical] {
            assert_eq!(
                interval(options(false, true), false, true, Some(thermal)),
                (Some(THROTTLED_FRAME_INTERVAL), 1)
            );
        }
    }

    #[test]
    fn an_active_window_without_thermal_pressure_draws_as_asked() {
        for thermal in [None, Some(ThermalState::Nominal), Some(ThermalState::Fair)] {
            assert_eq!(
                interval(options(false, false), true, true, thermal),
                (None, 1)
            );
        }
    }
}
