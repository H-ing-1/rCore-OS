use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadClass {
    VoiceAssistant,
    MultimediaPlayback,
    NotificationBurst,
    GameAudio,
    LowLatencyMonitor,
    BackgroundCapture,
    StudioRender,
    Telemetry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulingHint {
    ThroughputFirst,
    LatencyFirst,
    PowerSaving,
    Balanced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuePressure {
    Idle,
    Stable,
    Busy,
    Saturated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioWorkloadProfile {
    pub class: WorkloadClass,
    pub channels: u8,
    pub sample_rate: u32,
    pub frame_bytes: usize,
    pub target_latency_ms: u32,
    pub burst_frames: usize,
    pub sustained_frames: usize,
    pub control_ops_per_second: u32,
    pub duplex: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferPlan {
    pub buffer_bytes: u32,
    pub period_bytes: u32,
    pub periods: u16,
    pub prefetched_periods: u16,
    pub irq_interval_frames: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueBudget {
    pub descriptors_per_transfer: u16,
    pub max_inflight_transfers: u16,
    pub reserved_control_descriptors: u16,
    pub reserved_event_descriptors: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPlan {
    pub buffer: BufferPlan,
    pub queue: QueueBudget,
    pub hint: SchedulingHint,
    pub pressure: QueuePressure,
    pub score: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannerSnapshot {
    pub inflight_transfers: usize,
    pub completed_transfers: usize,
    pub queued_control_ops: usize,
    pub average_batch_frames: usize,
}

impl AudioWorkloadProfile {
    pub const fn stereo_music(sample_rate: u32) -> Self {
        Self {
            class: WorkloadClass::MultimediaPlayback,
            channels: 2,
            sample_rate,
            frame_bytes: 4,
            target_latency_ms: 40,
            burst_frames: 512,
            sustained_frames: 1024,
            control_ops_per_second: 4,
            duplex: false,
        }
    }

    pub const fn interactive_voice(sample_rate: u32) -> Self {
        Self {
            class: WorkloadClass::VoiceAssistant,
            channels: 1,
            sample_rate,
            frame_bytes: 2,
            target_latency_ms: 15,
            burst_frames: 160,
            sustained_frames: 320,
            control_ops_per_second: 20,
            duplex: true,
        }
    }

    pub const fn competitive_game(sample_rate: u32) -> Self {
        Self {
            class: WorkloadClass::GameAudio,
            channels: 2,
            sample_rate,
            frame_bytes: 4,
            target_latency_ms: 12,
            burst_frames: 128,
            sustained_frames: 256,
            control_ops_per_second: 12,
            duplex: false,
        }
    }

    pub const fn studio_render(sample_rate: u32, channels: u8) -> Self {
        Self {
            class: WorkloadClass::StudioRender,
            channels,
            sample_rate,
            frame_bytes: 8,
            target_latency_ms: 80,
            burst_frames: 1024,
            sustained_frames: 4096,
            control_ops_per_second: 2,
            duplex: false,
        }
    }
}

pub fn estimate_frame_bytes(channels: u8, bits_per_sample: u8) -> usize {
    let bytes_per_channel = (bits_per_sample as usize).div_ceil(8);
    channels as usize * bytes_per_channel
}

pub fn frames_to_bytes(frames: usize, frame_bytes: usize) -> usize {
    frames.saturating_mul(frame_bytes)
}

pub fn bytes_to_frames(bytes: usize, frame_bytes: usize) -> usize {
    bytes.checked_div(frame_bytes).unwrap_or(0)
}

pub fn latency_ms_for_frames(frames: usize, sample_rate: u32) -> u32 {
    if sample_rate == 0 {
        0
    } else {
        ((frames as u64 * 1000) / sample_rate as u64) as u32
    }
}

pub fn align_up(value: usize, align: usize) -> usize {
    if align <= 1 {
        value
    } else {
        let rem = value % align;
        if rem == 0 {
            value
        } else {
            value + (align - rem)
        }
    }
}

pub fn choose_period_count(profile: &AudioWorkloadProfile, hint: SchedulingHint) -> u16 {
    match (profile.class, hint) {
        (WorkloadClass::VoiceAssistant, SchedulingHint::LatencyFirst) => 2,
        (WorkloadClass::GameAudio, SchedulingHint::LatencyFirst) => 2,
        (WorkloadClass::StudioRender, SchedulingHint::ThroughputFirst) => 8,
        (WorkloadClass::BackgroundCapture, SchedulingHint::PowerSaving) => 6,
        (WorkloadClass::NotificationBurst, SchedulingHint::Balanced) => 3,
        (_, SchedulingHint::PowerSaving) => 5,
        (_, SchedulingHint::ThroughputFirst) => 6,
        (_, SchedulingHint::LatencyFirst) => 3,
        _ => 4,
    }
}

pub fn choose_prefetched_periods(periods: u16, pressure: QueuePressure) -> u16 {
    match pressure {
        QueuePressure::Idle => periods.min(1),
        QueuePressure::Stable => periods.min(2),
        QueuePressure::Busy => periods.min(3),
        QueuePressure::Saturated => periods.min(4),
    }
}

pub fn derive_queue_pressure(snapshot: PlannerSnapshot) -> QueuePressure {
    let inflight = snapshot.inflight_transfers;
    let control = snapshot.queued_control_ops;
    if inflight == 0 && control == 0 {
        QueuePressure::Idle
    } else if inflight < 8 && control < 4 {
        QueuePressure::Stable
    } else if inflight < 24 && control < 12 {
        QueuePressure::Busy
    } else {
        QueuePressure::Saturated
    }
}

pub fn choose_scheduling_hint(profile: &AudioWorkloadProfile) -> SchedulingHint {
    match profile.class {
        WorkloadClass::VoiceAssistant
        | WorkloadClass::GameAudio
        | WorkloadClass::LowLatencyMonitor => SchedulingHint::LatencyFirst,
        WorkloadClass::StudioRender => SchedulingHint::ThroughputFirst,
        WorkloadClass::Telemetry | WorkloadClass::BackgroundCapture => SchedulingHint::PowerSaving,
        _ => SchedulingHint::Balanced,
    }
}

pub fn build_buffer_plan(
    profile: &AudioWorkloadProfile,
    hint: SchedulingHint,
    pressure: QueuePressure,
) -> BufferPlan {
    let period_count = choose_period_count(profile, hint);
    let base_period_frames = match hint {
        SchedulingHint::LatencyFirst => profile.burst_frames.max(64),
        SchedulingHint::ThroughputFirst => profile.sustained_frames.max(profile.burst_frames),
        SchedulingHint::PowerSaving => (profile.sustained_frames * 3 / 2).max(profile.burst_frames),
        SchedulingHint::Balanced => {
            ((profile.burst_frames + profile.sustained_frames) / 2).max(profile.burst_frames)
        }
    };

    let irq_interval_frames = match pressure {
        QueuePressure::Idle => base_period_frames / 2,
        QueuePressure::Stable => base_period_frames,
        QueuePressure::Busy => base_period_frames + base_period_frames / 4,
        QueuePressure::Saturated => base_period_frames + base_period_frames / 2,
    }
    .max(32);

    let period_bytes =
        align_up(frames_to_bytes(base_period_frames, profile.frame_bytes), 64) as u32;
    let buffer_bytes = period_bytes.saturating_mul(period_count as u32);

    BufferPlan {
        buffer_bytes,
        period_bytes,
        periods: period_count,
        prefetched_periods: choose_prefetched_periods(period_count, pressure),
        irq_interval_frames,
    }
}

pub fn build_queue_budget(
    profile: &AudioWorkloadProfile,
    buffer: BufferPlan,
    pressure: QueuePressure,
) -> QueueBudget {
    let descriptors_per_transfer = if profile.duplex { 4 } else { 3 };
    let base_inflight = match pressure {
        QueuePressure::Idle => 4,
        QueuePressure::Stable => 8,
        QueuePressure::Busy => 12,
        QueuePressure::Saturated => 16,
    };
    let inflation =
        (buffer.prefetched_periods as usize).saturating_mul(descriptors_per_transfer as usize);
    let max_inflight = (base_inflight + inflation / 3).min(64) as u16;

    QueueBudget {
        descriptors_per_transfer,
        max_inflight_transfers: max_inflight,
        reserved_control_descriptors: profile.control_ops_per_second.clamp(2, 32) as u16,
        reserved_event_descriptors: if profile.duplex { 8 } else { 4 },
    }
}

pub fn score_plan(profile: &AudioWorkloadProfile, buffer: BufferPlan, queue: QueueBudget) -> i32 {
    let latency_frames = bytes_to_frames(buffer.period_bytes as usize, profile.frame_bytes)
        * buffer.periods as usize;
    let latency_ms = latency_ms_for_frames(latency_frames, profile.sample_rate) as i32;
    let target_latency = profile.target_latency_ms as i32;
    let latency_penalty = (latency_ms - target_latency).abs();
    let queue_bonus =
        queue.max_inflight_transfers as i32 - queue.reserved_control_descriptors as i32 / 2;
    let prefetch_bonus = buffer.prefetched_periods as i32 * 3;
    200 - latency_penalty + queue_bonus + prefetch_bonus
}

pub fn build_stream_plan(profile: &AudioWorkloadProfile, snapshot: PlannerSnapshot) -> StreamPlan {
    let pressure = derive_queue_pressure(snapshot);
    let hint = choose_scheduling_hint(profile);
    let buffer = build_buffer_plan(profile, hint, pressure);
    let queue = build_queue_budget(profile, buffer, pressure);
    let score = score_plan(profile, buffer, queue);
    StreamPlan {
        buffer,
        queue,
        hint,
        pressure,
        score,
    }
}

pub fn candidate_plans(profile: &AudioWorkloadProfile) -> Vec<StreamPlan> {
    let hints = [
        SchedulingHint::Balanced,
        SchedulingHint::LatencyFirst,
        SchedulingHint::ThroughputFirst,
        SchedulingHint::PowerSaving,
    ];
    let pressures = [
        QueuePressure::Idle,
        QueuePressure::Stable,
        QueuePressure::Busy,
        QueuePressure::Saturated,
    ];
    let mut plans = Vec::new();
    for hint in hints {
        for pressure in pressures {
            let buffer = build_buffer_plan(profile, hint, pressure);
            let queue = build_queue_budget(profile, buffer, pressure);
            let score = score_plan(profile, buffer, queue);
            plans.push(StreamPlan {
                buffer,
                queue,
                hint,
                pressure,
                score,
            });
        }
    }
    plans
}

pub fn select_best_plan(profile: &AudioWorkloadProfile) -> StreamPlan {
    let candidates = candidate_plans(profile);
    let mut best = candidates[0];
    for plan in candidates.into_iter().skip(1) {
        if plan.score > best.score {
            best = plan;
        }
    }
    best
}

pub fn split_transfer_schedule(total_frames: usize, period_frames: usize) -> Vec<usize> {
    let mut remaining = total_frames;
    let mut schedule = Vec::new();
    let chunk = period_frames.max(1);
    while remaining > 0 {
        let current = remaining.min(chunk);
        schedule.push(current);
        remaining -= current;
    }
    schedule
}

pub fn compute_jitter_score(schedule: &[usize]) -> u32 {
    if schedule.len() <= 1 {
        return 0;
    }
    let avg = schedule.iter().sum::<usize>() as i64 / schedule.len() as i64;
    schedule
        .iter()
        .map(|value| (*value as i64 - avg).unsigned_abs() as u32)
        .sum()
}

pub fn redistribute_schedule(schedule: &[usize], target_chunk: usize) -> Vec<usize> {
    if schedule.is_empty() {
        return Vec::new();
    }
    let total: usize = schedule.iter().sum();
    split_transfer_schedule(total, target_chunk.max(1))
}

pub fn classify_latency_bucket(latency_ms: u32) -> &'static str {
    match latency_ms {
        0..=10 => "ultra-low",
        11..=20 => "low",
        21..=50 => "interactive",
        51..=120 => "media",
        _ => "background",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_frame_bytes() {
        assert_eq!(estimate_frame_bytes(2, 16), 4);
        assert_eq!(estimate_frame_bytes(6, 24), 18);
    }

    #[test]
    fn test_align_up() {
        assert_eq!(align_up(63, 64), 64);
        assert_eq!(align_up(128, 64), 128);
    }

    #[test]
    fn test_pressure_derivation() {
        assert_eq!(
            derive_queue_pressure(PlannerSnapshot {
                inflight_transfers: 0,
                completed_transfers: 0,
                queued_control_ops: 0,
                average_batch_frames: 0,
            }),
            QueuePressure::Idle
        );
        assert_eq!(
            derive_queue_pressure(PlannerSnapshot {
                inflight_transfers: 20,
                completed_transfers: 3,
                queued_control_ops: 6,
                average_batch_frames: 256,
            }),
            QueuePressure::Busy
        );
    }

    #[test]
    fn test_build_stream_plan() {
        let profile = AudioWorkloadProfile::competitive_game(48000);
        let plan = build_stream_plan(
            &profile,
            PlannerSnapshot {
                inflight_transfers: 3,
                completed_transfers: 5,
                queued_control_ops: 1,
                average_batch_frames: 128,
            },
        );
        assert!(plan.buffer.period_bytes > 0);
        assert!(plan.queue.max_inflight_transfers >= 4);
    }

    #[test]
    fn test_candidate_plan_generation() {
        let profile = AudioWorkloadProfile::stereo_music(44100);
        let plans = candidate_plans(&profile);
        assert_eq!(plans.len(), 16);
    }

    #[test]
    fn test_schedule_split_and_redistribute() {
        let schedule = split_transfer_schedule(1000, 256);
        assert_eq!(schedule.iter().sum::<usize>(), 1000);
        let redistributed = redistribute_schedule(&schedule, 128);
        assert_eq!(redistributed.iter().sum::<usize>(), 1000);
        assert!(redistributed.len() >= schedule.len());
    }

    #[test]
    fn test_classify_latency_bucket() {
        assert_eq!(classify_latency_bucket(8), "ultra-low");
        assert_eq!(classify_latency_bucket(18), "low");
        assert_eq!(classify_latency_bucket(42), "interactive");
        assert_eq!(classify_latency_bucket(90), "media");
        assert_eq!(classify_latency_bucket(160), "background");
    }
}
