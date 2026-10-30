// Inverse CDF 9/7 wavelet transform, one level: each workgroup reconstructs a 32x32 output tile
// from the four sub-bands, with a 4 sample apron, in workgroup memory.
// The lifting constants, steps and tiling follow PyroWave's shaders/idwt.comp and dwt_common.h:
// Copyright (c) 2025 Hans-Kristian Arntzen
// SPDX-License-Identifier: MIT
// Changes: WGSL; whole-sample symmetric extension by index reflection instead of mirrored
// texture gathers (the same extension); both scalings folded into the load; FP32 throughout.

struct Params {
    // LL, HL, LH, HH band offsets in `coef`.
    band0: u32,
    band1: u32,
    band2: u32,
    band3: u32,
    band_width: u32,
    band_height: u32,
    // Output (2 * band size, row stride 2 * band_width) offset in `coef`.
    dst: u32,
    _pad: u32,
}

@group(0) @binding(0) var<storage, read_write> coef: array<f32>;
@group(0) @binding(2) var<uniform> params: Params;

const ALPHA: f32 = -1.586134342059924;
const BETA: f32 = -0.052980118572961;
const GAMMA: f32 = 0.882911075530934;
const DELTA: f32 = 0.443506852043971;
const K: f32 = 1.230174104914001;
const INV_K: f32 = 1.0 / 1.230174104914001;

const TILE: u32 = 32u;
const APRON: u32 = 4u;
const SPAN: u32 = 40u; // TILE + 2 * APRON
const HALF: u32 = 20u;
const THREADS: u32 = 256u;

var<workgroup> s: array<f32, 1600>;

// Whole-sample symmetric extension: x[-i] = x[i], x[n - 1 + i] = x[n - 1 - i].
fn reflect(i: i32, n: i32) -> i32 {
    var j = i;
    if (j < 0) {
        j = -j;
    }
    if (j >= n) {
        j = 2 * n - 2 - j;
    }
    return clamp(j, 0, n - 1);
}

fn lift_rows(parity: u32, c: f32, t: u32) {
    for (var i = t; i < SPAN * HALF; i += THREADS) {
        let row = i / HALF;
        let x = 2u * (i % HALF) + parity;
        if (x >= 1u && x + 1u < SPAN) {
            let at = row * SPAN + x;
            s[at] -= c * (s[at - 1u] + s[at + 1u]);
        }
    }
}

fn lift_columns(parity: u32, c: f32, t: u32) {
    for (var i = t; i < SPAN * HALF; i += THREADS) {
        let col = i % SPAN;
        let y = 2u * (i / SPAN) + parity;
        if (y >= 1u && y + 1u < SPAN) {
            let at = y * SPAN + col;
            s[at] -= c * (s[at - SPAN] + s[at + SPAN]);
        }
    }
}

@compute @workgroup_size(256)
fn main(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_index) t: u32) {
    let bw = params.band_width;
    let bh = params.band_height;
    let n = i32(2u * bw);
    let m = i32(2u * bh);
    let ox = i32(wg.x * TILE) - i32(APRON);
    let oy = i32(wg.y * TILE) - i32(APRON);

    // Interleave the bands (even samples low-pass, odd high-pass) with both 1D scalings.
    for (var i = t; i < SPAN * SPAN; i += THREADS) {
        let gx = reflect(ox + i32(i % SPAN), n);
        let gy = reflect(oy + i32(i / SPAN), m);
        let xh = u32(gx & 1);
        let yh = u32(gy & 1);
        var band = params.band0;
        if (xh == 1u && yh == 0u) {
            band = params.band1;
        } else if (xh == 0u && yh == 1u) {
            band = params.band2;
        } else if (xh == 1u && yh == 1u) {
            band = params.band3;
        }
        let v = coef[band + u32(gy >> 1) * bw + u32(gx >> 1)];
        let sx = select(K, INV_K, xh == 1u);
        let sy = select(K, INV_K, yh == 1u);
        s[i] = v * sx * sy;
    }
    workgroupBarrier();
    lift_rows(0u, DELTA, t);
    workgroupBarrier();
    lift_rows(1u, GAMMA, t);
    workgroupBarrier();
    lift_rows(0u, BETA, t);
    workgroupBarrier();
    lift_rows(1u, ALPHA, t);
    workgroupBarrier();
    lift_columns(0u, DELTA, t);
    workgroupBarrier();
    lift_columns(1u, GAMMA, t);
    workgroupBarrier();
    lift_columns(0u, BETA, t);
    workgroupBarrier();
    lift_columns(1u, ALPHA, t);
    workgroupBarrier();

    for (var i = t; i < TILE * TILE; i += THREADS) {
        let x = i % TILE;
        let y = i / TILE;
        let gx = wg.x * TILE + x;
        let gy = wg.y * TILE + y;
        if (i32(gx) < n && i32(gy) < m) {
            coef[params.dst + gy * u32(n) + gx] = s[(y + APRON) * SPAN + x + APRON];
        }
    }
}
