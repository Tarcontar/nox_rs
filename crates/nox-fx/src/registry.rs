//! The effects Nox renders: what each is called, its category, its
//! parameters (kinds, ranges, defaults) and its ports (pictures and mattes
//! in and out). Resolving an item's effects into a graph is `graph.rs`. Labels are English msgids, marked for
//! extraction with `n_` and translated by the GUI.

use nox_project::{Effect, Value};
use std::hash::{Hash, Hasher};

/// Marks a label for translation (`xgettext --keyword=n_`, see
/// `po/update-pot.sh`); the GUI translates it where it's shown.
pub const fn n_(text: &'static str) -> &'static str {
    text
}

/// Where the "Add effect" list puts an effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Generic,
    Keying,
    Stylize,
    Ai,
    Matte,
}

impl Category {
    pub const ALL: [Category; 5] = [
        Self::Generic,
        Self::Keying,
        Self::Stylize,
        Self::Ai,
        Self::Matte,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Generic => n_("Blur and Sharpen"),
            Self::Keying => n_("Keying"),
            Self::Stylize => n_("Stylize"),
            Self::Ai => n_("AI"),
            Self::Matte => n_("Mattes"),
        }
    }
}

/// What flows through a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortKind {
    /// RGBA in the working space, straight alpha.
    Picture,
    /// One channel, 0–1 (e.g. how much of a pixel is the subject).
    Matte,
}

/// An input or output of an effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortDef {
    pub name: &'static str,
    pub label: &'static str,
    pub kind: PortKind,
    /// An input that may stay unconnected (e.g. a mask).
    pub optional: bool,
}

/// The picture an effect works on; unconnected, the one before it.
pub const PICTURE_IN: PortDef = PortDef {
    name: "picture",
    label: n_("Picture"),
    kind: PortKind::Picture,
    optional: false,
};
/// Where the effect applies (mixed with its input by the matte);
/// unconnected, everywhere.
pub const MASK_IN: PortDef = PortDef {
    name: "mask",
    label: n_("Mask"),
    kind: PortKind::Matte,
    optional: true,
};
pub const MATTE_IN: PortDef = PortDef {
    name: "matte",
    label: n_("Matte"),
    kind: PortKind::Matte,
    optional: false,
};
pub const PICTURE_OUT: PortDef = PortDef {
    name: "picture",
    label: n_("Picture"),
    kind: PortKind::Picture,
    optional: false,
};
pub const MATTE_OUT: PortDef = PortDef {
    name: "matte",
    label: n_("Matte"),
    kind: PortKind::Matte,
    optional: false,
};

/// What a number means, for the controls showing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Plain,
    /// 0–100 %.
    Percent,
    /// Percent of the picture's height: sizes stay the same relative to
    /// the picture at any resolution (preview, export, zoom).
    PictureHeight,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamKind {
    Number {
        min: f64,
        max: f64,
        default: f64,
        /// Decimal places shown.
        digits: u32,
        unit: Unit,
    },
    Bool {
        default: bool,
    },
    /// One of `options` (stored value, label).
    Choice {
        options: &'static [(&'static str, &'static str)],
        default: &'static str,
    },
    /// sRGB as picked (display-referred, 0–1); effects convert it.
    Color {
        default: [f64; 3],
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamDef {
    pub name: &'static str,
    pub label: &'static str,
    pub kind: ParamKind,
}

impl ParamDef {
    /// Whether it can have keyframes (numbers only).
    pub fn animatable(&self) -> bool {
        matches!(self.kind, ParamKind::Number { .. })
    }

    /// The default value.
    pub fn default(&self) -> ParamValue {
        match self.kind {
            ParamKind::Number { default, .. } => ParamValue::Number(default),
            ParamKind::Bool { default } => ParamValue::Bool(default),
            ParamKind::Choice { default, .. } => ParamValue::Choice(default),
            ParamKind::Color { default } => ParamValue::Color(default),
        }
    }

    /// `value` as this parameter's, if it is one (else the default).
    fn read(&self, value: Option<&Value>) -> ParamValue {
        let value = value.and_then(|value| match self.kind {
            ParamKind::Number { min, max, .. } => {
                Some(ParamValue::Number(value.as_f64()?.clamp(min, max)))
            }
            ParamKind::Bool { .. } => Some(ParamValue::Bool(value.as_bool()?)),
            ParamKind::Choice { options, .. } => {
                let stored = value.as_str()?;
                let (name, _) = options.iter().find(|(name, _)| *name == stored)?;
                Some(ParamValue::Choice(name))
            }
            ParamKind::Color { .. } => {
                let list = value.as_array()?;
                let channel = |i: usize| list.get(i)?.as_f64();
                Some(ParamValue::Color([channel(0)?, channel(1)?, channel(2)?]))
            }
        });
        value.unwrap_or_else(|| self.default())
    }
}

/// A parameter's value at some time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamValue {
    Number(f64),
    Bool(bool),
    Choice(&'static str),
    Color([f64; 3]),
}

impl ParamValue {
    /// How it's stored in the project.
    pub fn to_value(self) -> Value {
        match self {
            Self::Number(v) => Value::Float(v),
            Self::Bool(v) => Value::Bool(v),
            Self::Choice(v) => Value::from(v),
            Self::Color(rgb) => Value::Array(rgb.map(Value::Float).to_vec()),
        }
    }

    pub(crate) fn hash_into(&self, hasher: &mut impl Hasher) {
        match self {
            Self::Number(v) => v.to_bits().hash(hasher),
            Self::Bool(v) => v.hash(hasher),
            Self::Choice(v) => v.hash(hasher),
            Self::Color(rgb) => rgb.map(f64::to_bits).hash(hasher),
        }
    }
}

/// An effect Nox renders.
#[derive(Debug)]
pub struct EffectDef {
    /// The project's `effect_name`.
    pub id: &'static str,
    pub label: &'static str,
    pub category: Category,
    pub params: &'static [ParamDef],
    /// What it needs downloaded (background removal's models).
    pub task: Option<nox_ai::models::Task>,
    pub inputs: &'static [PortDef],
    pub outputs: &'static [PortDef],
}

/// The same effect: the same id.
impl PartialEq for EffectDef {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl EffectDef {
    pub fn param(&self, name: &str) -> Option<&'static ParamDef> {
        self.params.iter().find(|p| p.name == name)
    }

    pub fn input(&self, name: &str) -> Option<&'static PortDef> {
        self.inputs.iter().find(|p| p.name == name)
    }

    pub fn output(&self, name: &str) -> Option<&'static PortDef> {
        self.outputs.iter().find(|p| p.name == name)
    }

    /// Whether it outputs a picture.
    pub fn outputs_picture(&self) -> bool {
        self.output("picture").is_some()
    }

    /// Whether it fits a plain stack (the inspector): a picture in and
    /// out, and no input that needs wiring. Others are made on the
    /// Effects page.
    pub fn in_stack(&self) -> bool {
        self.input("picture").is_some()
            && self.outputs_picture()
            && self
                .inputs
                .iter()
                .all(|p| p.name == "picture" || p.optional)
    }
}

pub const GAUSSIAN_BLUR: &str = "nox.gaussian_blur";
pub const SHARPEN: &str = "nox.sharpen";
pub const PAINTERLY: &str = "nox.painterly";
pub const CHROMA_KEY: &str = "nox.chroma_key";
pub const SEGMENT_SUBJECT: &str = "nox.segment_subject";
pub const APPLY_MATTE: &str = "nox.apply_matte";
pub const INVERT_MATTE: &str = "nox.invert_matte";
pub const FEATHER_MATTE: &str = "nox.feather_matte";

/// Which background removal models to use.
const MODEL: ParamDef = ParamDef {
    name: "model",
    label: n_("Model"),
    kind: ParamKind::Choice {
        options: &[
            ("auto", n_("Automatic")),
            ("video", n_("Video (fast)")),
            ("still", n_("Detailed (slow)")),
        ],
        default: "auto",
    },
};

/// A picture effect that can be masked.
const MASKABLE: &[PortDef] = &[PICTURE_IN, MASK_IN];

const fn number(
    name: &'static str,
    label: &'static str,
    min: f64,
    max: f64,
    default: f64,
    digits: u32,
    unit: Unit,
) -> ParamDef {
    ParamDef {
        name,
        label,
        kind: ParamKind::Number {
            min,
            max,
            default,
            digits,
            unit,
        },
    }
}

static EFFECTS: [EffectDef; 8] = [
    EffectDef {
        id: GAUSSIAN_BLUR,
        label: n_("Gaussian Blur"),
        category: Category::Generic,
        params: &[number(
            "radius",
            n_("Radius"),
            0.0,
            10.0,
            1.0,
            2,
            Unit::PictureHeight,
        )],
        task: None,
        inputs: MASKABLE,
        outputs: &[PICTURE_OUT],
    },
    EffectDef {
        id: SHARPEN,
        label: n_("Sharpen"),
        category: Category::Generic,
        params: &[
            number("amount", n_("Amount"), 0.0, 500.0, 100.0, 0, Unit::Percent),
            number(
                "radius",
                n_("Radius"),
                0.0,
                2.0,
                0.15,
                2,
                Unit::PictureHeight,
            ),
            number(
                "threshold",
                n_("Threshold"),
                0.0,
                50.0,
                0.0,
                1,
                Unit::Percent,
            ),
        ],
        task: None,
        inputs: MASKABLE,
        outputs: &[PICTURE_OUT],
    },
    EffectDef {
        id: PAINTERLY,
        label: n_("Painterly"),
        category: Category::Stylize,
        params: &[
            number(
                "radius",
                n_("Radius"),
                0.0,
                3.0,
                0.5,
                2,
                Unit::PictureHeight,
            ),
            number("sharpness", n_("Sharpness"), 1.0, 16.0, 8.0, 1, Unit::Plain),
        ],
        task: None,
        inputs: MASKABLE,
        outputs: &[PICTURE_OUT],
    },
    EffectDef {
        id: CHROMA_KEY,
        label: n_("Chroma Key"),
        category: Category::Keying,
        params: &[
            ParamDef {
                name: "color",
                label: n_("Key Color"),
                kind: ParamKind::Color {
                    default: [0.0, 0.69, 0.25],
                },
            },
            number(
                "tolerance",
                n_("Tolerance"),
                0.0,
                100.0,
                20.0,
                1,
                Unit::Percent,
            ),
            number(
                "softness",
                n_("Softness"),
                0.0,
                100.0,
                10.0,
                1,
                Unit::Percent,
            ),
            number(
                "spill",
                n_("Spill Suppression"),
                0.0,
                100.0,
                50.0,
                0,
                Unit::Percent,
            ),
            ParamDef {
                name: "show_matte",
                label: n_("Show Matte"),
                kind: ParamKind::Bool { default: false },
            },
        ],
        task: None,
        inputs: MASKABLE,
        outputs: &[PICTURE_OUT, MATTE_OUT],
    },
    EffectDef {
        id: SEGMENT_SUBJECT,
        label: n_("Segment Subject"),
        category: Category::Ai,
        params: &[MODEL],
        task: Some(nox_ai::models::Task::BackgroundRemoval),
        inputs: &[PICTURE_IN],
        outputs: &[MATTE_OUT],
    },
    EffectDef {
        id: APPLY_MATTE,
        label: n_("Apply Matte"),
        category: Category::Matte,
        params: &[
            ParamDef {
                name: "mode",
                label: n_("Mode"),
                kind: ParamKind::Choice {
                    options: &[
                        ("multiply", n_("Multiply Alpha")),
                        ("replace", n_("Replace Alpha")),
                    ],
                    default: "multiply",
                },
            },
            ParamDef {
                name: "invert",
                label: n_("Invert"),
                kind: ParamKind::Bool { default: false },
            },
        ],
        task: None,
        inputs: &[PICTURE_IN, MATTE_IN],
        outputs: &[PICTURE_OUT],
    },
    EffectDef {
        id: INVERT_MATTE,
        label: n_("Invert Matte"),
        category: Category::Matte,
        params: &[],
        task: None,
        inputs: &[MATTE_IN],
        outputs: &[MATTE_OUT],
    },
    EffectDef {
        id: FEATHER_MATTE,
        label: n_("Feather Matte"),
        category: Category::Matte,
        params: &[number(
            "radius",
            n_("Radius"),
            0.0,
            10.0,
            0.5,
            2,
            Unit::PictureHeight,
        )],
        task: None,
        inputs: &[MATTE_IN],
        outputs: &[MATTE_OUT],
    },
];

/// Every effect, in the order the "Add effect" list shows them.
pub fn effects() -> &'static [EffectDef] {
    &EFFECTS
}

/// The effect with `effect_name` `id`.
pub fn find(id: &str) -> Option<&'static EffectDef> {
    EFFECTS.iter().find(|e| e.id == id)
}

/// The values of every parameter of `def` in `effect` (one of its kind)
/// at media time `time` (seconds): keyframed, stored or the default.
pub fn values(effect: &Effect, def: &EffectDef, time: f64) -> Vec<ParamValue> {
    def.params
        .iter()
        .map(|param| match param.kind {
            ParamKind::Number {
                min, max, default, ..
            } => ParamValue::Number(effect.number_at(param.name, time, default).clamp(min, max)),
            _ => param.read(effect.param(param.name)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_effects_and_ports() {
        let in_stack: Vec<&str> = effects()
            .iter()
            .filter(|e| e.in_stack())
            .map(|e| e.id)
            .collect();
        assert_eq!(in_stack, [GAUSSIAN_BLUR, SHARPEN, PAINTERLY, CHROMA_KEY]);
        let segment = find(SEGMENT_SUBJECT).unwrap();
        assert!(!segment.outputs_picture());
        assert_eq!(segment.output("matte").unwrap().kind, PortKind::Matte);
        for def in effects() {
            assert!(!def.outputs.is_empty(), "{}", def.id);
        }
    }
}
