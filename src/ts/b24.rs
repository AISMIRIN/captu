// ARIB STD-B24 text codec.
//
// Decodes 8-bit coded character set (CCS) sequences used in EIT event names
// and short descriptions.  Default G-set mapping (ARIB STD-B24 table 7-1):
//   G0 = Kanji (JIS X 0208, 2-byte)
//   G1 = ASCII  (1-byte)
//   G2 = Hiragana (1-byte)
//   G3 = Katakana (1-byte)

/// Decode an ARIB STD-B24 encoded byte string to UTF-8.
pub fn decode_arib_b24(data: &[u8]) -> String {
    let mut result = String::new();
    let mut i = 0;
    let mut g: [u8; 4] = [0, 1, 2, 3]; // G0..G3 initial designations
    let mut gl: usize = 0; // G0 in GL
    let mut gr: usize = 2; // G2(Hiragana) in GR — ARIB B24 default

    while i < data.len() {
        let b = data[i];
        match b {
            0x00 => { i += 1; }
            0x0A | 0x0D => { result.push('\n'); i += 1; }
            0x20 => { result.push(' '); i += 1; }
            0x1B => {
                i += 1;
                if i >= data.len() { break; }
                match data[i] {
                    // Designate multi-byte set (Kanji)
                    0x24 => {
                        i += 1;
                        if i >= data.len() { break; }
                        match data[i] {
                            0x42 => { g[0] = 0; i += 1; } // G0 = Kanji
                            0x29 => {
                                i += 1;
                                if i < data.len() && data[i] == 0x42 { g[1] = 0; i += 1; }
                            }
                            0x2A => {
                                i += 1;
                                if i < data.len() && data[i] == 0x42 { g[2] = 0; i += 1; }
                            }
                            0x2B => {
                                i += 1;
                                if i < data.len() && data[i] == 0x42 { g[3] = 0; i += 1; }
                            }
                            _ => { i += 1; }
                        }
                    }
                    // Designate single-byte sets into G0..G3
                    0x28 => {
                        i += 1;
                        if i < data.len() {
                            g[0] = match data[i] {
                                0x42 => 1, 0x4A => 3, 0x30 => 2, 0x31 => 3, _ => g[0],
                            };
                            i += 1;
                        }
                    }
                    0x29 => {
                        i += 1;
                        if i < data.len() {
                            g[1] = match data[i] { 0x42 => 1, 0x30 => 2, 0x31 => 3, _ => g[1] };
                            i += 1;
                        }
                    }
                    0x2A => {
                        i += 1;
                        if i < data.len() {
                            g[2] = match data[i] { 0x42 => 1, 0x30 => 2, 0x31 => 3, _ => g[2] };
                            i += 1;
                        }
                    }
                    0x2B => {
                        i += 1;
                        if i < data.len() {
                            g[3] = match data[i] { 0x42 => 1, 0x30 => 2, 0x31 => 3, _ => g[3] };
                            i += 1;
                        }
                    }
                    // Locking shifts GL
                    0x6E => { gl = 2; i += 1; } // LS2
                    0x6F => { gl = 3; i += 1; } // LS3
                    // Locking shifts GR (LS1R, LS2R, LS3R)
                    0x7E => { gr = 1; i += 1; } // LS1R: G1 -> GR
                    0x7D => { gr = 2; i += 1; } // LS2R: G2 -> GR
                    0x7C => { gr = 3; i += 1; } // LS3R: G3 -> GR
                    _ => { i += 1; }
                }
            }
            0x0F => { gl = 0; i += 1; } // LS0: G0 -> GL
            0x0E => { gl = 1; i += 1; } // LS1: G1 -> GL
            // GL range
            0x21..=0x7E => {
                decode_gset(&mut result, &mut i, data, b, g[gl], false);
            }
            // GR range
            0xA1..=0xFE => {
                decode_gset(&mut result, &mut i, data, b, g[gr], true);
            }
            _ => { i += 1; }
        }
    }

    result.trim().to_string()
}

fn decode_gset(result: &mut String, i: &mut usize, data: &[u8], b: u8, gset: u8, is_gr: bool) {
    match gset {
        0 => {
            // Kanji (JIS X 0208): 2-byte pair
            // GL bytes are 0x21-0x7E; GR bytes are 0xA1-0xFE (already EUC-JP)
            //
            // ARIB STD-B24 repurposes JIS X 0208 rows 85-94 (GL 0x75-0x7E /
            // GR 0xF5-0xFE) for supplementary symbols and non-JIS kanji.
            // encoding_rs::EUC_JP maps those via JIS X 0213 producing garbage
            // characters (e.g. 橳, 湜). Drop them silently instead.
            let gl_first = if is_gr { b & 0x7F } else { b };
            if gl_first >= 0x75 {
                // ARIB supplementary area — skip both bytes and move on
                let second_ok = *i + 1 < data.len() && if is_gr {
                    (0xA1..=0xFE).contains(&data[*i + 1])
                } else {
                    (0x21..=0x7E).contains(&data[*i + 1])
                };
                *i += if second_ok { 2 } else { 1 };
                return;
            }
            let next_ok = *i + 1 < data.len() && if is_gr {
                (0xA1..=0xFE).contains(&data[*i + 1])
            } else {
                (0x21..=0x7E).contains(&data[*i + 1])
            };
            if next_ok {
                let euc = if is_gr {
                    [b, data[*i + 1]]
                } else {
                    [b | 0x80, data[*i + 1] | 0x80]
                };
                let (decoded, _, _) = encoding_rs::EUC_JP.decode(&euc);
                result.push_str(&decoded);
                *i += 2;
            } else {
                *i += 1;
            }
        }
        1 => {
            result.push((b & 0x7F) as char);
            *i += 1;
        }
        2 => {
            // Hiragana 1-byte set.  Positions 1-86 (GL 0x21-0x76) map cleanly via EUC-JP
            // row 4.  Positions 87-94 (GL 0x77-0x7E) are ARIB extensions outside JIS X0208.
            // The extension table is shared with katakana (same punctuation characters):
            //   89=ー  93=、  94=・  (empirically confirmed from broadcast EIT data)
            let gl_byte = if is_gr { b & 0x7F } else { b };
            if gl_byte >= 0x77 {
                const ARIB_HIRA_EXT: [char; 8] =
                    ['ヷ', 'ヸ', 'ー', 'ヺ', '・', 'ヽ', '、', '・'];
                let idx = (gl_byte - 0x77) as usize;
                if let Some(&ch) = ARIB_HIRA_EXT.get(idx) {
                    result.push(ch);
                }
            } else {
                let b2 = if is_gr { b } else { b | 0x80 };
                let euc = [0xA4u8, b2];
                let (decoded, _, _) = encoding_rs::EUC_JP.decode(&euc);
                result.push_str(&decoded);
            }
            *i += 1;
        }
        3 => {
            // Katakana 1-byte set.  Positions 1-86 (GL 0x21-0x76) map to ァ-ヶ via EUC-JP
            // row 5.  Positions 87-94 (GL 0x77-0x7E) are ARIB extensions not in JIS X0208.
            //
            // Empirically observed from テレビ東京 broadcasts: GL 0x7E (col 94) encodes
            // KATAKANA MIDDLE DOT (・ U+30FB).  Per common ARIB B24 spec references,
            // Empirically confirmed from actual Japanese broadcast EIT data:
            //   col 89 (GL 0x79 / GR 0xF9) = ー (U+30FC, long vowel mark)
            //     → confirmed from テレ朝 EIT
            //   col 93 (GL 0x7D / GR 0xFD) = 、(U+3001, ideographic comma)
            //     → confirmed from EIT
            //   col 94 (GL 0x7E / GR 0xFE) = ・ (U+30FB, middle dot)
            //     → confirmed from テレ東 EIT
            // Others (87, 88, 90, 91, 92) are educated guesses; not yet seen in test data.
            let gl_byte = if is_gr { b & 0x7F } else { b };
            if gl_byte >= 0x77 {
                // ARIB katakana extension: GL positions 0x77-0x7E (87-94).
                //   87=ヷ  88=ヸ  89=ー  90=ヺ  91=・  92=ヽ  93=、  94=・
                const ARIB_KATA_EXT: [char; 8] =
                    ['ヷ', 'ヸ', 'ー', 'ヺ', '・', 'ヽ', '、', '・'];
                let idx = (gl_byte - 0x77) as usize;
                if let Some(&ch) = ARIB_KATA_EXT.get(idx) {
                    result.push(ch);
                }
            } else {
                let b2 = if is_gr { b } else { b | 0x80 };
                let euc = [0xA5u8, b2];
                let (decoded, _, _) = encoding_rs::EUC_JP.decode(&euc);
                result.push_str(&decoded);
            }
            *i += 1;
        }
        _ => { *i += 1; }
    }
}
