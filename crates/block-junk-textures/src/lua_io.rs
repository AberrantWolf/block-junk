//! `textures.lua` round-trip: parse (execute the file, walk the returned
//! table) and serialize (deterministic, hand-readable Lua).
//!
//! The file is **pure data** — `return { ... }` with no engine API — so
//! texture-studio can both read it and rewrite it wholesale without
//! touching hand-written mod code in `data.lua`. Serialization is
//! canonical: stable field order (params in OpSpec order), defaults
//! omitted, so a load→save cycle normalizes formatting but never changes
//! meaning.

use std::path::Path;

use mlua::{Lua, Table, Value};

use crate::doc::{
    DEFAULT_PIXELS_PER_BLOCK, FinishDef, LayerDef, ParamValue, RampStop, Step, TextureDef,
    TextureSetDoc,
};
use crate::error::TexError;
use crate::ops::{OPS, ParamType, find_op};
use crate::validate::{parse_blend, validate_doc};

// --------------------------------------------------------------- parse

pub fn parse_file(path: &Path) -> Result<TextureSetDoc, TexError> {
    let src = std::fs::read_to_string(path).map_err(|source| TexError::Io {
        path: path.to_owned(),
        source,
    })?;
    parse_str(&src, &path.display().to_string())
}

/// Parse + validate a `textures.lua` source string. `source_name` is used
/// in error messages (usually the file path).
pub fn parse_str(src: &str, source_name: &str) -> Result<TextureSetDoc, TexError> {
    let lua = Lua::new();
    let value: Value = lua
        .load(src)
        .set_name(source_name)
        .eval()
        .map_err(|source| TexError::Lua {
            source_name: source_name.to_owned(),
            source,
        })?;
    let Value::Table(root) = value else {
        return Err(TexError::invalid(
            source_name,
            "file must `return { ... }` (a table)",
        ));
    };

    let pixels_per_block = match root.get::<Value>("pixels_per_block") {
        Ok(Value::Integer(i)) => i as u32,
        Ok(Value::Nil) => DEFAULT_PIXELS_PER_BLOCK,
        Ok(Value::Number(n)) if n.fract() == 0.0 => n as u32,
        _ => {
            return Err(TexError::invalid("pixels_per_block", "must be an integer"));
        }
    };

    let mut textures = Vec::new();
    match root.get::<Value>("textures") {
        Ok(Value::Table(list)) => {
            for (idx, item) in list.sequence_values::<Value>().enumerate() {
                let at = format!("textures[{}]", idx + 1);
                let Ok(Value::Table(t)) = item else {
                    return Err(TexError::invalid(&at, "expected a table"));
                };
                textures.push(parse_texture(&t, &at)?);
            }
        }
        Ok(Value::Nil) => {}
        _ => return Err(TexError::invalid("textures", "must be an array of tables")),
    }

    let doc = TextureSetDoc {
        pixels_per_block,
        textures,
    };
    validate_doc(&doc)?;
    Ok(doc)
}

fn parse_texture(t: &Table, at: &str) -> Result<TextureDef, TexError> {
    let id: String = match t.get::<Value>("id") {
        Ok(Value::String(s)) => s.to_string_lossy().to_string(),
        _ => return Err(TexError::invalid(at, "missing string field \"id\"")),
    };
    let at = format!("texture \"{id}\"");

    let mut layers = Vec::new();
    match t.get::<Value>("layers") {
        Ok(Value::Table(list)) => {
            for (li, item) in list.sequence_values::<Value>().enumerate() {
                let at_layer = format!("{at} / layer {}", li + 1);
                let Ok(Value::Table(lt)) = item else {
                    return Err(TexError::invalid(&at_layer, "expected a table"));
                };
                layers.push(parse_layer(&lt, &at_layer)?);
            }
        }
        _ => return Err(TexError::invalid(&at, "missing \"layers\" array")),
    }

    let finish = match t.get::<Value>("finish") {
        Ok(Value::Table(ft)) => Some(FinishDef {
            scale: opt_f32(&ft, "scale", &at)?.unwrap_or(FinishDef::default().scale),
            brightness: opt_f32(&ft, "brightness", &at)?.unwrap_or(FinishDef::default().brightness),
            seed: opt_u32(&ft, "seed", &at)?.unwrap_or(0),
        }),
        Ok(Value::Nil) => None,
        _ => return Err(TexError::invalid(&at, "\"finish\" must be a table")),
    };

    Ok(TextureDef { id, layers, finish })
}

fn parse_layer(t: &Table, at: &str) -> Result<LayerDef, TexError> {
    let blend = match t.get::<Value>("blend") {
        Ok(Value::String(s)) => parse_blend(&s.to_string_lossy(), at)?,
        Ok(Value::Nil) => Default::default(),
        _ => return Err(TexError::invalid(at, "\"blend\" must be a string")),
    };
    let mut steps = Vec::new();
    match t.get::<Value>("steps") {
        Ok(Value::Table(list)) => {
            for (si, item) in list.sequence_values::<Value>().enumerate() {
                let at_step = format!("{at} / step {}", si + 1);
                let Ok(Value::Table(st)) = item else {
                    return Err(TexError::invalid(&at_step, "expected a table"));
                };
                steps.push(parse_step(&st, &at_step)?);
            }
        }
        _ => return Err(TexError::invalid(at, "missing \"steps\" array")),
    }
    Ok(LayerDef {
        period: opt_u32(t, "period", at)?.unwrap_or(1),
        blend,
        opacity: opt_f32(t, "opacity", at)?.unwrap_or(1.0),
        steps,
    })
}

fn parse_step(t: &Table, at: &str) -> Result<Step, TexError> {
    let op: String = match t.get::<Value>("op") {
        Ok(Value::String(s)) => s.to_string_lossy().to_string(),
        _ => return Err(TexError::invalid(at, "missing string field \"op\"")),
    };
    let at = format!("{at} ({op})");
    let Some(spec) = find_op(&op) else {
        return Err(TexError::invalid(
            &at,
            format!(
                "unknown op \"{op}\"; valid: {}",
                OPS.iter().map(|o| o.name).collect::<Vec<_>>().join(", ")
            ),
        ));
    };

    let mut step = Step::new(&op);
    for pair in t.pairs::<Value, Value>() {
        let (k, v) = pair.map_err(|source| TexError::Lua {
            source_name: at.clone(),
            source,
        })?;
        let Value::String(key) = k else {
            return Err(TexError::invalid(&at, "step keys must be strings"));
        };
        let key = key.to_string_lossy().to_string();
        match key.as_str() {
            "op" => {}
            "out" => match v {
                Value::String(s) => step.out = Some(s.to_string_lossy().to_string()),
                _ => return Err(TexError::invalid(&at, "\"out\" must be a string")),
            },
            _ => {
                let Some(ps) = spec.params.iter().find(|p| p.name == key) else {
                    return Err(TexError::invalid(
                        &at,
                        format!(
                            "unknown param \"{key}\" (valid: {})",
                            spec.params
                                .iter()
                                .map(|p| p.name)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ));
                };
                let pv = parse_param(&v, &ps.ty, &key, &at)?;
                // Normalize: a value that equals the spec default is the
                // same doc as an absent one — drop it so load→save→load
                // is a structural fixpoint, not just a semantic one.
                if !is_default(&pv, &ps.ty) {
                    step.params.insert(key, pv);
                }
            }
        }
    }
    Ok(step)
}

fn parse_param(v: &Value, ty: &ParamType, key: &str, at: &str) -> Result<ParamValue, TexError> {
    let bad = |want: &str| {
        Err(TexError::invalid(
            at,
            format!("param \"{key}\" must be {want}"),
        ))
    };
    match ty {
        ParamType::F32 { .. } => match as_f32(v) {
            Some(f) => Ok(ParamValue::F32(f)),
            None => bad("a number"),
        },
        ParamType::U32 { .. } | ParamType::Seed => match as_u32(v) {
            Some(i) => Ok(ParamValue::U32(i)),
            None => bad("a non-negative integer"),
        },
        ParamType::Enum { .. } | ParamType::Ref { .. } => match v {
            Value::String(s) => Ok(ParamValue::Str(s.to_string_lossy().to_string())),
            _ => bad("a string"),
        },
        ParamType::Color { .. } => match number_array(v) {
            Some(nums) if nums.len() == 3 => {
                Ok(ParamValue::Color([nums[0], nums[1], nums[2], 1.0]))
            }
            Some(nums) if nums.len() == 4 => {
                Ok(ParamValue::Color([nums[0], nums[1], nums[2], nums[3]]))
            }
            _ => bad("a table { r, g, b } or { r, g, b, a }"),
        },
        ParamType::Vec2 { .. } => match number_array(v) {
            Some(nums) if nums.len() == 2 => Ok(ParamValue::Vec2([nums[0], nums[1]])),
            _ => bad("a table of two numbers"),
        },
        ParamType::Stops => match v {
            Value::Table(list) => {
                let mut stops = Vec::new();
                for (i, item) in list.clone().sequence_values::<Value>().enumerate() {
                    let stop = item.ok().and_then(|s| number_array(&s)).and_then(|n| {
                        (n.len() == 4).then(|| RampStop {
                            pos: n[0],
                            color: [n[1], n[2], n[3]],
                        })
                    });
                    match stop {
                        Some(s) => stops.push(s),
                        None => {
                            return Err(TexError::invalid(
                                at,
                                format!(
                                    "param \"{key}\" stop {} must be {{ pos, r, g, b }}",
                                    i + 1
                                ),
                            ));
                        }
                    }
                }
                Ok(ParamValue::Stops(stops))
            }
            _ => bad("a table of { pos, r, g, b } stops"),
        },
    }
}

fn as_f32(v: &Value) -> Option<f32> {
    match v {
        Value::Number(n) => Some(*n as f32),
        Value::Integer(i) => Some(*i as f32),
        _ => None,
    }
}

fn as_u32(v: &Value) -> Option<u32> {
    match v {
        Value::Integer(i) if *i >= 0 => Some(*i as u32),
        Value::Number(n) if n.fract() == 0.0 && *n >= 0.0 => Some(*n as u32),
        _ => None,
    }
}

fn number_array(v: &Value) -> Option<Vec<f32>> {
    let Value::Table(t) = v else { return None };
    let mut out = Vec::new();
    for item in t.clone().sequence_values::<Value>() {
        out.push(as_f32(&item.ok()?)?);
    }
    Some(out)
}

fn opt_f32(t: &Table, key: &str, at: &str) -> Result<Option<f32>, TexError> {
    match t.get::<Value>(key) {
        Ok(Value::Nil) => Ok(None),
        Ok(v) => as_f32(&v)
            .map(Some)
            .ok_or_else(|| TexError::invalid(at, format!("\"{key}\" must be a number"))),
        Err(_) => Ok(None),
    }
}

fn opt_u32(t: &Table, key: &str, at: &str) -> Result<Option<u32>, TexError> {
    match t.get::<Value>(key) {
        Ok(Value::Nil) => Ok(None),
        Ok(v) => as_u32(&v)
            .map(Some)
            .ok_or_else(|| TexError::invalid(at, format!("\"{key}\" must be an integer"))),
        Err(_) => Ok(None),
    }
}

// ----------------------------------------------------------- serialize

/// Canonical Lua text for a doc. Stable ordering throughout; params print
/// in OpSpec order with spec-default values omitted.
pub fn serialize(doc: &TextureSetDoc) -> String {
    let mut s = String::with_capacity(4096);
    s.push_str(
        "-- Texture definitions. Owned by texture-studio: it parses and rewrites\n\
         -- this whole file on save (formatting is normalized, values preserved).\n\
         -- Safe to hand-edit between studio sessions. Schema reference:\n\
         -- crates/block-junk-textures/src/ops.rs\n",
    );
    s.push_str("return {\n");
    s.push_str(&format!(
        "  pixels_per_block = {},\n\n  textures = {{\n",
        doc.pixels_per_block
    ));
    for tex in &doc.textures {
        s.push_str(&format!("    {{\n      id = {},\n", lua_str(&tex.id)));
        s.push_str("      layers = {\n");
        for layer in &tex.layers {
            s.push_str("        {\n");
            s.push_str(&format!("          period = {},", layer.period));
            if layer.blend != Default::default() {
                s.push_str(&format!(" blend = {},", lua_str(layer.blend.name())));
            }
            if layer.opacity != 1.0 {
                s.push_str(&format!(" opacity = {},", fnum(layer.opacity)));
            }
            s.push('\n');
            s.push_str("          steps = {\n");
            for step in &layer.steps {
                s.push_str(&step_line(step));
            }
            s.push_str("          },\n        },\n");
        }
        s.push_str("      },\n");
        if let Some(f) = &tex.finish {
            s.push_str(&format!(
                "      finish = {{ scale = {}, brightness = {}{} }},\n",
                fnum(f.scale),
                fnum(f.brightness),
                if f.seed != 0 {
                    format!(", seed = {}", f.seed)
                } else {
                    String::new()
                }
            ));
        }
        s.push_str("    },\n");
    }
    s.push_str("  },\n}\n");
    s
}

fn step_line(step: &Step) -> String {
    let mut parts = vec![format!("op = {}", lua_str(&step.op))];
    if let Some(spec) = find_op(&step.op) {
        for ps in spec.params {
            let Some(v) = step.params.get(ps.name) else {
                continue;
            };
            if is_default(v, &ps.ty) {
                continue;
            }
            parts.push(format!("{} = {}", ps.name, value_text(v)));
        }
        // Params not in the spec can't exist post-validation; skip silently.
    } else {
        for (k, v) in &step.params {
            parts.push(format!("{k} = {}", value_text(v)));
        }
    }
    if let Some(out) = &step.out {
        parts.push(format!("out = {}", lua_str(out)));
    }
    let line = format!("            {{ {} }},\n", parts.join(", "));
    // Wrap long stop lists onto their own lines for readability.
    if line.len() <= 100 {
        line
    } else {
        let mut s = String::from("            {\n");
        for part in parts {
            s.push_str(&format!("              {part},\n"));
        }
        s.push_str("            },\n");
        s
    }
}

fn is_default(v: &ParamValue, ty: &ParamType) -> bool {
    match (v, ty) {
        (ParamValue::F32(x), ParamType::F32 { default, .. }) => x == default,
        (ParamValue::U32(x), ParamType::U32 { default, .. }) => x == default,
        (ParamValue::U32(x), ParamType::Seed) => *x == 0,
        (ParamValue::Str(x), ParamType::Enum { default, .. }) => x == default,
        (ParamValue::Color(x), ParamType::Color { default }) => x == default,
        (ParamValue::Vec2(x), ParamType::Vec2 { default }) => x == default,
        _ => false,
    }
}

fn value_text(v: &ParamValue) -> String {
    match v {
        ParamValue::F32(x) => fnum(*x),
        ParamValue::U32(x) => x.to_string(),
        ParamValue::Str(x) => lua_str(x),
        ParamValue::Color(c) => {
            if c[3] == 1.0 {
                format!("{{ {}, {}, {} }}", fnum(c[0]), fnum(c[1]), fnum(c[2]))
            } else {
                format!(
                    "{{ {}, {}, {}, {} }}",
                    fnum(c[0]),
                    fnum(c[1]),
                    fnum(c[2]),
                    fnum(c[3])
                )
            }
        }
        ParamValue::Vec2(o) => format!("{{ {}, {} }}", fnum(o[0]), fnum(o[1])),
        ParamValue::Stops(stops) => {
            let inner: Vec<String> = stops
                .iter()
                .map(|s| {
                    format!(
                        "{{ {}, {}, {}, {} }}",
                        fnum(s.pos),
                        fnum(s.color[0]),
                        fnum(s.color[1]),
                        fnum(s.color[2])
                    )
                })
                .collect();
            format!("{{ {} }}", inner.join(", "))
        }
    }
}

/// Shortest-roundtrip float formatting; Lua parses Rust's `{:?}` floats.
fn fnum(x: f32) -> String {
    format!("{x:?}")
}

fn lua_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
return {
  pixels_per_block = 16,
  textures = {
    {
      id = "test:gravel",
      layers = {
        {
          period = 3,
          steps = {
            { op = "voronoi", cells = 6, jitter = 0.9, seed = 2, out = "stones" },
            { op = "fbm", cells = 8, octaves = 3, out = "grit" },
            { op = "ramp", input = "stones",
              stops = { { 0.0, 0.25, 0.24, 0.26 }, { 1.0, 0.62, 0.6, 0.64 } } },
            { op = "ramp", input = "grit", blend = "overlay", opacity = 0.3,
              stops = { { 0.0, 0.0, 0.0, 0.0 }, { 1.0, 1.0, 1.0, 1.0 } } },
            { op = "fill", color = { 0.1, 0.3, 0.1 }, blend = "multiply",
              mask = "stones.cells", opacity = 0.25 },
          },
        },
      },
      finish = { scale = 14.0, brightness = 0.06 },
    },
  },
}
"#;

    #[test]
    fn parse_then_save_then_parse_is_fixpoint() {
        let doc1 = parse_str(SAMPLE, "sample").unwrap();
        let text = serialize(&doc1);
        let doc2 = parse_str(&text, "roundtrip").unwrap();
        assert_eq!(doc1, doc2);
        // And serialization itself is stable.
        assert_eq!(text, serialize(&doc2));
    }

    #[test]
    fn parsed_doc_bakes() {
        let doc = parse_str(SAMPLE, "sample").unwrap();
        let baked = crate::bake::bake_set(&doc).unwrap();
        assert_eq!(baked[0].layers[0].size, 48);
    }

    #[test]
    fn unknown_op_is_a_good_error() {
        let src = r#"return { textures = { { id = "a:b", layers = { { steps = {
            { op = "perlinz" } } } } } } }"#;
        let err = parse_str(src, "bad").unwrap_err().to_string();
        assert!(err.contains("perlinz") && err.contains("valid:"), "{err}");
    }

    #[test]
    fn unknown_param_is_a_good_error() {
        let src = r#"return { textures = { { id = "a:b", layers = { { steps = {
            { op = "fbm", cellz = 4 } } } } } } }"#;
        let err = parse_str(src, "bad").unwrap_err().to_string();
        assert!(err.contains("cellz"), "{err}");
    }

    #[test]
    fn forward_reference_rejected() {
        let src = r#"return { textures = { { id = "a:b", layers = { { steps = {
            { op = "ramp", input = "later", stops = { { 0.0, 0.0, 0.0, 0.0 } } },
            { op = "fbm", out = "later" } } } } } } }"#;
        let err = parse_str(src, "bad").unwrap_err().to_string();
        assert!(err.contains("later"), "{err}");
    }

    #[test]
    fn empty_doc_parses() {
        let doc = parse_str("return {}", "empty").unwrap();
        assert_eq!(doc.pixels_per_block, 16);
        assert!(doc.textures.is_empty());
    }
}
