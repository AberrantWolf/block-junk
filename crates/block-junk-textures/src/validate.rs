//! Whole-document validation. Run after parse (and after every editor
//! mutation) so evaluation can assume well-formed input. Errors carry a
//! doc path precise enough to fix the Lua by hand.

use std::collections::HashMap;

use crate::doc::{BlendMode, MAX_LAYERS_PER_TEXTURE, MAX_PERIOD, ParamValue, Step, TextureSetDoc};
use crate::error::TexError;
use crate::ops::{OpCategory, ParamType, find_op};

pub fn validate_doc(doc: &TextureSetDoc) -> Result<(), TexError> {
    if !(4..=64).contains(&doc.pixels_per_block) {
        return Err(TexError::invalid(
            "pixels_per_block",
            format!("{} is outside 4..=64", doc.pixels_per_block),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for tex in &doc.textures {
        let at_tex = format!("texture \"{}\"", tex.id);
        if tex
            .id
            .split_once(':')
            .is_none_or(|(ns, name)| ns.is_empty() || name.is_empty())
        {
            return Err(TexError::invalid(&at_tex, "id must be \"namespace:name\""));
        }
        if !seen.insert(tex.id.clone()) {
            return Err(TexError::invalid(&at_tex, "duplicate texture id"));
        }
        if tex.layers.is_empty() {
            return Err(TexError::invalid(&at_tex, "needs at least one layer"));
        }
        if tex.layers.len() > MAX_LAYERS_PER_TEXTURE {
            return Err(TexError::invalid(
                &at_tex,
                format!(
                    "{} layers exceeds the cap of {MAX_LAYERS_PER_TEXTURE} (each layer is a \
                     per-fragment texture fetch)",
                    tex.layers.len()
                ),
            ));
        }
        for (li, layer) in tex.layers.iter().enumerate() {
            let at_layer = format!("{at_tex} / layer {}", li + 1);
            if !(1..=MAX_PERIOD).contains(&layer.period) {
                return Err(TexError::invalid(
                    &at_layer,
                    format!("period {} outside 1..={MAX_PERIOD}", layer.period),
                ));
            }
            if !(0.0..=1.0).contains(&layer.opacity) {
                return Err(TexError::invalid(
                    &at_layer,
                    format!("opacity {} outside 0..=1", layer.opacity),
                ));
            }
            validate_steps(&layer.steps, &at_layer)?;
        }
        if let Some(f) = &tex.finish {
            let at = format!("{at_tex} / finish");
            if f.scale <= 0.0 {
                return Err(TexError::invalid(&at, "scale must be > 0"));
            }
            if !(0.0..=0.5).contains(&f.brightness) {
                return Err(TexError::invalid(&at, "brightness outside 0..=0.5"));
            }
        }
    }
    Ok(())
}

fn validate_steps(steps: &[Step], at_layer: &str) -> Result<(), TexError> {
    // Walk in order, tracking which named outputs exist (with their
    // channel lists) and whether an implicit-prev grey value is live.
    let mut named: HashMap<&str, &'static [&'static str]> = HashMap::new();
    let mut have_prev = false;
    let mut painted = false;
    for (si, step) in steps.iter().enumerate() {
        let at = format!("{at_layer} / step {} ({})", si + 1, step.op);
        let Some(spec) = find_op(&step.op) else {
            return Err(TexError::invalid(
                &at,
                format!("unknown op \"{}\"", step.op),
            ));
        };
        // Every supplied param must exist on the spec with the right type.
        for (key, value) in &step.params {
            let Some(ps) = spec.params.iter().find(|p| p.name == key.as_str()) else {
                return Err(TexError::invalid(
                    &at,
                    format!(
                        "unknown param \"{key}\" (valid: {})",
                        param_names(spec.params)
                    ),
                ));
            };
            check_type_and_range(&at, key, value, &ps.ty)?;
            // Refs must resolve to an already-produced output.
            if let (ParamType::Ref { .. }, ParamValue::Str(r)) = (&ps.ty, value) {
                let (name, channel) = match r.split_once('.') {
                    Some((n, c)) => (n, Some(c)),
                    None => (r.as_str(), None),
                };
                let Some(outputs) = named.get(name) else {
                    return Err(TexError::invalid(
                        &at,
                        format!(
                            "param \"{key}\" references \"{name}\", which no earlier step names \
                             via out = \"{name}\""
                        ),
                    ));
                };
                if let Some(c) = channel
                    && !outputs.contains(&c)
                {
                    return Err(TexError::invalid(
                        &at,
                        format!(
                            "\"{name}\" has no channel \"{c}\" (has: {})",
                            outputs.join(", ")
                        ),
                    ));
                }
            }
        }
        // Required refs must be present; optional input/a fall back to prev.
        for ps in spec.params {
            if let ParamType::Ref { required } = ps.ty {
                let supplied = step.params.contains_key(ps.name);
                let falls_back_to_prev = matches!(ps.name, "input" | "a");
                if required && !supplied {
                    return Err(TexError::invalid(
                        &at,
                        format!("missing required reference \"{}\"", ps.name),
                    ));
                }
                if !supplied && falls_back_to_prev && !have_prev {
                    return Err(TexError::invalid(
                        &at,
                        format!(
                            "\"{}\" omitted but there is no previous grey step to fall back to",
                            ps.name
                        ),
                    ));
                }
            }
            if matches!(ps.ty, ParamType::Stops) && !step.params.contains_key(ps.name) {
                return Err(TexError::invalid(
                    &at,
                    format!("missing required param \"{}\"", ps.name),
                ));
            }
        }
        match spec.category {
            OpCategory::GreyGen | OpCategory::GreyFilter => {
                if let Some(out) = &step.out {
                    if out.is_empty() || out.contains('.') {
                        return Err(TexError::invalid(
                            &at,
                            "out names must be non-empty and must not contain '.'",
                        ));
                    }
                    if named.insert(out.as_str(), spec.outputs).is_some() {
                        return Err(TexError::invalid(
                            &at,
                            format!("duplicate out name \"{out}\" in this layer"),
                        ));
                    }
                }
                have_prev = true;
            }
            OpCategory::Paint => {
                if step.out.is_some() {
                    return Err(TexError::invalid(&at, "paint steps can't have an out name"));
                }
                painted = true;
            }
        }
    }
    if !painted {
        return Err(TexError::invalid(
            at_layer,
            "layer has no paint step (fill/ramp) — it would bake fully transparent",
        ));
    }
    Ok(())
}

fn check_type_and_range(
    at: &str,
    key: &str,
    value: &ParamValue,
    ty: &ParamType,
) -> Result<(), TexError> {
    let bad = |want: &str| {
        Err(TexError::invalid(
            at,
            format!("param \"{key}\" should be {want}"),
        ))
    };
    match ty {
        ParamType::F32 { min, max, .. } => match value {
            ParamValue::F32(v) => {
                if !(*min..=*max).contains(v) {
                    return Err(TexError::invalid(
                        at,
                        format!("param \"{key}\" = {v} outside {min}..={max}"),
                    ));
                }
                Ok(())
            }
            _ => bad("a number"),
        },
        ParamType::U32 { min, max, .. } => match value {
            ParamValue::U32(v) => {
                if !(*min..=*max).contains(v) {
                    return Err(TexError::invalid(
                        at,
                        format!("param \"{key}\" = {v} outside {min}..={max}"),
                    ));
                }
                Ok(())
            }
            _ => bad("an integer"),
        },
        ParamType::Seed => match value {
            ParamValue::U32(_) => Ok(()),
            _ => bad("an integer"),
        },
        ParamType::Enum { options, .. } => match value {
            ParamValue::Str(s) if options.contains(&s.as_str()) => Ok(()),
            ParamValue::Str(s) => Err(TexError::invalid(
                at,
                format!("param \"{key}\" = \"{s}\"; valid: {}", options.join(", ")),
            )),
            _ => bad("a string"),
        },
        ParamType::Ref { .. } => match value {
            ParamValue::Str(_) => Ok(()),
            _ => bad("an output reference string"),
        },
        ParamType::Color { .. } => match value {
            ParamValue::Color(c) if c.iter().all(|v| (0.0..=1.0).contains(v)) => Ok(()),
            ParamValue::Color(_) => bad("color components within 0..=1"),
            _ => bad("a color table { r, g, b[, a] }"),
        },
        ParamType::Vec2 { .. } => match value {
            ParamValue::Vec2(_) => Ok(()),
            _ => bad("a table of two numbers"),
        },
        ParamType::Stops => match value {
            ParamValue::Stops(stops) => {
                if stops.is_empty() {
                    return bad("at least one stop { pos, r, g, b }");
                }
                if stops.windows(2).any(|w| w[0].pos > w[1].pos) {
                    return Err(TexError::invalid(
                        at,
                        format!("param \"{key}\" stops must be sorted by position"),
                    ));
                }
                Ok(())
            }
            _ => bad("a stops table { { pos, r, g, b }, ... }"),
        },
    }
}

fn param_names(params: &[crate::ops::ParamSpec]) -> String {
    params.iter().map(|p| p.name).collect::<Vec<_>>().join(", ")
}

/// Blend mode names accepted in layer defs — shared with the parser.
pub fn parse_blend(s: &str, at: &str) -> Result<BlendMode, TexError> {
    BlendMode::parse(s).ok_or_else(|| {
        TexError::invalid(
            at,
            format!(
                "unknown blend \"{s}\"; valid: {}",
                BlendMode::ALL.map(|m| m.name()).join(", ")
            ),
        )
    })
}
