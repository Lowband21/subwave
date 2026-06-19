//! Clocked subtitle scheduling for Wayland subtitle presentation.
//!
//! The scheduler is intentionally independent of Wayland and GStreamer so subtitle timing behavior
//! can be covered with unit tests. Callers feed decoded subtitle events in running-time units and
//! poll for attach/clear actions using the current media running time.

use std::{sync::OnceLock, time::Duration};

/// A decoded subtitle event addressed to a selected subtitle stream generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodedSubtitleEvent<P> {
    /// Present a decoded subtitle payload during the given running-time interval.
    Show(DecodedSubtitleCue<P>),
    /// Clear the subtitle surface at the given running time.
    Clear(DecodedSubtitleClear),
}

impl<P> DecodedSubtitleEvent<P> {
    /// Build a decoded subtitle show event.
    pub fn show(
        stream_id: impl Into<String>,
        generation: u64,
        start: Duration,
        end: Option<Duration>,
        payload: P,
    ) -> Self {
        Self::Show(DecodedSubtitleCue {
            stream_id: stream_id.into(),
            generation,
            start,
            end,
            payload,
        })
    }

    /// Build a decoded subtitle clear event.
    pub fn clear(stream_id: impl Into<String>, generation: u64, at: Duration) -> Self {
        Self::Clear(DecodedSubtitleClear {
            stream_id: stream_id.into(),
            generation,
            at,
        })
    }
}

/// A decoded subtitle payload and its running-time presentation window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedSubtitleCue<P> {
    pub stream_id: String,
    pub generation: u64,
    pub start: Duration,
    pub end: Option<Duration>,
    pub payload: P,
}

/// A decoded subtitle clear event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedSubtitleClear {
    pub stream_id: String,
    pub generation: u64,
    pub at: Duration,
}

/// A subtitle action that is due for the renderer at the current media time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubtitleAction<P> {
    /// Attach a decoded subtitle payload to the subtitle surface.
    Attach(SubtitleAttach<P>),
    /// Detach any current subtitle payload from the subtitle surface.
    Clear(SubtitleClearAction),
}

/// A due subtitle attach action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubtitleAttach<P> {
    pub stream_id: String,
    pub generation: u64,
    pub start: Duration,
    pub end: Option<Duration>,
    pub payload: P,
}

/// A due subtitle clear action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubtitleClearAction {
    pub stream_id: String,
    pub generation: u64,
}

/// Clocked scheduler for decoded subtitle presentation.
///
/// The scheduler tracks one selected subtitle stream and generation. Events for older generations
/// or non-selected streams are ignored, which lets seek/flush and track-switch operations invalidate
/// already decoded-but-stale subtitle buffers without involving a Wayland compositor.
#[derive(Debug)]
pub struct SubtitleScheduler<P> {
    stream_id: String,
    generation: u64,
    next_sequence: u64,
    active: Option<ActiveSubtitle>,
    pending: Vec<PendingEvent<P>>,
}

impl<P> SubtitleScheduler<P> {
    /// Create a scheduler for the currently selected subtitle stream generation.
    pub fn new(stream_id: impl Into<String>, generation: u64) -> Self {
        Self {
            stream_id: stream_id.into(),
            generation,
            next_sequence: 0,
            active: None,
            pending: Vec::new(),
        }
    }

    /// Queue a decoded subtitle event.
    ///
    /// Returns `false` when the event belongs to a stale generation, a non-selected stream, or an
    /// empty/negative presentation interval.
    pub fn push_event(&mut self, event: DecodedSubtitleEvent<P>) -> bool {
        match event {
            DecodedSubtitleEvent::Show(cue) => self.push_show(cue),
            DecodedSubtitleEvent::Clear(clear) => self.push_clear(clear),
        }
    }

    /// Advance the selected stream generation after a seek or decoder flush.
    ///
    /// Pending events are discarded. If a subtitle is currently attached, a clear action is returned
    /// so the renderer can remove stale subtitle pixels immediately.
    pub fn flush_generation(&mut self, generation: u64) -> Option<SubtitleAction<P>> {
        self.generation = generation;
        self.pending.clear();
        self.active.take().map(clear_action_for_active)
    }

    /// Switch to a different subtitle stream and generation.
    ///
    /// Pending events from the previous stream are discarded. If a subtitle is currently attached, a
    /// clear action is returned so track changes do not leave stale subtitle pixels visible.
    pub(crate) fn switch_stream(
        &mut self,
        stream_id: impl Into<String>,
        generation: u64,
    ) -> Option<SubtitleAction<P>> {
        self.stream_id = stream_id.into();
        self.generation = generation;
        self.pending.clear();
        self.active.take().map(clear_action_for_active)
    }

    /// Drain all actions due at or before `running_time`.
    pub fn drain_due(&mut self, running_time: Duration) -> Vec<SubtitleAction<P>> {
        let mut actions = Vec::new();

        loop {
            let active_due = self.active.as_ref().and_then(|active| {
                active.end.and_then(|end| {
                    (end <= running_time).then_some(DueOrder {
                        time: end,
                        priority: DuePriority::Clear,
                        sequence: active.sequence,
                    })
                })
            });

            let pending_due = self.next_pending_due(running_time);

            match (active_due, pending_due) {
                (None, None) => break,
                (Some(_), None) => {
                    if let Some(active) = self.active.take() {
                        actions.push(clear_action_for_active(active));
                    }
                }
                (None, Some((index, _))) => {
                    self.apply_pending_event(index, running_time, &mut actions);
                }
                (Some(active_order), Some((index, pending_order))) => {
                    if active_order <= pending_order {
                        if let Some(active) = self.active.take() {
                            actions.push(clear_action_for_active(active));
                        }
                    } else {
                        self.apply_pending_event(index, running_time, &mut actions);
                    }
                }
            }
        }

        actions
    }

    fn push_show(&mut self, cue: DecodedSubtitleCue<P>) -> bool {
        if !self.is_current(&cue.stream_id, cue.generation) {
            return false;
        }

        if cue.end.is_some_and(|end| end <= cue.start) {
            return false;
        }

        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.pending
            .push(PendingEvent::Show(PendingShow { sequence, cue }));
        true
    }

    fn push_clear(&mut self, clear: DecodedSubtitleClear) -> bool {
        if !self.is_current(&clear.stream_id, clear.generation) {
            return false;
        }

        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.pending
            .push(PendingEvent::Clear(PendingClear { sequence, clear }));
        true
    }

    fn is_current(&self, stream_id: &str, generation: u64) -> bool {
        self.stream_id == stream_id && self.generation == generation
    }

    fn next_pending_due(&self, running_time: Duration) -> Option<(usize, DueOrder)> {
        self.pending
            .iter()
            .enumerate()
            .filter_map(|(index, event)| {
                let order = event.due_order();
                (order.time <= running_time).then_some((index, order))
            })
            .min_by_key(|(_, order)| *order)
    }

    fn apply_pending_event(
        &mut self,
        index: usize,
        running_time: Duration,
        actions: &mut Vec<SubtitleAction<P>>,
    ) {
        match self.pending.remove(index) {
            PendingEvent::Show(pending) => self.apply_show(pending, running_time, actions),
            PendingEvent::Clear(pending) => self.apply_clear(pending, actions),
        }
    }

    fn apply_show(
        &mut self,
        pending: PendingShow<P>,
        running_time: Duration,
        actions: &mut Vec<SubtitleAction<P>>,
    ) {
        let cue = pending.cue;
        if !self.is_current(&cue.stream_id, cue.generation) {
            return;
        }

        if cue.end.is_some_and(|end| end <= running_time) {
            return;
        }

        log_attach_timing(&cue, running_time);

        self.active = Some(ActiveSubtitle {
            sequence: pending.sequence,
            stream_id: cue.stream_id.clone(),
            generation: cue.generation,
            start: cue.start,
            end: cue.end,
        });

        actions.push(SubtitleAction::Attach(SubtitleAttach {
            stream_id: cue.stream_id,
            generation: cue.generation,
            start: cue.start,
            end: cue.end,
            payload: cue.payload,
        }));
    }

    fn apply_clear(&mut self, pending: PendingClear, actions: &mut Vec<SubtitleAction<P>>) {
        let clear = pending.clear;
        if !self.is_current(&clear.stream_id, clear.generation) {
            return;
        }

        let should_clear = self.active.as_ref().is_some_and(|active| {
            active.stream_id == clear.stream_id
                && active.generation == clear.generation
                && active.start <= clear.at
        });

        if !should_clear {
            return;
        }

        let active = self
            .active
            .take()
            .expect("active subtitle exists when clear action is due");
        actions.push(clear_action_for_active(active));
    }
}

#[derive(Clone, Debug)]
struct ActiveSubtitle {
    sequence: u64,
    stream_id: String,
    generation: u64,
    start: Duration,
    end: Option<Duration>,
}

#[derive(Clone, Debug)]
struct PendingShow<P> {
    sequence: u64,
    cue: DecodedSubtitleCue<P>,
}

#[derive(Clone, Debug)]
struct PendingClear {
    sequence: u64,
    clear: DecodedSubtitleClear,
}

#[derive(Clone, Debug)]
enum PendingEvent<P> {
    Show(PendingShow<P>),
    Clear(PendingClear),
}

impl<P> PendingEvent<P> {
    fn due_order(&self) -> DueOrder {
        match self {
            Self::Show(show) => DueOrder {
                time: show.cue.start,
                priority: DuePriority::Attach,
                sequence: show.sequence,
            },
            Self::Clear(clear) => DueOrder {
                time: clear.clear.at,
                priority: DuePriority::Clear,
                sequence: clear.sequence,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct DueOrder {
    time: Duration,
    priority: DuePriority,
    sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum DuePriority {
    Clear,
    Attach,
}

const SUBTITLE_SYNC_LOG_TARGET: &str = "subwave_wayland::subtitle_sync";
const SUBTITLE_SYNC_DEBUG_ENV: &str = "SUBWAVE_WAYLAND_SUBTITLE_SYNC_DEBUG";

fn log_attach_timing<P>(cue: &DecodedSubtitleCue<P>, presentation_time: Duration) {
    if !subtitle_sync_debug_enabled()
        || !log::log_enabled!(target: SUBTITLE_SYNC_LOG_TARGET, log::Level::Debug)
    {
        return;
    }

    log::debug!(
        target: SUBTITLE_SYNC_LOG_TARGET,
        "[subs-sync] attach stream={} generation={} cue_start_ms={:.3} \
         presentation_media_time_ms={:.3} delta_ms={:+.3}",
        cue.stream_id,
        cue.generation,
        duration_ms(cue.start),
        duration_ms(presentation_time),
        signed_delta_ms(presentation_time, cue.start),
    );
}

fn subtitle_sync_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var(SUBTITLE_SYNC_DEBUG_ENV).is_ok_and(|value| env_flag_enabled(&value))
    })
}

fn env_flag_enabled(value: &str) -> bool {
    let value = value.trim();
    value == "1"
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("on")
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn signed_delta_ms(presentation_time: Duration, cue_start: Duration) -> f64 {
    if presentation_time >= cue_start {
        duration_ms(presentation_time - cue_start)
    } else {
        -duration_ms(cue_start - presentation_time)
    }
}

fn clear_action_for_active<P>(active: ActiveSubtitle) -> SubtitleAction<P> {
    SubtitleAction::Clear(SubtitleClearAction {
        stream_id: active.stream_id,
        generation: active.generation,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        env_flag_enabled, signed_delta_ms, DecodedSubtitleEvent, SubtitleAction, SubtitleAttach,
        SubtitleClearAction, SubtitleScheduler,
    };
    use std::time::Duration;

    const STREAM: &str = "text/en";
    const ALT_STREAM: &str = "text/es";

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn show(
        text: &'static str,
        start_ms: u64,
        end_ms: Option<u64>,
        generation: u64,
    ) -> DecodedSubtitleEvent<&'static str> {
        DecodedSubtitleEvent::show(STREAM, generation, ms(start_ms), end_ms.map(ms), text)
    }

    fn alt_show(
        text: &'static str,
        start_ms: u64,
        end_ms: Option<u64>,
        generation: u64,
    ) -> DecodedSubtitleEvent<&'static str> {
        DecodedSubtitleEvent::show(ALT_STREAM, generation, ms(start_ms), end_ms.map(ms), text)
    }

    fn clear(at_ms: u64, generation: u64) -> DecodedSubtitleEvent<&'static str> {
        DecodedSubtitleEvent::clear(STREAM, generation, ms(at_ms))
    }

    fn attach(
        text: &'static str,
        start_ms: u64,
        end_ms: Option<u64>,
        generation: u64,
    ) -> SubtitleAction<&'static str> {
        SubtitleAction::Attach(SubtitleAttach {
            stream_id: STREAM.to_string(),
            generation,
            start: ms(start_ms),
            end: end_ms.map(ms),
            payload: text,
        })
    }

    fn alt_attach(
        text: &'static str,
        start_ms: u64,
        end_ms: Option<u64>,
        generation: u64,
    ) -> SubtitleAction<&'static str> {
        SubtitleAction::Attach(SubtitleAttach {
            stream_id: ALT_STREAM.to_string(),
            generation,
            start: ms(start_ms),
            end: end_ms.map(ms),
            payload: text,
        })
    }

    fn clear_action(generation: u64) -> SubtitleAction<&'static str> {
        SubtitleAction::Clear(SubtitleClearAction {
            stream_id: STREAM.to_string(),
            generation,
        })
    }

    fn alt_clear_action(generation: u64) -> SubtitleAction<&'static str> {
        SubtitleAction::Clear(SubtitleClearAction {
            stream_id: ALT_STREAM.to_string(),
            generation,
        })
    }

    #[test]
    fn early_cue_arrival_is_not_due_before_start() {
        let mut scheduler = SubtitleScheduler::new(STREAM, 0);

        assert!(scheduler.push_event(show("hello", 1_000, Some(2_000), 0)));

        assert!(scheduler.drain_due(ms(999)).is_empty());
    }

    #[test]
    fn late_tick_attaches_overdue_cue_with_expected_delta() {
        let mut scheduler = SubtitleScheduler::new(STREAM, 0);

        assert!(scheduler.push_event(show("hello", 1_000, Some(2_000), 0)));

        assert_eq!(signed_delta_ms(ms(1_250), ms(1_000)), 250.0);
        assert_eq!(
            scheduler.drain_due(ms(1_250)),
            vec![attach("hello", 1_000, Some(2_000), 0)]
        );
        assert!(scheduler.drain_due(ms(1_250)).is_empty());
    }

    #[test]
    fn sync_debug_flag_accepts_explicit_truthy_values_only() {
        assert!(env_flag_enabled("1"));
        assert!(env_flag_enabled(" true "));
        assert!(env_flag_enabled("YES"));
        assert!(env_flag_enabled("on"));
        assert!(!env_flag_enabled(""));
        assert!(!env_flag_enabled("0"));
        assert!(!env_flag_enabled("false"));
    }

    #[test]
    fn cue_start_attaches_when_running_time_reaches_start() {
        let mut scheduler = SubtitleScheduler::new(STREAM, 0);

        assert!(scheduler.push_event(show("hello", 1_000, Some(2_000), 0)));

        assert_eq!(
            scheduler.drain_due(ms(1_000)),
            vec![attach("hello", 1_000, Some(2_000), 0)]
        );
    }

    #[test]
    fn cue_end_clears_the_active_subtitle() {
        let mut scheduler = SubtitleScheduler::new(STREAM, 0);

        assert!(scheduler.push_event(show("hello", 0, Some(1_000), 0)));
        assert_eq!(
            scheduler.drain_due(ms(0)),
            vec![attach("hello", 0, Some(1_000), 0)]
        );
        assert!(scheduler.drain_due(ms(999)).is_empty());

        assert_eq!(scheduler.drain_due(ms(1_000)), vec![clear_action(0)]);
    }

    #[test]
    fn explicit_clear_event_clears_the_active_subtitle() {
        let mut scheduler = SubtitleScheduler::new(STREAM, 0);

        assert!(scheduler.push_event(show("hello", 0, None, 0)));
        assert_eq!(
            scheduler.drain_due(ms(0)),
            vec![attach("hello", 0, None, 0)]
        );
        assert!(scheduler.push_event(clear(500, 0)));
        assert!(scheduler.drain_due(ms(499)).is_empty());

        assert_eq!(scheduler.drain_due(ms(500)), vec![clear_action(0)]);
    }

    #[test]
    fn overlapping_cue_replaces_active_without_old_end_clearing_replacement() {
        let mut scheduler = SubtitleScheduler::new(STREAM, 0);

        assert!(scheduler.push_event(show("first", 0, Some(5_000), 0)));
        assert!(scheduler.push_event(show("second", 3_000, Some(7_000), 0)));

        assert_eq!(
            scheduler.drain_due(ms(0)),
            vec![attach("first", 0, Some(5_000), 0)]
        );
        assert_eq!(
            scheduler.drain_due(ms(3_000)),
            vec![attach("second", 3_000, Some(7_000), 0)]
        );
        assert!(scheduler.drain_due(ms(5_000)).is_empty());
        assert_eq!(scheduler.drain_due(ms(7_000)), vec![clear_action(0)]);
    }

    #[test]
    fn seek_flush_generation_discards_pending_and_rejects_stale_events() {
        let mut scheduler = SubtitleScheduler::new(STREAM, 0);

        assert!(scheduler.push_event(show("before seek", 1_000, Some(2_000), 0)));
        let clear = scheduler.flush_generation(1);

        assert!(clear.is_none());
        assert!(scheduler.drain_due(ms(1_000)).is_empty());
        assert!(!scheduler.push_event(show("stale", 1_000, Some(2_000), 0)));
        assert!(scheduler.push_event(show("after seek", 1_500, Some(2_500), 1)));

        assert_eq!(
            scheduler.drain_due(ms(1_500)),
            vec![attach("after seek", 1_500, Some(2_500), 1)]
        );
    }

    #[test]
    fn seek_flush_generation_clears_active_subtitle() {
        let mut scheduler = SubtitleScheduler::new(STREAM, 0);

        assert!(scheduler.push_event(show("before seek", 0, Some(2_000), 0)));
        assert_eq!(
            scheduler.drain_due(ms(0)),
            vec![attach("before seek", 0, Some(2_000), 0)]
        );

        assert_eq!(scheduler.flush_generation(1), Some(clear_action(0)));
        assert!(scheduler.drain_due(ms(2_000)).is_empty());
    }

    #[test]
    fn track_switch_clears_active_and_invalidates_old_stream_events() {
        let mut scheduler = SubtitleScheduler::new(STREAM, 0);

        assert!(scheduler.push_event(show("english", 0, Some(2_000), 0)));
        assert!(scheduler.push_event(show("queued english", 750, Some(1_250), 0)));
        assert_eq!(
            scheduler.drain_due(ms(0)),
            vec![attach("english", 0, Some(2_000), 0)]
        );

        assert_eq!(
            scheduler.switch_stream(ALT_STREAM, 0),
            Some(clear_action(0))
        );
        assert!(!scheduler.push_event(show("stale english", 500, Some(1_000), 0)));
        assert!(scheduler.push_event(alt_show("spanish", 500, Some(1_000), 0)));

        assert_eq!(
            scheduler.drain_due(ms(750)),
            vec![alt_attach("spanish", 500, Some(1_000), 0)]
        );
        assert_eq!(scheduler.drain_due(ms(1_000)), vec![alt_clear_action(0)]);
    }
}
