use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SampleWindowMetrics {
    pub peak: f32,
    pub rms: f32,
    pub mean: f32,
    pub dc_offset: f32,
    pub zero_crossing_rate: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SilenceClass {
    Silent,
    NearSilent,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GainPlan {
    pub input_peak: f32,
    pub target_peak: f32,
    pub applied_gain: f32,
    pub limited: bool,
}

pub fn normalize_i16_to_f32(sample: i16) -> f32 {
    sample as f32 / i16::MAX as f32
}

pub fn compute_peak(samples: &[i16]) -> f32 {
    samples
        .iter()
        .map(|&sample| normalize_i16_to_f32(sample).abs())
        .fold(0.0, f32::max)
}

pub fn compute_rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let energy: f32 = samples
        .iter()
        .map(|&sample| {
            let value = normalize_i16_to_f32(sample);
            value * value
        })
        .sum();
    sqrt_newton(energy / samples.len() as f32)
}

fn sqrt_newton(value: f32) -> f32 {
    if value <= 0.0 {
        return 0.0;
    }
    let mut x = if value >= 1.0 { value } else { 1.0 };
    for _ in 0..8 {
        x = 0.5 * (x + value / x);
    }
    x
}

pub fn compute_mean(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples
        .iter()
        .map(|&sample| normalize_i16_to_f32(sample))
        .sum();
    sum / samples.len() as f32
}

pub fn compute_zero_crossing_rate(samples: &[i16]) -> f32 {
    if samples.len() <= 1 {
        return 0.0;
    }
    let mut crossings = 0usize;
    for pair in samples.windows(2) {
        let a = pair[0];
        let b = pair[1];
        if (a < 0 && b >= 0) || (a >= 0 && b < 0) {
            crossings += 1;
        }
    }
    crossings as f32 / (samples.len() - 1) as f32
}

pub fn analyze_window(samples: &[i16]) -> SampleWindowMetrics {
    let mean = compute_mean(samples);
    SampleWindowMetrics {
        peak: compute_peak(samples),
        rms: compute_rms(samples),
        mean,
        dc_offset: mean.abs(),
        zero_crossing_rate: compute_zero_crossing_rate(samples),
    }
}

pub fn classify_silence(metrics: SampleWindowMetrics, threshold: f32) -> SilenceClass {
    if metrics.peak <= threshold * 0.5 && metrics.rms <= threshold * 0.5 {
        SilenceClass::Silent
    } else if metrics.peak <= threshold && metrics.rms <= threshold {
        SilenceClass::NearSilent
    } else {
        SilenceClass::Active
    }
}

pub fn suggest_gain(metrics: SampleWindowMetrics, target_peak: f32, max_gain: f32) -> GainPlan {
    if metrics.peak <= 0.0001 {
        return GainPlan {
            input_peak: metrics.peak,
            target_peak,
            applied_gain: 1.0,
            limited: false,
        };
    }
    let ideal_gain = target_peak / metrics.peak;
    let limited = ideal_gain > max_gain;
    GainPlan {
        input_peak: metrics.peak,
        target_peak,
        applied_gain: if limited { max_gain } else { ideal_gain },
        limited,
    }
}

pub fn apply_gain(samples: &[i16], gain: f32) -> Vec<i16> {
    samples
        .iter()
        .map(|&sample| {
            let scaled = sample as f32 * gain;
            scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
        })
        .collect()
}

pub fn rolling_rms(samples: &[i16], window: usize) -> Vec<f32> {
    if window == 0 || samples.is_empty() {
        return Vec::new();
    }
    let mut output = Vec::new();
    for start in 0..samples.len() {
        let end = (start + window).min(samples.len());
        output.push(compute_rms(&samples[start..end]));
        if end == samples.len() {
            break;
        }
    }
    output
}

pub fn detect_transients(samples: &[i16], threshold: f32) -> Vec<usize> {
    let mut indices = Vec::new();
    for (idx, pair) in samples.windows(2).enumerate() {
        let delta = (pair[1] as i32 - pair[0] as i32).unsigned_abs() as f32 / i16::MAX as f32;
        if delta >= threshold {
            indices.push(idx + 1);
        }
    }
    indices
}

pub fn compute_channel_balance(left: &[i16], right: &[i16]) -> f32 {
    let left_rms = compute_rms(left);
    let right_rms = compute_rms(right);
    if right_rms == 0.0 {
        left_rms
    } else {
        left_rms / right_rms
    }
}

pub fn split_interleaved_stereo(samples: &[i16]) -> (Vec<i16>, Vec<i16>) {
    let mut left = Vec::with_capacity(samples.len() / 2);
    let mut right = Vec::with_capacity(samples.len() / 2);
    for frame in samples.chunks(2) {
        if let Some(&l) = frame.first() {
            left.push(l);
        }
        if frame.len() > 1 {
            right.push(frame[1]);
        }
    }
    (left, right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn test_compute_peak_and_rms() {
        let samples = [0i16, 16384, -16384, 8192];
        let peak = compute_peak(&samples);
        let rms = compute_rms(&samples);
        assert!(peak > 0.4);
        assert!(rms > 0.2);
    }

    #[test]
    fn test_classify_silence() {
        let silent = analyze_window(&[0i16; 32]);
        assert_eq!(classify_silence(silent, 0.01), SilenceClass::Silent);
    }

    #[test]
    fn test_gain_plan() {
        let metrics = analyze_window(&[4096i16, -4096, 2048, -2048]);
        let plan = suggest_gain(metrics, 0.8, 4.0);
        assert!(plan.applied_gain >= 1.0);
    }

    #[test]
    fn test_apply_gain() {
        let out = apply_gain(&[1000i16, -1000], 2.0);
        assert_eq!(out, vec![2000, -2000]);
    }

    #[test]
    fn test_detect_transients() {
        let samples = [0i16, 128, 30000, 31000, 0];
        let indices = detect_transients(&samples, 0.5);
        assert!(!indices.is_empty());
    }

    #[test]
    fn test_split_stereo() {
        let (left, right) = split_interleaved_stereo(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(left, vec![1, 3, 5]);
        assert_eq!(right, vec![2, 4, 6]);
    }
}
