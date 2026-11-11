/// Ambient-light dimming factor shared by the clock and the LED ring: log-shaped (dark room
/// ~0.02, 10 lx ~0.4, >= 500 lx full), low-pass filtered, with hysteresis so that sensor noise
/// around a step boundary does not make the LEDs flicker between two levels.
#[derive(Clone, Debug)]
pub struct AmbientDimmer {
    smooth: f32,
    out: f32,
    primed: bool,
}

/// ~6 s time constant at one reading per 2 s.
const ALPHA: f32 = 0.3;
const HYSTERESIS: f32 = 0.03;

impl Default for AmbientDimmer {
    fn default() -> Self {
        Self::new()
    }
}

impl AmbientDimmer {
    pub fn new() -> Self {
        Self {
            smooth: 1.0,
            out: 1.0,
            primed: false,
        }
    }

    /// Feeds a lux reading (polled every ~2 s); true if `factor()` changed.
    pub fn update(&mut self, lux: f32) -> bool {
        let a = (1.0 + lux.max(0.0)).log10() / 501f32.log10();
        // The C++ station builds (clang -ffp-contract=on, aarch64 / cortex-a53) fused both
        // `a + b * c` below into one FMA; mul_add keeps their rounding.
        let target = a.min(1.0).mul_add(0.98, 0.02);
        self.smooth = if self.primed {
            (target - self.smooth).mul_add(ALPHA, self.smooth)
        } else {
            target
        };
        if self.primed && (self.smooth - self.out).abs() <= HYSTERESIS {
            return false;
        }
        self.primed = true;
        self.out = self.smooth;
        true
    }

    /// 1 until the first reading.
    pub fn factor(&self) -> f32 {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn first_reading_applies_at_once() {
        let mut d = AmbientDimmer::new();
        assert_eq!(d.factor(), 1.0);
        assert!(d.update(0.0));
        assert!(close(d.factor(), 0.02));

        let mut d = AmbientDimmer::new();
        assert!(d.update(1000.0));
        assert!(close(d.factor(), 1.0));
        // Negative readings count as darkness.
        let mut d = AmbientDimmer::new();
        assert!(d.update(-5.0));
        assert!(close(d.factor(), 0.02));
    }

    #[test]
    fn later_readings_are_smoothed() {
        let mut d = AmbientDimmer::new();
        d.update(500.0);
        assert!(d.update(0.0));
        assert!(close(d.factor(), 1.0 + 0.3 * (0.02 - 1.0)));
        assert!(d.update(0.0));
        assert!(close(d.factor(), 0.706 + 0.3 * (0.02 - 0.706)));
    }

    #[test]
    fn hysteresis_holds_small_changes() {
        let mut d = AmbientDimmer::new();
        d.update(0.0);
        // 0.5 lx targets ~0.0839: the first step moves the smoothed value only ~0.019.
        assert!(!d.update(0.5));
        assert!(close(d.factor(), 0.02));
        // The second step accumulates past 0.03 from the shown value.
        assert!(d.update(0.5));
        assert!(close(d.factor(), 0.0526));
        // Small steps back down stay within the band again.
        assert!(!d.update(0.4));
        assert!(close(d.factor(), 0.0526));
    }

    /// Эталон C fmaf: начало при 500 лк (target=1), затем темнота (target=0.02).
    /// Промежуточные lux зависели от точности системного log10f, а проверять нужно FMA.
    /// На шагах 3, 8 и 10 арифметика без слияния отличается на один бит.
    #[test]
    fn rounds_like_the_fused_cxx_build() {
        let mut d = AmbientDimmer::new();
        d.update(500.0);
        for bits in [
            0x3f34_bc6a,
            0x3f00_0d1b,
            0x3eb6_57fb,
            0x3e82_b66b,
            0x3e3d_2440,
            0x3e0a_8b0a,
            0x3dce_3f95,
            0x3d9c_a96f,
            0x3d73_e710,
            0x3d43_4ecd,
            0x3d21_4a9e,
            0x3d09_7ae3,
        ] {
            d.update(0.0);
            assert_eq!(d.smooth.to_bits(), bits);
        }
    }
}
