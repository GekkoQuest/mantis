//! BC5 (unsigned): 16 bytes per 4x4 block of two channels, each a [BC4](super::bc4)
//! block: bytes 0..8 hold red, bytes 8..16 green. Normal maps store X in red and Y in
//! green; the runtime reconstructs Z.

/// Version of this encoder. BC5 is two BC4 blocks, so it is BC4's version: a change to
/// the BC4 encoder changes every BC5 texture too.
pub const ENCODER_VERSION: u32 = super::bc4::ENCODER_VERSION;

/// Decodes one block to 16 red and 16 green values, row-major.
pub fn decode_block(block: &[u8; 16]) -> ([u8; 16], [u8; 16]) {
    let [r0, r1, r2, r3, r4, r5, r6, r7, g0, g1, g2, g3, g4, g5, g6, g7] = *block;
    (
        super::bc4::decode_block(&[r0, r1, r2, r3, r4, r5, r6, r7]),
        super::bc4::decode_block(&[g0, g1, g2, g3, g4, g5, g6, g7]),
    )
}

/// Encodes 16 red and 16 green values (row-major).
pub fn encode_block(red: &[u8; 16], green: &[u8; 16]) -> [u8; 16] {
    let [r0, r1, r2, r3, r4, r5, r6, r7] = super::bc4::encode_block(red);
    let [g0, g1, g2, g3, g4, g5, g6, g7] = super::bc4::encode_block(green);
    [r0, r1, r2, r3, r4, r5, r6, r7, g0, g1, g2, g3, g4, g5, g6, g7]
}
