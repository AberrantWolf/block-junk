-- Texture definitions. Owned by texture-studio: it parses and rewrites
-- this whole file on save (formatting is normalized, values preserved).
-- Safe to hand-edit between studio sessions. Schema reference:
-- crates/block-junk-textures/src/ops.rs
return {
  pixels_per_block = 32,

  textures = {
    {
      id = "vanilla:stone",
      layers = {
        {
          period = 2,
          steps = {
            { op = "fbm", cells = 5, seed = 11, out = "base" },
            {
              op = "ramp",
              input = "base",
              stops = { { 0.0, 0.38, 0.38, 0.41 }, { 1.0, 0.62, 0.62, 0.66 } },
            },
            { op = "white", seed = 3, out = "spk" },
            { op = "threshold", input = "spk", threshold = 0.92, softness = 0.02, out = "spkm" },
            { op = "fill", color = { 0.3, 0.3, 0.34 }, opacity = 0.6, mask = "spkm" },
          },
        },
        {
          period = 3,
          steps = {
            { op = "fbm", octaves = 5, gain = 0.55, seed = 27, out = "m" },
            { op = "threshold", input = "m", threshold = 0.68, softness = 0.07, out = "mossm" },
            {
              op = "ramp",
              input = "m",
              stops = { { 0.5, 0.25, 0.34, 0.18 }, { 1.0, 0.36, 0.5, 0.22 } },
              opacity = 0.85,
              mask = "mossm",
            },
          },
        },
      },
      finish = { scale = 12.0, brightness = 0.05 },
    },
    {
      id = "vanilla:dirt",
      layers = {
        {
          period = 2,
          steps = {
            { op = "fbm", cells = 5, seed = 5, out = "base" },
            {
              op = "ramp",
              input = "base",
              stops = { { 0.0, 0.33, 0.22, 0.13 }, { 1.0, 0.52, 0.38, 0.24 } },
            },
            { op = "white", seed = 8, out = "spk" },
            { op = "threshold", input = "spk", threshold = 0.9, softness = 0.03, out = "spkm" },
            { op = "fill", color = { 0.27, 0.18, 0.1 }, opacity = 0.5, mask = "spkm" },
          },
        },
        {
          period = 5,
          steps = {
            { op = "fbm", cells = 2, octaves = 2, seed = 31, out = "p" },
            { op = "threshold", input = "p", threshold = 0.58, softness = 0.16, out = "pm" },
            {
              op = "fill",
              color = { 0.3, 0.2, 0.11 },
              blend = "multiply",
              opacity = 0.45,
              mask = "pm",
            },
          },
        },
      },
      finish = { scale = 10.0, brightness = 0.05 },
    },
    {
      id = "vanilla:grass_top",
      layers = {
        {
          period = 3,
          steps = {
            { op = "fbm", seed = 2, out = "g" },
            { op = "white", out = "noise" },
            {
              op = "scratches",
              count = 64,
              length = 0.13,
              width = 0.008,
              seed = 4,
              out = "grass",
            },
            { op = "math", a = "g", b = "noise", mode = "average", out = "noise_grass" },
            { op = "math", b = "grass", mode = "add", out = "grass_strands" },
            { op = "ramp", stops = { { 0.0, 0.26, 0.46, 0.18 }, { 1.0, 0.45, 0.7, 0.28 } } },
          },
        },
        {
          period = 4,
          steps = {
            { op = "voronoi", cells = 9, seed = 14, out = "v" },
            { op = "fbm", cells = 8, octaves = 2, seed = 23, out = "wn" },
            { op = "invert", input = "v.f1", out = "blob0" },
            { op = "warp", input = "blob0", by = "wn", amount = 0.04, out = "blob" },
            { op = "threshold", input = "blob", threshold = 0.78, out = "rockm" },
            { op = "fbm", cells = 3, octaves = 1, gain = 0.36, out = "big_perlin" },
            { op = "threshold", threshold = 0.69, out = "bp_thresh" },
            { op = "math", a = "rockm", b = "bp_thresh", out = "filt_rocks" },
            {
              op = "ramp",
              input = "v.cells",
              stops = { { 0.0, 0.38, 0.38, 0.41 }, { 1.0, 0.6, 0.6, 0.63 } },
              mask = "filt_rocks",
            },
          },
        },
        {
          period = 7,
          steps = {
            { op = "fbm", cells = 2, octaves = 2, seed = 9, out = "p" },
            { op = "threshold", input = "p", threshold = 0.55, softness = 0.2, out = "pm" },
            { op = "fill", color = { 0.3, 0.52, 0.2 }, opacity = 0.35, mask = "pm" },
          },
        },
      },
      finish = { scale = 16.0, brightness = 0.06 },
    },
    {
      id = "vanilla:grass_side",
      layers = {
        {
          period = 2,
          steps = {
            { op = "fbm", cells = 5, seed = 5, out = "base" },
            {
              op = "ramp",
              input = "base",
              stops = { { 0.0, 0.33, 0.22, 0.13 }, { 1.0, 0.52, 0.38, 0.24 } },
            },
          },
        },
        {
          period = 1,
          steps = {
            { op = "gradient", out = "g" },
            { op = "fbm", cells = 16, octaves = 2, seed = 4, out = "n" },
            { op = "warp", input = "g", by = "n", amount = 0.06, out = "gw" },
            {
              op = "threshold",
              input = "gw",
              threshold = 0.67,
              softness = 0.085,
              out = "band_pre",
            },
            { op = "threshold", input = "g", threshold = 0.86, out = "force_white" },
            { op = "math", b = "band_pre", mode = "add", out = "band" },
            {
              op = "ramp",
              input = "n",
              stops = { { 0.0, 0.28, 0.48, 0.19 }, { 1.0, 0.42, 0.66, 0.26 } },
              mask = "band",
            },
          },
        },
      },
      finish = { scale = 12.0, brightness = 0.05 },
    },
    {
      id = "vanilla:wood",
      layers = {
        {
          period = 2,
          steps = {
            { op = "bricks", columns = 2, gap = 0.05, bevel = 0.12, seed = 6, out = "b" },
            {
              op = "ramp",
              input = "b.cells",
              stops = { { 0.0, 0.48, 0.34, 0.18 }, { 1.0, 0.62, 0.46, 0.26 } },
            },
            { op = "invert", input = "b", out = "seams" },
            { op = "fill", color = { 0.24, 0.15, 0.08 }, opacity = 0.85, mask = "seams" },
            {
              op = "scratches",
              count = 14,
              length = 0.5,
              width = 0.008,
              seed = 21,
              out = "grain",
            },
            { op = "fill", color = { 0.3, 0.2, 0.1 }, opacity = 0.35, mask = "grain" },
          },
        },
      },
      finish = { scale = 9.0, brightness = 0.04 },
    },
    {
      id = "vanilla:leaves",
      layers = {
        {
          period = 2,
          steps = {
            { op = "voronoi", cells = 9, seed = 17, out = "lv" },
            { op = "quantize", input = "lv.cells", out = "lq" },
            {
              op = "ramp",
              input = "lq",
              stops = { { 0.0, 0.14, 0.36, 0.16 }, { 1.0, 0.3, 0.58, 0.26 } },
            },
            { op = "threshold", input = "lv", threshold = 0.16, softness = 0.1, out = "body" },
            { op = "invert", input = "body", out = "crevice" },
            { op = "fill", color = { 0.08, 0.22, 0.1 }, opacity = 0.7, mask = "crevice" },
          },
        },
      },
      finish = { scale = 8.0, brightness = 0.07 },
    },
    {
      id = "vanilla:door",
      layers = {
        {
          period = 1,
          steps = {
            { op = "bricks", columns = 3, rows = 1, bevel = 0.08, seed = 3, out = "p" },
            {
              op = "ramp",
              input = "p.cells",
              stops = { { 0.0, 0.34, 0.16, 0.05 }, { 1.0, 0.46, 0.24, 0.09 } },
            },
            { op = "invert", input = "p", out = "seams" },
            { op = "fill", color = { 0.16, 0.08, 0.03 }, opacity = 0.9, mask = "seams" },
            { op = "shape", size = 0.1, softness = 0.03, out = "dot" },
            {
              op = "transform",
              input = "dot",
              scale = 1,
              offset = { -0.28, -0.04 },
              out = "handle",
            },
            { op = "fill", color = { 0.15, 0.15, 0.17 }, mask = "handle" },
          },
        },
      },
    },
    {
      id = "vanilla:gravel",
      layers = {
        {
          period = 3,
          steps = {
            { op = "voronoi", cells = 7, seed = 12, out = "st" },
            { op = "quantize", input = "st.cells", steps = 5, out = "shade" },
            {
              op = "ramp",
              input = "shade",
              stops = { { 0.0, 0.36, 0.34, 0.33 }, { 1.0, 0.6, 0.58, 0.57 } },
            },
            { op = "threshold", input = "st", threshold = 0.2, softness = 0.5, out = "body" },
            { op = "invert", input = "body", out = "cracks" },
            { op = "fill", color = { 0.16, 0.15, 0.14 }, opacity = 0.8, mask = "cracks" },
            { op = "white", seed = 5, out = "grit" },
            {
              op = "ramp",
              input = "grit",
              stops = { { 0.0, 0.0, 0.0, 0.0 }, { 1.0, 1.0, 1.0, 1.0 } },
              blend = "overlay",
              opacity = 0.18,
            },
          },
        },
        {
          period = 7,
          steps = {
            { op = "fbm", cells = 2, octaves = 2, seed = 40, out = "t" },
            { op = "threshold", input = "t", threshold = 0.55, softness = 0.18, out = "tm" },
            {
              op = "fill",
              color = { 0.45, 0.38, 0.28 },
              blend = "multiply",
              opacity = 0.3,
              mask = "tm",
            },
          },
        },
      },
      finish = { scale = 11.0, brightness = 0.05 },
    },
  },
}
