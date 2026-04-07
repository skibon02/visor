pub const LANE_HEIGHT_PX: f32 = 40.0;
pub const LABEL_WIDTH_PX: f32 = 80.0;

const MIN_SPP: f64 = 0.25; // max zoom in: 4 pixels per sample
const MAX_SPP: f64 = (1u64 << 23) as f64;

#[derive(Debug, Clone)]
pub struct ViewState {
    pub sample_offset: u64,
    pub samples_per_pixel: f64,
    pub total_samples: u64,
}

impl ViewState {
    pub fn new(total_samples: u64) -> Self {
        let spp = (total_samples as f64).max(1.0);
        Self {
            sample_offset: 0,
            samples_per_pixel: spp,
            total_samples,
        }
    }

    pub fn zoom(&mut self, factor: f64, cursor_x: f32, waveform_width_px: f32) {
        let old_spp = self.samples_per_pixel;
        let new_spp = (old_spp / factor).clamp(MIN_SPP, MAX_SPP);
        if (new_spp - old_spp).abs() < f64::EPSILON {
            return;
        }

        let cursor_sample = self.sample_offset as f64 + cursor_x as f64 * old_spp;
        let new_offset = cursor_sample - cursor_x as f64 * new_spp;

        self.samples_per_pixel = new_spp;
        self.sample_offset = (new_offset.max(0.0) as u64)
            .min(self.max_offset(waveform_width_px));
    }

    pub fn pan(&mut self, delta_pixels: f32, waveform_width_px: f32) {
        let delta_samples = delta_pixels as f64 * self.samples_per_pixel;
        if delta_samples >= 0.0 {
            self.sample_offset = self
                .sample_offset
                .saturating_add(delta_samples as u64)
                .min(self.max_offset(waveform_width_px));
        } else {
            self.sample_offset = self
                .sample_offset
                .saturating_sub((-delta_samples) as u64);
        }
    }

    pub fn clamp(&mut self, waveform_width_px: f32) {
        self.samples_per_pixel = self.samples_per_pixel.clamp(MIN_SPP, MAX_SPP);
        self.sample_offset = self.sample_offset.min(self.max_offset(waveform_width_px));
    }

    fn max_offset(&self, waveform_width_px: f32) -> u64 {
        let visible = waveform_width_px as f64 * self.samples_per_pixel;
        self.total_samples.saturating_sub(visible as u64)
    }

    pub fn layout(&self, available_size: [f32; 2], channel_count: usize) -> RenderLayout {
        let width = available_size[0];
        let waveform_width = (width - LABEL_WIDTH_PX).max(0.0);
        let spp = self.samples_per_pixel;
        let viewport_samples = (waveform_width as f64 * spp).ceil() as u64;

        RenderLayout {
            channel_count,
            lane_height_px: LANE_HEIGHT_PX,
            label_width_px: LABEL_WIDTH_PX,
            waveform_width_px: waveform_width,
            samples_per_pixel: spp,
            first_sample: self.sample_offset,
            viewport_samples,
            total_samples: self.total_samples,
        }
    }
}

pub struct RenderLayout {
    pub channel_count: usize,
    pub lane_height_px: f32,
    pub label_width_px: f32,
    pub waveform_width_px: f32,
    pub samples_per_pixel: f64,
    pub first_sample: u64,
    pub viewport_samples: u64,
    pub total_samples: u64,
}
