use serde::{Deserialize, Serialize};

const MIN_BLACK_HOLE_SIZE: u16 = 160;
const MAX_BLACK_HOLE_SIZE: u16 = 480;
const MIN_ROTATION_SECONDS: f32 = 4.0;
const MAX_ROTATION_SECONDS: f32 = 60.0;
const MAX_PARALLAX_STRENGTH: f32 = 0.2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AppVisualEffectManifest {
    pub id: String,
    pub surface: AppVisualSurface,
    pub kind: AppVisualEffectKind,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub options: BlackHoleVisualOptions,
}

impl AppVisualEffectManifest {
    pub fn validate(&self) -> Result<(), String> {
        self.options.validate()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AppVisualSurface {
    ChatEmpty,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AppVisualEffectKind {
    BlackHole,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct BlackHoleVisualOptions {
    pub auto_rotate: bool,
    pub mouse_parallax: bool,
    pub rotation_seconds: f32,
    pub parallax_strength: f32,
    pub size: u16,
}

impl Default for BlackHoleVisualOptions {
    fn default() -> Self {
        Self {
            auto_rotate: true,
            mouse_parallax: true,
            rotation_seconds: 18.0,
            parallax_strength: 0.08,
            size: 300,
        }
    }
}

impl BlackHoleVisualOptions {
    pub fn validate(&self) -> Result<(), String> {
        if !self.rotation_seconds.is_finite()
            || !(MIN_ROTATION_SECONDS..=MAX_ROTATION_SECONDS).contains(&self.rotation_seconds)
        {
            return Err(format!(
                "rotationSeconds must be between {MIN_ROTATION_SECONDS} and {MAX_ROTATION_SECONDS}"
            ));
        }
        if !self.parallax_strength.is_finite()
            || !(0.0..=MAX_PARALLAX_STRENGTH).contains(&self.parallax_strength)
        {
            return Err(format!(
                "parallaxStrength must be between 0 and {MAX_PARALLAX_STRENGTH}"
            ));
        }
        if !(MIN_BLACK_HOLE_SIZE..=MAX_BLACK_HOLE_SIZE).contains(&self.size) {
            return Err(format!(
                "size must be between {MIN_BLACK_HOLE_SIZE} and {MAX_BLACK_HOLE_SIZE}"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black_hole_options_default_to_motion_and_parallax() {
        let options = BlackHoleVisualOptions::default();
        assert!(options.auto_rotate);
        assert!(options.mouse_parallax);
        assert_eq!(options.rotation_seconds, 18.0);
        assert_eq!(options.parallax_strength, 0.08);
        assert_eq!(options.size, 300);
        options.validate().unwrap();
    }

    #[test]
    fn black_hole_options_reject_non_finite_and_out_of_range_values() {
        for options in [
            BlackHoleVisualOptions {
                rotation_seconds: f32::NAN,
                ..Default::default()
            },
            BlackHoleVisualOptions {
                rotation_seconds: 3.9,
                ..Default::default()
            },
            BlackHoleVisualOptions {
                parallax_strength: 0.21,
                ..Default::default()
            },
            BlackHoleVisualOptions {
                size: 159,
                ..Default::default()
            },
        ] {
            assert!(options.validate().is_err());
        }
    }

    #[test]
    fn visual_effect_manifest_uses_camel_case_wire_values() {
        let effect: AppVisualEffectManifest = serde_json::from_value(serde_json::json!({
            "id": "black-hole",
            "surface": "chatEmpty",
            "kind": "blackHole"
        }))
        .unwrap();

        assert_eq!(effect.surface, AppVisualSurface::ChatEmpty);
        assert_eq!(effect.kind, AppVisualEffectKind::BlackHole);
        assert_eq!(effect.options, BlackHoleVisualOptions::default());
    }

    #[test]
    fn visual_effect_manifest_rejects_unknown_surface_kind_and_options() {
        for value in [
            serde_json::json!({
                "id": "bad-surface",
                "surface": "dashboard",
                "kind": "blackHole"
            }),
            serde_json::json!({
                "id": "bad-kind",
                "surface": "chatEmpty",
                "kind": "shader"
            }),
            serde_json::json!({
                "id": "bad-option",
                "surface": "chatEmpty",
                "kind": "blackHole",
                "options": {"script": "effect.js"}
            }),
            serde_json::json!({
                "id": "bad-field",
                "surface": "chatEmpty",
                "kind": "blackHole",
                "script": "effect.js"
            }),
        ] {
            assert!(serde_json::from_value::<AppVisualEffectManifest>(value).is_err());
        }
    }
}
