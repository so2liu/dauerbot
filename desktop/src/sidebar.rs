use serde::{Deserialize, Deserializer, Serialize};

const DEFAULT: f32 = 224.;
const MIN: f32 = 168.;
const MAX: f32 = 320.;

/// Local desktop preference, encoded as a number independently of daemon configuration.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SidebarWidth(f32);

impl Default for SidebarWidth {
    fn default() -> Self {
        Self(DEFAULT)
    }
}

impl<'de> Deserialize<'de> for SidebarWidth {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(value
            .as_f64()
            .filter(|v| v.is_finite() && (MIN as f64..=MAX as f64).contains(v))
            .map(|v| Self(v as f32))
            .unwrap_or_default())
    }
}

impl SidebarWidth {
    pub fn fitted(self, window: f32) -> f32 {
        // Keep a narrow window usable without overwriting the preferred width.
        self.0.min((window.max(0.) * 0.36).min(MAX))
    }

    pub fn from_drag(wanted: f32, window: f32) -> Self {
        let wanted = if wanted.is_finite() { wanted } else { DEFAULT };
        // Persist a valid preference even if an unusually narrow window caps its rendering.
        Self(wanted.clamp(MIN, MAX).min((window.max(0.) * 0.36).max(MIN)))
    }
}

#[cfg(test)]
mod tests {
    use super::SidebarWidth;

    #[test]
    fn drag_is_bounded_and_window_fitting_preserves_preference() {
        let wide = SidebarWidth::from_drag(900., 1400.);
        assert_eq!(wide.fitted(1400.), 320.);
        assert!((wide.fitted(700.) - 252.).abs() < 0.001);
        assert_eq!(wide.fitted(1400.), 320.);
        assert_eq!(SidebarWidth::from_drag(-100., 1400.).fitted(1400.), 168.);
        assert!(wide.fitted(100.) <= 36.1);
        assert_eq!(
            SidebarWidth::from_drag(f32::NAN, 1400.),
            SidebarWidth::default()
        );
    }
}
