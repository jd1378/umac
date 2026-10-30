// Coefficient decoding and dequantization for one 32x32 block per workgroup.
// Ported from PyroWave's shaders/wavelet_dequant.comp:
// Copyright (c) 2025 Hans-Kristian Arntzen
// SPDX-License-Identifier: MIT
// Changes: WGSL without subgroup operations (prefix sums in workgroup memory), every payload
// read bounded to the block's own words, output to a linear f32 buffer.

struct Params {
    block_count: u32,
    stride: u32,
    payload_words: u32,
    _pad: u32,
}

struct BlockInfo {
    coef_offset: u32,
    band_width: u32,
    band_height: u32,
    block_xy: u32,
}

@group(0) @binding(0) var<storage, read_write> coef: array<f32>;
@group(0) @binding(2) var<uniform> params: Params;
@group(0) @binding(3) var<storage, read> payload: array<u32>;
@group(0) @binding(4) var<storage, read> offsets: array<u32>;
@group(0) @binding(5) var<storage, read> blocks: array<BlockInfo>;

const MISSING: u32 = 0xffffffffu;
const MAX_SCALE_EXP: i32 = 4;

var<workgroup> plane_offsets: array<u32, 16>;
var<workgroup> costs: array<u32, 16>;
var<workgroup> sign_base: u32;
var<workgroup> counts: array<u32, 128>;

var<private> byte_lo: u32;
var<private> byte_hi: u32;

fn read_byte(b: u32) -> u32 {
    if (b < byte_lo || b >= byte_hi || (b >> 2u) >= params.payload_words) {
        return 0u;
    }
    return (payload[b >> 2u] >> ((b & 3u) * 8u)) & 0xffu;
}

fn read_u16(b: u32) -> u32 {
    return read_byte(b) | (read_byte(b + 1u) << 8u);
}

// Bytes taken by the first `count` 4x2 subblocks of a code word (2 bits each).
fn code_cost(code: u32, count: u32) -> u32 {
    var sum = 0u;
    for (var s = 0u; s < count; s++) {
        sum += (code >> (2u * s)) & 3u;
    }
    return sum;
}

fn decode_quant(quant_code: u32) -> f32 {
    let e = MAX_SCALE_EXP - i32(quant_code >> 3u);
    let m = i32(quant_code & 7u);
    // exp2 instead of an integer shift: no overflow for out of range codes.
    return (1.0 / (8.0 * 1024.0 * 1024.0)) * f32(8 + m) * exp2(f32(20 + e));
}

fn unswizzle8x8(index: u32) -> vec2<u32> {
    var y = extractBits(index, 0u, 1u);
    var x = extractBits(index, 1u, 2u);
    y |= extractBits(index, 3u, 2u) << 1u;
    x |= extractBits(index, 5u, 1u) << 2u;
    return vec2<u32>(x, y);
}

@compute @workgroup_size(128)
fn main(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    let block = wg.x + wg.y * params.stride;
    if (block >= params.block_count) {
        return;
    }
    let info = blocks[block];
    let off = offsets[block];

    let sub = li & 7u;
    let bx8 = (li >> 3u) & 3u;
    let by8 = (li >> 5u) & 3u;
    let lin = by8 * 4u + bx8;
    let origin = vec2<u32>(info.block_xy & 0xffffu, info.block_xy >> 16u) * 32u + 8u * vec2<u32>(bx8, by8) + unswizzle8x8(sub << 3u);

    var values: array<f32, 8>;
    for (var k = 0u; k < 8u; k++) {
        values[k] = 0.0;
    }

    var ballot = 0u;
    var q_code = 0u;
    if (off != MISSING) {
        byte_lo = off * 4u;
        let words = (payload[min(off, params.payload_words - 1u)] >> 16u) & 0xfffu;
        byte_hi = (off + words) * 4u;
        ballot = read_u16(byte_lo);
        q_code = read_byte(byte_lo + 4u);
    }
    let n = countOneBits(ballot);

    if (li < 16u) {
        var cost = 0u;
        if (extractBits(ballot, li, 1u) != 0u) {
            let idx = countOneBits(ballot & ((1u << li) - 1u));
            let code = read_u16(byte_lo + 8u + 2u * idx);
            let q_bits = read_byte(byte_lo + 8u + 2u * n + idx) & 0xfu;
            cost = code_cost(code, 8u) + 8u * q_bits;
        }
        costs[li] = cost;
    }
    workgroupBarrier();
    if (li == 0u) {
        var running = byte_lo + 8u + 3u * n;
        for (var i = 0u; i < 16u; i++) {
            plane_offsets[i] = running;
            running += costs[i];
        }
        sign_base = running * 8u;
    }
    workgroupBarrier();

    var significant = 0u;
    var scale = 0.0;
    if (extractBits(ballot, lin, 1u) != 0u) {
        let idx = countOneBits(ballot & ((1u << lin) - 1u));
        let code = read_u16(byte_lo + 8u + 2u * idx);
        let qs = read_byte(byte_lo + 8u + 2u * n + idx);
        let q_bits = qs & 0xfu;
        scale = decode_quant(q_code) * (f32((qs >> 4u) & 0xfu) / 8.0 + 0.25);
        if (code != 0u) {
            var at = plane_offsets[lin] + code_cost(code, sub) + q_bits * sub;
            let planes = q_bits + ((code >> (2u * sub)) & 3u);
            var mags: array<u32, 8>;
            for (var k = 0u; k < 8u; k++) {
                mags[k] = 0u;
            }
            for (var q = i32(planes) - 1; q >= 0; q--) {
                let byte = read_byte(at);
                for (var b = 0u; b < 8u; b++) {
                    mags[b] |= ((byte >> b) & 1u) << u32(q);
                }
                at++;
            }
            for (var k = 0u; k < 8u; k++) {
                if (mags[k] != 0u) {
                    values[k] = f32(mags[k]) + 0.5;
                    significant++;
                }
            }
        }
    }

    counts[li] = significant;
    workgroupBarrier();
    if (li == 0u) {
        var running = 0u;
        for (var i = 0u; i < 128u; i++) {
            let c = counts[i];
            counts[i] = running;
            running += c;
        }
    }
    workgroupBarrier();

    var bit = sign_base + counts[li];
    for (var k = 0u; k < 8u; k++) {
        if (values[k] != 0.0) {
            let s = (read_byte(bit >> 3u) >> (bit & 7u)) & 1u;
            values[k] *= 1.0 - 2.0 * f32(s);
            bit++;
        }
    }

    for (var k = 0u; k < 8u; k++) {
        let p = origin + vec2<u32>(k >> 1u, k & 1u);
        if (p.x < info.band_width && p.y < info.band_height) {
            coef[info.coef_offset + p.y * info.band_width + p.x] = values[k] * scale;
        }
    }
}
