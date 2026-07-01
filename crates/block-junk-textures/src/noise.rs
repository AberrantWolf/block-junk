//! Deterministic, tileable noise primitives.
//!
//! Everything here is a pure function of (uv, params, seed) — no tables,
//! no global state — so bakes are byte-identical across machines and the
//! studio preview always matches the game.
//!
//! Tileability: lattice noises (perlin, worley) wrap their integer lattice
//! at the cell count, so any integer `cells` is seamless. FBM doubles
//! frequency per octave (integer × 2 stays integer ⇒ stays seamless).
//!
//! `value_fbm_world` is the per-fragment "finish" noise. The WGSL twin
//! lives in `render/chunk_material.wgsl` (`finish_fbm`) — same lattice,
//! same hash, so the studio's CPU preview matches the in-game shader.
//! Keep them in sync.

/// 32-bit integer mix (xorshift-multiply finalizer). The base of every
/// hash below.
#[inline]
pub fn hash_u32(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x7feb_352d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846c_a68b);
    h ^= h >> 16;
    h
}

/// Hash an integer lattice point (+ seed) to [0, 1).
#[inline]
pub fn hash2(x: u32, y: u32, seed: u32) -> f32 {
    let h = hash_u32(
        x.wrapping_mul(0x9E37_79B9) ^ y.wrapping_mul(0x85EB_CA6B) ^ seed.wrapping_mul(0xC2B2_AE35),
    );
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// Quintic fade (Perlin's 6t⁵-15t⁴+10t³).
#[inline]
fn fade(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

/// Gradient dot at a wrapped lattice corner. 8 diagonal-ish gradients
/// picked by hash; adequate quality for stylized textures.
#[inline]
fn grad_dot(cx: u32, cy: u32, seed: u32, dx: f32, dy: f32) -> f32 {
    const G: [[f32; 2]; 8] = [
        [1.0, 1.0],
        [-1.0, 1.0],
        [1.0, -1.0],
        [-1.0, -1.0],
        [1.0, 0.0],
        [-1.0, 0.0],
        [0.0, 1.0],
        [0.0, -1.0],
    ];
    let h = hash_u32(
        cx.wrapping_mul(0x9E37_79B9)
            ^ cy.wrapping_mul(0x85EB_CA6B)
            ^ seed.wrapping_mul(0xC2B2_AE35),
    );
    let g = G[(h & 7) as usize];
    g[0] * dx + g[1] * dy
}

/// Periodic Perlin noise on a `cells × cells` lattice over the unit tile.
/// `u`/`v` may be any real; the function has period 1 in both. Output
/// roughly in [-1, 1].
pub fn perlin_periodic(u: f32, v: f32, cells: u32, seed: u32) -> f32 {
    let cells = cells.max(1);
    let x = (u - u.floor()) * cells as f32;
    let y = (v - v.floor()) * cells as f32;
    let xi = x.floor();
    let yi = y.floor();
    let xf = x - xi;
    let yf = y - yi;
    // Wrap lattice coords at `cells` so corner (cells, k) == corner (0, k).
    let x0 = (xi as i64).rem_euclid(cells as i64) as u32;
    let y0 = (yi as i64).rem_euclid(cells as i64) as u32;
    let x1 = (x0 + 1) % cells;
    let y1 = (y0 + 1) % cells;

    let d00 = grad_dot(x0, y0, seed, xf, yf);
    let d10 = grad_dot(x1, y0, seed, xf - 1.0, yf);
    let d01 = grad_dot(x0, y1, seed, xf, yf - 1.0);
    let d11 = grad_dot(x1, y1, seed, xf - 1.0, yf - 1.0);

    let sx = fade(xf);
    let sy = fade(yf);
    let a = d00 + sx * (d10 - d00);
    let b = d01 + sx * (d11 - d01);
    // ~1/sqrt(2) max magnitude for these gradients; scale toward [-1, 1].
    (a + sy * (b - a)) * std::f32::consts::SQRT_2
}

/// Tileable fractal Perlin, normalized to [0, 1]. Frequency doubles per
/// octave (lacunarity fixed at 2 to preserve integer lattice periods).
pub fn fbm_periodic(u: f32, v: f32, cells: u32, octaves: u32, gain: f32, seed: u32) -> f32 {
    let octaves = octaves.clamp(1, 8);
    let mut amp = 1.0_f32;
    let mut freq = 1u32;
    let mut sum = 0.0_f32;
    let mut norm = 0.0_f32;
    for oct in 0..octaves {
        sum += amp
            * perlin_periodic(
                u,
                v,
                cells.saturating_mul(freq).max(1),
                seed ^ (oct * 0x9E37),
            );
        norm += amp;
        amp *= gain;
        freq = freq.saturating_mul(2);
    }
    if norm <= 0.0 {
        return 0.5;
    }
    (sum / norm * 0.5 + 0.5).clamp(0.0, 1.0)
}

/// Tileable Worley/cellular noise. Returns `(f1, f2, cell_rand)`:
/// distances to the nearest and second-nearest feature point in units of
/// one cell pitch, and a flat random value for the nearest cell.
pub fn worley_periodic(u: f32, v: f32, cells: u32, jitter: f32, seed: u32) -> (f32, f32, f32) {
    let cells = cells.max(1) as i64;
    let x = (u - u.floor()) * cells as f32;
    let y = (v - v.floor()) * cells as f32;
    let xi = x.floor() as i64;
    let yi = y.floor() as i64;

    let mut f1 = f32::INFINITY;
    let mut f2 = f32::INFINITY;
    let mut nearest_rand = 0.0_f32;
    for dy in -1..=1i64 {
        for dx in -1..=1i64 {
            let cx = xi + dx;
            let cy = yi + dy;
            // Hash the *wrapped* cell so the tile edges line up.
            let wx = cx.rem_euclid(cells) as u32;
            let wy = cy.rem_euclid(cells) as u32;
            let jx = 0.5 + (hash2(wx, wy, seed) - 0.5) * jitter;
            let jy = 0.5 + (hash2(wx, wy, seed ^ 0x517C_C1B7) - 0.5) * jitter;
            let px = cx as f32 + jx;
            let py = cy as f32 + jy;
            let d = ((px - x) * (px - x) + (py - y) * (py - y)).sqrt();
            if d < f1 {
                f2 = f1;
                f1 = d;
                nearest_rand = hash2(wx, wy, seed ^ 0x68E3_1DA4);
            } else if d < f2 {
                f2 = d;
            }
        }
    }
    (f1, f2, nearest_rand)
}

/// 2D value noise on an unbounded integer lattice (not tileable — used
/// for the world-space finish, which never repeats by design).
fn value_noise_world(x: f32, y: f32, seed: u32) -> f32 {
    let xi = x.floor();
    let yi = y.floor();
    let xf = x - xi;
    let yf = y - yi;
    let x0 = xi as i64 as u32; // two's-complement wrap is fine for hashing
    let y0 = yi as i64 as u32;
    let x1 = x0.wrapping_add(1);
    let y1 = y0.wrapping_add(1);
    let sx = fade(xf);
    let sy = fade(yf);
    let a = hash2(x0, y0, seed) + sx * (hash2(x1, y0, seed) - hash2(x0, y0, seed));
    let b = hash2(x0, y1, seed) + sx * (hash2(x1, y1, seed) - hash2(x0, y1, seed));
    a + sy * (b - a)
}

/// 2-octave world-space value FBM in [0, 1] — the CPU twin of the WGSL
/// `finish_fbm`. `x`/`y` are world units divided by the finish scale.
pub fn value_fbm_world(x: f32, y: f32, seed: u32) -> f32 {
    let n = value_noise_world(x, y, seed) * 0.6667
        + value_noise_world(x * 2.0, y * 2.0, seed ^ 0x9E37) * 0.3333;
    n.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Period-1 wrap: shifting uv by whole tiles changes nothing.
    #[test]
    fn perlin_is_periodic() {
        for &(u, v) in &[(0.13, 0.77), (0.5, 0.01), (0.999, 0.999)] {
            let a = perlin_periodic(u, v, 5, 42);
            let b = perlin_periodic(u + 1.0, v + 3.0, 5, 42);
            assert!(
                (a - b).abs() < 1e-5,
                "perlin not periodic at ({u},{v}): {a} vs {b}"
            );
        }
    }

    #[test]
    fn worley_is_periodic() {
        for &(u, v) in &[(0.13, 0.77), (0.02, 0.5), (0.98, 0.03)] {
            let a = worley_periodic(u, v, 4, 1.0, 7);
            let b = worley_periodic(u + 2.0, v + 1.0, 4, 1.0, 7);
            assert!((a.0 - b.0).abs() < 1e-4 && (a.2 - b.2).abs() < 1e-6);
        }
    }

    /// No seam: values just inside opposite tile edges are close (the
    /// function is continuous across the wrap).
    #[test]
    fn fbm_has_no_seam() {
        let eps = 1e-4_f32;
        for i in 0..16 {
            let v = i as f32 / 16.0;
            let left = fbm_periodic(eps, v, 4, 4, 0.5, 9);
            let right = fbm_periodic(1.0 - eps, v, 4, 4, 0.5, 9);
            assert!(
                (left - right).abs() < 0.02,
                "seam at v={v}: {left} vs {right}"
            );
        }
    }

    #[test]
    fn noise_is_deterministic() {
        assert_eq!(
            fbm_periodic(0.3, 0.6, 4, 4, 0.5, 1),
            fbm_periodic(0.3, 0.6, 4, 4, 0.5, 1)
        );
        assert_ne!(
            fbm_periodic(0.3, 0.6, 4, 4, 0.5, 1),
            fbm_periodic(0.3, 0.6, 4, 4, 0.5, 2)
        );
    }
}
