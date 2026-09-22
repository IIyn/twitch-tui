//! Audio visualizer: FFT spectrum with smoothing, peaks and several styles.

use audio::SAMPLE_RATE;
use term::{Rect, Rgb, Screen};

use crate::theme;

const FFT_SIZE: usize = 2048;
const MIN_FREQ: f32 = 35.0;
const MAX_FREQ: f32 = 16_000.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VizStyle {
    Bars,
    Mirror,
    Wave,
    /// The stream itself, drawn as coloured ASCII.
    Video,
}

impl VizStyle {
    pub fn next(self) -> VizStyle {
        match self {
            VizStyle::Bars => VizStyle::Mirror,
            VizStyle::Mirror => VizStyle::Wave,
            VizStyle::Wave => VizStyle::Video,
            VizStyle::Video => VizStyle::Bars,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            VizStyle::Bars => "spectrum",
            VizStyle::Mirror => "mirror",
            VizStyle::Wave => "scope",
            VizStyle::Video => "video",
        }
    }
}

pub struct Visualizer {
    pub style: VizStyle,
    bars: Vec<f32>,
    peaks: Vec<f32>,
    peak_vel: Vec<f32>,
    wave: Vec<f32>,
    /// Loudest recent band level in dB.
    ceiling: f32,
    window: Vec<f32>,
    phase: f32,
}

impl Visualizer {
    pub fn new() -> Visualizer {
        let window = (0..FFT_SIZE)
            .map(|i| {
                let x = i as f32 / (FFT_SIZE - 1) as f32;
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * x).cos()
            })
            .collect();
        Visualizer {
            style: VizStyle::Bars,
            bars: Vec::new(),
            peaks: Vec::new(),
            peak_vel: Vec::new(),
            wave: Vec::new(),
            ceiling: -20.0,
            window,
            phase: 0.0,
        }
    }

    /// Feeds the latest samples. `count` is the number of bars wanted.
    pub fn update(&mut self, samples: &[f32], count: usize, dt: f32) {
        self.phase += dt;
        if self.bars.len() != count {
            self.bars = vec![0.0; count];
            self.peaks = vec![0.0; count];
            self.peak_vel = vec![0.0; count];
        }
        let n = samples.len().min(FFT_SIZE);
        self.wave = samples[samples.len() - n..].to_vec();

        let mut targets = if n < FFT_SIZE / 2 {
            vec![f32::NEG_INFINITY; count]
        } else {
            self.spectrum(&samples[samples.len() - n..], count)
        };

        // Automatic range in the dB domain: follow the loudest band quickly
        // upwards, slowly downwards, and show the 45 dB below it.
        let loudest = targets.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        if loudest.is_finite() {
            let rate = if loudest > self.ceiling { 0.3 } else { dt * 0.4 };
            self.ceiling += (loudest - self.ceiling) * rate.min(1.0);
            self.ceiling = self.ceiling.max(-60.0);
        }
        for t in targets.iter_mut() {
            *t = ((*t - (self.ceiling - 45.0)) / 45.0).clamp(0.0, 1.0);
        }

        for (i, &level) in targets.iter().enumerate() {
            let target = level * 0.95;
            let bar = &mut self.bars[i];
            if target > *bar {
                *bar += (target - *bar) * 0.65;
            } else {
                *bar = (*bar - dt * 1.6).max(target);
            }
            if *bar >= self.peaks[i] {
                self.peaks[i] = *bar;
                self.peak_vel[i] = -0.35;
            } else {
                self.peak_vel[i] += dt * 2.2;
                self.peaks[i] = (self.peaks[i] - self.peak_vel[i].max(0.0) * dt).max(*bar);
            }
        }
    }

    /// Per-band level in dB (0 dB = full-scale sine).
    fn spectrum(&self, samples: &[f32], count: usize) -> Vec<f32> {
        let mut re: Vec<f32> = vec![0.0; FFT_SIZE];
        let mut im = vec![0.0; FFT_SIZE];
        let offset = FFT_SIZE - samples.len();
        for (i, s) in samples.iter().enumerate() {
            re[offset + i] = s * self.window[offset + i];
        }
        fft(&mut re, &mut im);

        let bin_hz = SAMPLE_RATE as f32 / FFT_SIZE as f32;
        let mag = |k: usize| (re[k] * re[k] + im[k] * im[k]).sqrt();
        // A full-scale sine through a Hann window peaks at N/4.
        let norm = FFT_SIZE as f32 / 4.0;
        let ratio = (MAX_FREQ / MIN_FREQ).ln();
        (0..count)
            .map(|b| {
                let lo = MIN_FREQ * (ratio * b as f32 / count as f32).exp();
                let hi = MIN_FREQ * (ratio * (b + 1) as f32 / count as f32).exp();
                let lo_bin = (lo / bin_hz).floor() as usize;
                let hi_bin = ((hi / bin_hz).ceil() as usize).max(lo_bin + 1).min(FFT_SIZE / 2);
                let peak = (lo_bin..hi_bin).map(mag).fold(0.0f32, f32::max) / norm;
                // Tilt the highs up a bit (+3 dB/octave) so the spectrum looks
                // balanced for typical music and voice.
                let tilt = 10.0 * (lo.max(MIN_FREQ) / MIN_FREQ).log2() * 0.3;
                20.0 * (peak + 1e-9).log10() + tilt
            })
            .collect()
    }

    pub fn draw(&self, s: &mut Screen, r: Rect, active: bool) {
        if r.w == 0 || r.h == 0 {
            return;
        }
        if !active {
            self.draw_idle(s, r);
            return;
        }
        match self.style {
            VizStyle::Bars => self.draw_bars(s, r),
            VizStyle::Mirror => self.draw_mirror(s, r),
            VizStyle::Wave => self.draw_wave(s, r),
            // Drawn by the interface, which owns the decoded frames.
            VizStyle::Video => {}
        }
    }

    /// How many bars fit in `width` columns for the current style.
    pub fn bar_count(&self, width: u16) -> usize {
        (width as usize).div_ceil(2).max(1)
    }

    fn draw_bars(&self, s: &mut Screen, r: Rect) {
        const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let rows = r.h as f32;
        for (i, (&v, &peak)) in self.bars.iter().zip(&self.peaks).enumerate() {
            let x = r.x + (i as u16) * 2;
            if x >= r.right() {
                break;
            }
            let eighths = (v * rows * 8.0).round() as usize;
            for row in 0..r.h {
                let y = r.bottom() - 1 - row;
                let level = eighths.saturating_sub(row as usize * 8).min(8);
                let color = theme::gradient(row as f32 / rows.max(1.0));
                if level > 0 {
                    s.put(x, y, BLOCKS[level], color);
                }
            }
            let peak_row = ((peak * rows).floor() as u16).min(r.h - 1);
            if peak > 0.02 && peak_row as usize * 8 >= eighths {
                let y = r.bottom() - 1 - peak_row;
                s.put(x, y, '▔', theme::gradient(peak).lerp(Rgb(255, 255, 255), 0.5));
            }
        }
    }

    fn draw_mirror(&self, s: &mut Screen, r: Rect) {
        // Half-block resolution: every cell holds two vertical units and the
        // bars grow symmetrically from the centre line.
        let total = r.h as i32 * 2;
        let center = r.h as i32;
        for (i, &v) in self.bars.iter().enumerate() {
            let x = r.x + (i as u16) * 2;
            if x >= r.right() {
                break;
            }
            let extent = (v * r.h as f32).round() as i32;
            let lit = |u: i32| u >= center - extent && u < center + extent;
            for row in 0..r.h {
                let (upper, lower) = (row as i32 * 2, row as i32 * 2 + 1);
                let dist = ((upper + 1 - center).abs() as f32) / (total as f32 / 2.0).max(1.0);
                let color = theme::gradient(dist);
                let ch = match (lit(upper), lit(lower)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    _ => continue,
                };
                s.put(x, r.y + row, ch, color);
            }
        }
    }

    fn draw_wave(&self, s: &mut Screen, r: Rect) {
        // Braille gives 2x4 dots per cell.
        let dots_w = r.w as usize * 2;
        let dots_h = r.h as usize * 4;
        if self.wave.is_empty() {
            return;
        }
        let mut grid = vec![0u8; r.w as usize * r.h as usize];
        // Show ~20ms starting at a rising zero crossing so the trace is stable.
        let span = (SAMPLE_RATE as usize / 50).min(self.wave.len() / 2).max(1);
        let search = self.wave.len() - span;
        let start = (1..search)
            .find(|&i| self.wave[i - 1] < 0.0 && self.wave[i] >= 0.0)
            .unwrap_or(search);
        let window = &self.wave[start..start + span];
        let peak = window.iter().fold(0.0f32, |m, v| m.max(v.abs())).max(0.05);
        let per_dot = span as f32 / dots_w as f32;
        let dot_sample = |dx: usize| {
            let a = (dx as f32 * per_dot) as usize;
            let b = (((dx + 1) as f32 * per_dot) as usize).clamp(a + 1, span);
            window[a..b].iter().sum::<f32>() / (b - a) as f32
        };
        let mut prev_y: Option<usize> = None;
        for dx in 0..dots_w {
            let sample = dot_sample(dx) / peak;
            let dy = (((1.0 - sample * 0.9) / 2.0) * (dots_h - 1) as f32).round() as usize;
            let dy = dy.min(dots_h - 1);
            let (from, to) = match prev_y {
                Some(p) if p < dy => (p + 1, dy),
                Some(p) if p > dy => (dy, p - 1),
                _ => (dy, dy),
            };
            for y in from..=to {
                let cell = (y / 4) * r.w as usize + dx / 2;
                grid[cell] |= braille_bit(dx % 2, y % 4);
            }
            prev_y = Some(dy);
        }
        for cy in 0..r.h {
            for cx in 0..r.w {
                let bits = grid[cy as usize * r.w as usize + cx as usize];
                if bits != 0 {
                    let ch = char::from_u32(0x2800 + bits as u32).unwrap_or(' ');
                    let t = (cy as f32 + 0.5) / r.h as f32;
                    let color = theme::gradient(1.0 - (t - 0.5).abs() * 2.0);
                    s.put(r.x + cx, r.y + cy, ch, color);
                }
            }
        }
    }

    fn draw_idle(&self, s: &mut Screen, r: Rect) {
        // A slow breathing sine wave while nothing plays.
        let dots_w = r.w as usize * 2;
        let dots_h = r.h as usize * 4;
        let mut grid = vec![0u8; r.w as usize * r.h as usize];
        for dx in 0..dots_w {
            let x = dx as f32 / dots_w as f32;
            let amp = 0.25 + 0.1 * (self.phase * 0.7).sin();
            let v = (x * 12.0 + self.phase * 1.5).sin() * amp * (x * std::f32::consts::PI).sin();
            let dy = (((1.0 - v) / 2.0) * (dots_h - 1) as f32).round() as usize;
            let dy = dy.min(dots_h - 1);
            grid[(dy / 4) * r.w as usize + dx / 2] |= braille_bit(dx % 2, dy % 4);
        }
        for cy in 0..r.h {
            for cx in 0..r.w {
                let bits = grid[cy as usize * r.w as usize + cx as usize];
                if bits != 0 {
                    let ch = char::from_u32(0x2800 + bits as u32).unwrap_or(' ');
                    s.put(r.x + cx, r.y + cy, ch, theme::MUTED.lerp(theme::ACCENT, 0.35));
                }
            }
        }
    }
}

fn braille_bit(x: usize, y: usize) -> u8 {
    const BITS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
    BITS[x][y]
}

/// In-place iterative radix-2 FFT.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let angle = -2.0 * std::f32::consts::PI / len as f32;
        let (w_im, w_re) = angle.sin_cos();
        for start in (0..n).step_by(len) {
            let (mut cur_re, mut cur_im) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let a = start + k;
                let b = a + len / 2;
                let t_re = re[b] * cur_re - im[b] * cur_im;
                let t_im = re[b] * cur_im + im[b] * cur_re;
                re[b] = re[a] - t_re;
                im[b] = im[a] - t_im;
                re[a] += t_re;
                im[a] += t_im;
                let next_re = cur_re * w_re - cur_im * w_im;
                cur_im = cur_re * w_im + cur_im * w_re;
                cur_re = next_re;
            }
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_finds_sine_frequency() {
        let n = 1024;
        let mut re: Vec<f32> =
            (0..n).map(|i| (2.0 * std::f32::consts::PI * 50.0 * i as f32 / n as f32).sin()).collect();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im);
        let mags: Vec<f32> = (0..n / 2).map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt()).collect();
        let max = mags.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert_eq!(max, 50);
    }
}
