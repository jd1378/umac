// Final DC shift and conversion of decoded planes to 8-bit texels (R8 luma, RG8 CbCr), written
// to a byte buffer that is then copied into the output textures.
// The DC shift follows the PyroWave bitstream spec (v1); this file is original umac code.

struct Params {
    src0: u32,
    src1: u32,
    // Row stride of the source planes in elements.
    src_stride: u32,
    width: u32,
    height: u32,
    // Destination row pitch in u32 words and start word.
    dst_pitch: u32,
    dst: u32,
    // 0: one plane to R8. 1: two planes to RG8.
    two_planes: u32,
}

@group(0) @binding(0) var<storage, read_write> coef: array<f32>;
@group(0) @binding(2) var<uniform> params: Params;
@group(0) @binding(6) var<storage, read_write> texels: array<u32>;

fn to_unorm8(v: f32) -> u32 {
    return u32(round(clamp(v + 0.5, 0.0, 1.0) * 255.0));
}

fn sample(base: u32, x: u32, y: u32) -> u32 {
    if (x >= params.width) {
        return 0u;
    }
    return to_unorm8(coef[base + y * params.src_stride + x]);
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let word = gid.x;
    let y = gid.y;
    if (y >= params.height || word >= params.dst_pitch) {
        return;
    }
    var packed = 0u;
    if (params.two_planes == 0u) {
        for (var i = 0u; i < 4u; i++) {
            packed |= sample(params.src0, word * 4u + i, y) << (8u * i);
        }
    } else {
        for (var i = 0u; i < 2u; i++) {
            let x = word * 2u + i;
            packed |= sample(params.src0, x, y) << (16u * i);
            packed |= sample(params.src1, x, y) << (16u * i + 8u);
        }
    }
    texels[params.dst + y * params.dst_pitch + word] = packed;
}
