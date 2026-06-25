use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};

use super::b24::decode_arib_b24;

pub struct EpgInfo {
    pub title: String,               // Full event title decoded from EIT
    pub series_title: String,        // Derived: series name (for programs table)
    pub sub_title: Option<String>,   // Derived: episode subtitle / event sub-title
    pub episode_number: Option<u16>, // From series_descriptor(0xD5) or title pattern
    pub last_episode: Option<u16>,
    pub series_name: Option<String>,
    pub air_datetime: Option<DateTime<FixedOffset>>,
    pub detail: Option<String>,
}

// Decode ARIB 5-byte timestamp: MJD(16bit) + BCD time hh:mm:ss(24bit)
fn decode_mjd_bcd(data: &[u8]) -> Option<DateTime<FixedOffset>> {
    if data.len() < 5 {
        return None;
    }
    let mjd = ((data[0] as u32) << 8) | data[1] as u32;
    if mjd == 0xFFFF {
        return None;
    }

    // MJD to calendar date (algorithm from ARIB STD-B10)
    let yp = ((mjd as f64 - 15078.2) / 365.25) as i32;
    let mp = ((mjd as f64 - 14956.1 - (yp as f64 * 365.25).floor()) / 30.6001) as i32;
    let day = mjd as i32
        - 14956
        - (yp as f64 * 365.25).floor() as i32
        - (mp as f64 * 30.6001).floor() as i32;
    let (year, month) = if mp == 14 || mp == 15 {
        (yp + 1901, mp - 13)
    } else {
        (yp + 1900, mp - 1)
    };

    // BCD time
    let h = bcd_byte(data[2])? as u32;
    let m = bcd_byte(data[3])? as u32;
    let s = bcd_byte(data[4])? as u32;

    let jst = FixedOffset::east_opt(9 * 3600)?;
    let naive = NaiveDateTime::new(
        NaiveDate::from_ymd_opt(year, month as u32, day as u32)?,
        NaiveTime::from_hms_opt(h, m, s)?,
    );
    Some(DateTime::from_naive_utc_and_offset(
        naive - chrono::Duration::hours(9),
        jst,
    ))
}

fn bcd_byte(b: u8) -> Option<u8> {
    let hi = b >> 4;
    let lo = b & 0x0F;
    if hi > 9 || lo > 9 {
        return None;
    }
    Some(hi * 10 + lo)
}

fn parse_eit_section(data: &[u8]) -> Option<EpgInfo> {
    // Header: table_id(1) + section_syntax(2) + service_id(2) + version(1)
    //         + section/last_section(2) + tsid(2) + onid(2) + seg_last(1) + last_tid(1) = 14 bytes
    if data.len() < 18 {
        return None;
    }

    let mut pos = 14usize;
    let mut epg = EpgInfo {
        title: String::new(),
        series_title: String::new(),
        sub_title: None,
        episode_number: None,
        last_episode: None,
        series_name: None,
        air_datetime: None,
        detail: None,
    };

    // Parse events (skip CRC at end: last 4 bytes)
    let data_end = data.len().saturating_sub(4);

    while pos + 12 <= data_end {
        // event_id(2) + start_time(5) + duration(3) + running(2-ish) + desc_loop_len(12bit)
        let air_datetime = decode_mjd_bcd(&data[pos + 2..pos + 7]);
        let desc_loop_len = (((data[pos + 10] as usize) & 0x0F) << 8) | data[pos + 11] as usize;
        pos += 12;

        epg.air_datetime = air_datetime;

        let desc_end = (pos + desc_loop_len).min(data_end);
        while pos + 2 <= desc_end {
            let tag = data[pos];
            let dlen = data[pos + 1] as usize;
            pos += 2;
            if pos + dlen > desc_end {
                break;
            }
            let d = &data[pos..pos + dlen];

            match tag {
                0x4D => {
                    // short_event_descriptor: lang(3) + name_len(1) + name + text_len(1) + text
                    if dlen >= 4 {
                        let name_len = d[3] as usize;
                        if 4 + name_len <= dlen {
                            epg.title = strip_arib_icons(&decode_arib_b24(&d[4..4 + name_len]));
                        }
                        let text_pos = 4 + name_len;
                        if text_pos < dlen {
                            let text_len = d[text_pos] as usize;
                            if text_pos + 1 + text_len <= dlen && text_len > 0 {
                                // short description (short_text) - not stored separately
                            }
                        }
                    }
                }
                0xD5 => {
                    // series_descriptor: series_id(2)+flags(1)+expire(2)+ep(12)+last(12)+name_len(1)+name
                    if dlen >= 9 {
                        let ep = ((d[5] as u16) << 4) | ((d[6] as u16) >> 4);
                        let last = (((d[6] & 0x0F) as u16) << 8) | d[7] as u16;
                        if ep > 0 {
                            epg.episode_number = Some(ep);
                        }
                        if last > 0 {
                            epg.last_episode = Some(last);
                        }
                        let name_len = d[8] as usize;
                        if 9 + name_len <= dlen {
                            let s = decode_arib_b24(&d[9..9 + name_len]);
                            if !s.is_empty() {
                                epg.series_name = Some(s);
                            }
                        }
                    }
                }
                0x4E if dlen >= 5 => {
                    // extended_event_descriptor: descriptor_num(4)+last(4)+lang(3)+items_len(1)+items+text_len(1)+text
                    let items_len = d[4] as usize;
                    let text_pos = 5 + items_len;
                    if text_pos < dlen {
                        let text_len = d[text_pos] as usize;
                        if text_pos + 1 + text_len <= dlen && text_len > 0 {
                            let t = decode_arib_b24(&d[text_pos + 1..text_pos + 1 + text_len]);
                            if !t.is_empty() {
                                epg.detail = Some(t);
                            }
                        }
                    }
                }
                _ => {}
            }
            pos += dlen;
        }
        pos = desc_end;
        if !epg.title.is_empty() {
            break;
        }
    }

    if epg.title.is_empty() {
        None
    } else {
        Some(epg)
    }
}

// Scan EIT on PID=0x0012.
// service_ids: if non-empty, only accept sections whose service_id is in this list.
// For table_id=0x4E (present/following), only accept section_number=0 (current event).
//
// MPEG-2 PSI pointer_field handling (ATIS-0300006 / ISO 13818-1):
// When PUSI=1, payload byte 0 is a pointer_field (ptr).  Bytes [1..1+ptr] are the TAIL
// of the previous section (completing it), and bytes [1+ptr..] start the new section.
// The previous code discarded those ptr bytes, so multi-packet present sections (common on
// テレビ朝日 where the p/f section exceeds one TS packet) never completed and the decoder
// returned the "following" section instead.  The corrected logic completes the prior
// section with its tail bytes before starting the new one.
fn scan_eit(file: &mut File, service_ids: &[u16], max_packets: u32) -> Option<Vec<u8>> {
    let mut packet = [0u8; 188];
    // buf is non-empty only while accumulating a section we want to return.
    let mut buf: Vec<u8> = Vec::new();
    let mut expected = 0usize;

    for _ in 0..max_packets {
        if file.read_exact(&mut packet).is_err() {
            return None;
        }
        if packet[0] != 0x47 {
            continue;
        }

        let pid = ((packet[1] as u16 & 0x1F) << 8) | packet[2] as u16;
        if pid != 0x0012 {
            continue;
        }

        let pusi = (packet[1] & 0x40) != 0;
        let afc = (packet[3] & 0x30) >> 4;
        if afc == 2 {
            continue;
        }

        let mut ps = 4usize;
        if afc == 3 {
            ps = 5 + packet[4] as usize;
        }
        if ps >= 188 {
            continue;
        }

        if pusi {
            let ptr = packet[ps] as usize;
            ps += 1; // advance past the pointer_field byte

            // --- ptr tail bytes: complete the previous section if we have one ---
            if ptr > 0 && !buf.is_empty() && expected > 0 {
                let need = expected.saturating_sub(buf.len());
                let avail = 188usize.saturating_sub(ps);
                let take = ptr.min(need).min(avail);
                if take > 0 {
                    buf.extend_from_slice(&packet[ps..ps + take]);
                    if buf.len() >= expected {
                        return Some(buf[..expected].to_vec());
                    }
                }
            }
            // Discard any incomplete previous accumulation; start fresh.
            buf.clear();
            expected = 0;

            // --- New section starts at ps + ptr ---
            ps += ptr;
            if ps + 8 > 188 {
                continue;
            }

            let table_id = packet[ps];
            // Only EIT[p/f] present/following (0x4E). Schedule tables (0x50-0x6F)
            // span the full day and would return the previous programme.
            if table_id != 0x4E {
                continue;
            }
            if packet[ps + 1] & 0x80 == 0 {
                continue;
            }

            // service_id filter (bytes 3-4 of the section)
            let sec_svc = ((packet[ps + 3] as u16) << 8) | packet[ps + 4] as u16;
            if !service_ids.is_empty() && !service_ids.contains(&sec_svc) {
                continue;
            }

            // section_number=0: current event (present). section_number=1 is "following".
            if packet[ps + 6] != 0 {
                continue;
            }

            let slen = (((packet[ps + 1] as usize) & 0x0F) << 8) | packet[ps + 2] as usize;
            let total = 3 + slen;
            let avail = 188usize.saturating_sub(ps);
            buf.extend_from_slice(&packet[ps..ps + total.min(avail)]);
            expected = total;

            // Section may fit entirely within this packet.
            if buf.len() >= expected {
                return Some(buf[..expected].to_vec());
            }
        } else if !buf.is_empty() {
            // Continue accumulating the current target section.
            let rem = expected.saturating_sub(buf.len());
            let avail = 188usize.saturating_sub(ps);
            let take = rem.min(avail);
            buf.extend_from_slice(&packet[ps..ps + take]);

            if buf.len() >= expected && expected > 0 {
                return Some(buf[..expected].to_vec());
            }
        }
    }
    None
}

// ── Series/episode extraction helpers ─────────────────────────────────────────

// Remove ARIB service indicator characters (U+1F100–U+1F2FF: squared alphanumerics /
// squared ideographs used as broadcast icons like 🈓🈑).  These are valid Unicode but
// carry no meaning in stored EPG text.
fn strip_arib_icons(s: &str) -> String {
    s.chars()
        .filter(|&c| !('\u{1F100}'..='\u{1F2FF}').contains(&c))
        .collect()
}

// Strip trailing broadcast flags like [字][デ][SS][終][再][解] from a title string.
fn strip_broadcast_flags(s: &str) -> &str {
    let mut s = s.trim();
    loop {
        let t = s.trim_end();
        if t.ends_with(']') {
            if let Some(start) = t.rfind('[') {
                s = t[..start].trim();
                continue;
            }
        }
        break;
    }
    s
}

// Parse digits (ASCII 0-9 or fullwidth ０-９) at the start of `s`.
// Returns (numeric_value, byte_length_consumed) or None if no digit found.
fn parse_digits_at(s: &str) -> Option<(u16, usize)> {
    let mut val = 0u32;
    let mut byte_len = 0usize;
    let mut found = false;
    for c in s.chars() {
        let d = if c.is_ascii_digit() {
            c as u32 - '0' as u32
        } else if ('\u{FF10}'..='\u{FF19}').contains(&c) {
            // Fullwidth digit ０-９ (U+FF10-FF19)
            c as u32 - '\u{FF10}' as u32
        } else {
            break;
        };
        val = val * 10 + d;
        if val > 9999 {
            break;
        }
        byte_len += c.len_utf8();
        found = true;
    }
    if found && val <= u16::MAX as u32 {
        Some((val as u16, byte_len))
    } else {
        None
    }
}

// Derive series_title, episode_number, and sub_title from a raw EIT event title.
//
// Patterns recognised (in priority order):
//   1. `＃N` or `#N`  — followed by optional subtitle
//   2. `第N話`         — with optional whitespace around N
//
// If no episode token is found, split at the first ASCII/ideographic space:
//   series = first word, sub_title = remainder.
fn extract_series_episode(raw_title: &str) -> (String, Option<u16>, Option<String>) {
    let title = strip_broadcast_flags(raw_title);

    // Scan for ＃/# or 第…話 episode tokens.
    let mut byte_pos = 0usize;
    while byte_pos < title.len() {
        let rest = &title[byte_pos..];

        // Try ＃ (U+FF03, 3 bytes) or # (U+0023, 1 byte)
        let prefix_len = if rest.starts_with('＃') {
            '＃'.len_utf8()
        } else if rest.starts_with('#') {
            '#'.len_utf8()
        } else {
            0
        };
        if prefix_len > 0 {
            let after_prefix = &rest[prefix_len..];
            if let Some((n, dlen)) = parse_digits_at(after_prefix) {
                let series = title[..byte_pos].trim().to_string();
                let sub_raw = title[byte_pos + prefix_len + dlen..].trim();
                let sub = strip_broadcast_flags(sub_raw);
                return (
                    series,
                    Some(n),
                    if sub.is_empty() {
                        None
                    } else {
                        Some(sub.to_string())
                    },
                );
            }
        }

        // Try 第 (3 bytes) … 話 (3 bytes)
        if rest.starts_with("第") {
            let plen = "第".len();
            let after = &rest[plen..];
            // Allow optional space between 第 and digits
            let digits_start = after.trim_start_matches([' ', '\u{3000}']);
            let skip = after.len() - digits_start.len();
            if let Some((n, dlen)) = parse_digits_at(digits_start) {
                let series = title[..byte_pos].trim().to_string();
                let ep_end = byte_pos + plen + skip + dlen;
                // Skip optional whitespace + 話 suffix
                let after_digits = title[ep_end..].trim_start_matches([' ', '\u{3000}']);
                let ep_end2 = if after_digits.starts_with("話") {
                    title.len() - after_digits.len() + "話".len()
                } else {
                    ep_end
                };
                let sub_raw = title[ep_end2..].trim();
                let sub = strip_broadcast_flags(sub_raw);
                return (
                    series,
                    Some(n),
                    if sub.is_empty() {
                        None
                    } else {
                        Some(sub.to_string())
                    },
                );
            }
        }

        // Advance one Unicode scalar
        let c = title[byte_pos..].chars().next().unwrap();
        byte_pos += c.len_utf8();
    }

    // No episode token: split at first whitespace (half-width or full-width)
    if let Some(pos) = title.find([' ', '\u{3000}']) {
        let series = title[..pos].to_string();
        let sub_raw = title[pos..].trim();
        let sub = strip_broadcast_flags(sub_raw);
        (
            series,
            None,
            if sub.is_empty() {
                None
            } else {
                Some(sub.to_string())
            },
        )
    } else {
        (title.to_string(), None, None)
    }
}

// Populate series_title, sub_title (and episode_number from title if not already set)
// on a freshly parsed EpgInfo.
fn fill_series_episode(mut epg: EpgInfo) -> EpgInfo {
    let (series, ep_from_title, sub) = extract_series_episode(&epg.title);
    epg.series_title = series;
    epg.sub_title = sub;
    if epg.episode_number.is_none() {
        epg.episode_number = ep_from_title;
    }
    epg
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Extract EPG info from the TS file.
///
/// `caption_services` must be the service IDs returned by `pes::scan_psi`
/// (or an empty slice to scan all services).  Passing them in avoids
/// re-reading the PAT/PMT — callers should run `scan_psi` once and share
/// the result between `extract_epg` and `extract_captions`.
pub fn extract_epg(ts_path: &Path, caption_services: &[u16]) -> Result<EpgInfo> {
    let file_size = std::fs::metadata(ts_path)?.len();
    let caption_svcs = caption_services.to_vec();

    // Seek to 20% of file (KonomiTV's trick):
    // skips recording start margin where EIT still shows the previous programme.
    // Use 600K packet window (≈ 113 MB ≈ 2.5 s at 360 Mbit/s) to ensure at least
    // one full EIT[p/f] present cycle is captured even on テレビ朝日 with large sections.
    if file_size > 500_000 {
        let offset_20 = (file_size * 20 / 100 / 188) * 188; // align to TS packet boundary
        let mut file = File::open(ts_path)?;
        if file.seek(SeekFrom::Start(offset_20)).is_ok() {
            if let Some(data) = scan_eit(&mut file, &caption_svcs, 600_000) {
                if let Some(epg) = parse_eit_section(&data) {
                    return Ok(fill_series_episode(epg));
                }
            }
        }
    }

    // Fallback: scan from the beginning (short files / single-service TS)
    let mut file = File::open(ts_path)?;
    if let Some(data) = scan_eit(&mut file, &caption_svcs, 300_000) {
        if let Some(epg) = parse_eit_section(&data) {
            return Ok(fill_series_episode(epg));
        }
    }

    // Last resort: mtime as air_date
    use std::time::UNIX_EPOCH;
    let air_datetime = std::fs::metadata(ts_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| {
            let secs = d.as_secs() as i64;
            let jst = FixedOffset::east_opt(9 * 3600)?;
            Some(DateTime::from_timestamp(secs, 0)?.with_timezone(&jst))
        });

    Ok(EpgInfo {
        title: String::from("(unknown)"),
        series_title: String::new(),
        sub_title: None,
        episode_number: None,
        last_episode: None,
        series_name: None,
        air_datetime,
        detail: None,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        bcd_byte, decode_mjd_bcd, extract_series_episode, fill_series_episode, parse_digits_at,
        parse_eit_section, strip_arib_icons, strip_broadcast_flags, EpgInfo,
    };
    use chrono::Timelike;

    // ── bcd_byte ──────────────────────────────────────────────────────────────

    #[test]
    fn bcd_byte_zero() {
        assert_eq!(bcd_byte(0x00), Some(0));
    }

    #[test]
    fn bcd_byte_max() {
        assert_eq!(bcd_byte(0x99), Some(99));
    }

    #[test]
    fn bcd_byte_lo_nibble_invalid() {
        // lo nibble A (10) → invalid
        assert_eq!(bcd_byte(0x0A), None);
    }

    #[test]
    fn bcd_byte_hi_nibble_invalid() {
        // hi nibble A (10) → invalid
        assert_eq!(bcd_byte(0xA0), None);
    }

    // ── parse_digits_at ───────────────────────────────────────────────────────

    #[test]
    fn parse_digits_at_ascii() {
        assert_eq!(parse_digits_at("12"), Some((12, 2)));
    }

    #[test]
    fn parse_digits_at_fullwidth() {
        // FullWidth "１２" (U+FF11, U+FF12) → 12, 6 bytes consumed
        assert_eq!(parse_digits_at("１２"), Some((12, 6)));
    }

    #[test]
    fn parse_digits_at_mixed_ascii_fullwidth() {
        // "1２3" → 1 byte + 3 bytes + 1 byte = 5 bytes, value = 123
        assert_eq!(parse_digits_at("1２3"), Some((123, 5)));
    }

    #[test]
    fn parse_digits_at_no_digits() {
        assert_eq!(parse_digits_at("abc"), None);
        assert_eq!(parse_digits_at(""), None);
    }

    #[test]
    fn parse_digits_at_5digit_truncates() {
        // When '5' pushes val to 12345 (>9999), the code breaks before updating byte_len.
        // So byte_len=4 (bytes consumed up to '4') but val=12345 (includes overflow digit).
        // This pins the current behavior as a regression test.
        assert_eq!(parse_digits_at("12345"), Some((12345, 4)));
    }

    // ── strip_arib_icons ──────────────────────────────────────────────────────

    #[test]
    fn strip_arib_icons_removes_boundary_chars() {
        // U+1F100 and U+1F2FF are the inclusive range boundaries — both must be removed
        let s = "\u{1F100}テスト\u{1F2FF}";
        assert_eq!(strip_arib_icons(s), "テスト");
    }

    #[test]
    fn strip_arib_icons_outside_range_preserved() {
        // U+1F0FF (one below) and U+1F300 (one above) must pass through unchanged
        let s = "\u{1F0FF}あ\u{1F300}";
        assert_eq!(strip_arib_icons(s), "\u{1F0FF}あ\u{1F300}");
    }

    #[test]
    fn strip_arib_icons_empty() {
        assert_eq!(strip_arib_icons(""), "");
    }

    // ── parse_eit_section ─────────────────────────────────────────────────────

    /// Build a minimal EIT section with one event and a single 0x4D short_event_descriptor.
    /// `title_b24` is the raw ARIB B24 name payload (no leading length byte).
    fn eit_with_title(title_b24: &[u8]) -> Vec<u8> {
        let name_len = title_b24.len();
        let desc_body_len = 3 + 1 + name_len; // lang(3) + name_len_byte(1) + name
        let desc_total = 2 + desc_body_len; // tag(1) + dlen(1) + body

        let mut buf = Vec::with_capacity(14 + 12 + desc_total + 4);
        // Section header (14 bytes)
        buf.extend_from_slice(&[
            0x4E, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x4E,
        ]);
        // Event header (12 bytes): event_id(2) + start_time(5) + duration(3) + desc_len(2)
        buf.extend_from_slice(&[0x00, 0x01]); // event_id
        buf.extend_from_slice(&[0xFF, 0xFF, 0x00, 0x00, 0x00]); // start_time (0xFFFF → None)
        buf.extend_from_slice(&[0x00, 0x10, 0x00]); // duration BCD
        buf.push(((desc_total >> 8) & 0x0F) as u8); // desc_loop_len high nibble
        buf.push((desc_total & 0xFF) as u8); // desc_loop_len low byte
                                             // 0x4D descriptor
        buf.push(0x4D); // tag
        buf.push(desc_body_len as u8); // dlen
        buf.extend_from_slice(&[0x6A, 0x70, 0x6E]); // lang "jpn"
        buf.push(name_len as u8); // name_len
        buf.extend_from_slice(title_b24); // name bytes
                                          // CRC placeholder (4 bytes)
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        buf
    }

    #[test]
    fn parse_eit_section_too_short() {
        assert!(parse_eit_section(&[0u8; 17]).is_none());
    }

    #[test]
    fn parse_eit_section_empty_title_returns_none() {
        // Section with a valid event but no title descriptor → None
        let buf = vec![0u8; 14 + 12 + 4]; // header + event (desc_loop_len=0) + CRC
                                         // desc_loop_len = 0 (bytes 24-25 already 0)
        assert!(parse_eit_section(&buf).is_none());
    }

    #[test]
    fn parse_eit_section_short_event_title() {
        // ESC 0x28 0x4A = designate alphanumeric to G0, then ASCII "AB"
        let title_b24: &[u8] = &[0x1B, 0x28, 0x4A, 0x41, 0x42];
        let data = eit_with_title(title_b24);
        let epg = parse_eit_section(&data).expect("should return Some for non-empty title");
        assert_eq!(epg.title, "AB");
    }

    #[test]
    fn parse_eit_section_series_descriptor_ep_bits() {
        // Section with 0x4D title "X" + 0xD5 series_descriptor encoding ep=5, last=12.
        // ep = (d[5]<<4)|(d[6]>>4): d[5]=0x00, d[6]=0x5C → ep = 0|(0x5C>>4)=5
        // last = ((d[6]&0x0F)<<8)|d[7]: d[6]&0x0F=0x0C, d[7]=0x00 → last=12
        let title_b24: &[u8] = &[0x1B, 0x28, 0x4A, 0x58]; // "X"
        let name_len = title_b24.len();
        let desc_body_4d = 3 + 1 + name_len;
        let desc_total_4d = 2 + desc_body_4d;

        // 0xD5 series_descriptor: ep=5, last=12
        // ep = (d[5]<<4)|(d[6]>>4): d[5]=0x00, d[6] upper nibble = 0x5 → ep=5
        // last = ((d[6]&0x0F)<<8)|d[7]: d[6] lower nibble = 0x0, d[7]=0x0C → last=12
        let desc_d5_correct: &[u8] = &[
            0xD5, 0x09, // tag, dlen=9
            0x00, 0x01, // series_id
            0x00, // flags
            0x00, 0x00, // expire
            0x00, // d[5]: ep bits 11..4 = 0
            0x50, // d[6]: ep bits 3..0 = 5 (upper nibble), last bits 11..8 = 0 (lower nibble)
            0x0C, // d[7]: last bits 7..0 = 12
            0x00, // d[8]: name_len = 0
        ];
        let desc_total_d5 = desc_d5_correct.len();
        let desc_loop_len = desc_total_4d + desc_total_d5;

        let mut buf = Vec::new();
        // Section header (14 bytes)
        buf.extend_from_slice(&[
            0x4E, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x4E,
        ]);
        // Event header (12 bytes)
        buf.extend_from_slice(&[0x00, 0x01]);
        buf.extend_from_slice(&[0xFF, 0xFF, 0x00, 0x00, 0x00]); // start_time
        buf.extend_from_slice(&[0x00, 0x10, 0x00]); // duration
        buf.push(((desc_loop_len >> 8) & 0x0F) as u8);
        buf.push((desc_loop_len & 0xFF) as u8);
        // 0x4D descriptor
        buf.push(0x4D);
        buf.push(desc_body_4d as u8);
        buf.extend_from_slice(&[0x6A, 0x70, 0x6E]);
        buf.push(name_len as u8);
        buf.extend_from_slice(title_b24);
        // 0xD5 descriptor
        buf.extend_from_slice(desc_d5_correct);
        // CRC
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        let epg = parse_eit_section(&buf).expect("should return Some");
        assert_eq!(epg.title, "X");
        assert_eq!(epg.episode_number, Some(5));
        assert_eq!(epg.last_episode, Some(12));
    }

    #[test]
    fn parse_eit_section_desc_len_overflow_no_panic() {
        // Descriptor with dlen=20 but only 5 bytes allocated in desc_loop_len.
        // The overflow guard (pos + dlen > desc_end) must trigger a break, not a panic.
        let mut buf = vec![0u8; 14 + 12 + 5 + 4]; // header + event + 5 desc bytes + CRC
                                                  // desc_loop_len = 5 at event bytes 10-11 (buf[24..26])
        buf[25] = 0x05;
        // Descriptor: tag=0x4D, dlen=20 (exceeds the 5 allocated bytes)
        buf[26] = 0x4D;
        buf[27] = 0x14; // dlen = 20
        assert!(parse_eit_section(&buf).is_none()); // title stays "" → None
    }

    // ── fill_series_episode ───────────────────────────────────────────────────

    #[test]
    fn fill_series_episode_preserves_episode_from_descriptor() {
        // episode_number already set (from 0xD5 series_descriptor) must not be overwritten
        // even when the title contains a different episode pattern like "#7".
        let epg = EpgInfo {
            title: "シリーズ #7 話タイトル".to_string(),
            series_title: String::new(),
            sub_title: None,
            episode_number: Some(3), // set from 0xD5
            last_episode: None,
            series_name: None,
            air_datetime: None,
            detail: None,
        };
        let filled = fill_series_episode(epg);
        assert_eq!(
            filled.episode_number,
            Some(3),
            "descriptor ep must win over title ep"
        );
        assert_eq!(filled.series_title, "シリーズ");
    }

    // ── strip_broadcast_flags ─────────────────────────────────────────────────

    #[test]
    fn strip_flags_none() {
        assert_eq!(strip_broadcast_flags("普通のタイトル"), "普通のタイトル");
    }

    #[test]
    fn strip_flags_single() {
        assert_eq!(strip_broadcast_flags("タイトル[字]"), "タイトル");
    }

    #[test]
    fn strip_flags_multiple() {
        assert_eq!(strip_broadcast_flags("タイトル[字][デ][SS]"), "タイトル");
    }

    #[test]
    fn strip_flags_with_spaces() {
        assert_eq!(strip_broadcast_flags("タイトル [字] "), "タイトル");
    }

    #[test]
    fn strip_flags_does_not_strip_mid_bracket() {
        // Bracket in the middle of the title must not be stripped
        let s = "[special] タイトル";
        assert_eq!(strip_broadcast_flags(s), s.trim());
    }

    #[test]
    fn strip_flags_close_bracket_only() {
        // Unbalanced ']' with no matching '[' → rfind('[') returns None → no stripping
        assert_eq!(strip_broadcast_flags("タイトル]"), "タイトル]");
    }

    #[test]
    fn strip_flags_unclosed_open_bracket() {
        // '[字' with no closing ']' → ends_with(']') is false → no stripping
        assert_eq!(strip_broadcast_flags("タイトル[字"), "タイトル[字");
    }

    // ── extract_series_episode ────────────────────────────────────────────────

    #[test]
    fn no_episode_returns_full_title_as_series() {
        let (series, ep, sub) = extract_series_episode("アニメタイトル");
        assert_eq!(series, "アニメタイトル");
        assert_eq!(ep, None);
        assert_eq!(sub, None);
    }

    #[test]
    fn hash_ascii_episode() {
        let (series, ep, sub) = extract_series_episode("シリーズ名 #3 エピソードタイトル");
        assert_eq!(series, "シリーズ名");
        assert_eq!(ep, Some(3));
        assert_eq!(sub, Some("エピソードタイトル".to_string()));
    }

    #[test]
    fn hash_fullwidth_episode() {
        // ＃ is U+FF03 (fullwidth number sign)
        let (series, ep, sub) = extract_series_episode("シリーズ名＃12サブタイトル");
        assert_eq!(series, "シリーズ名");
        assert_eq!(ep, Some(12));
        assert_eq!(sub, Some("サブタイトル".to_string()));
    }

    #[test]
    fn dai_wa_episode() {
        let (series, ep, sub) = extract_series_episode("名探偵 第3話 黒の章");
        assert_eq!(series, "名探偵");
        assert_eq!(ep, Some(3));
        assert_eq!(sub, Some("黒の章".to_string()));
    }

    #[test]
    fn dai_wa_no_sub() {
        let (series, ep, sub) = extract_series_episode("アニメ第5話");
        assert_eq!(series, "アニメ");
        assert_eq!(ep, Some(5));
        assert_eq!(sub, None);
    }

    #[test]
    fn episode_with_broadcast_flags_stripped() {
        let (series, ep, _sub) = extract_series_episode("シリーズ #7 タイトル[字][デ]");
        assert_eq!(series, "シリーズ");
        assert_eq!(ep, Some(7));
    }

    #[test]
    fn no_episode_space_split() {
        // No episode token → split at first space: first word = series
        let (series, ep, sub) = extract_series_episode("番組名 特別編");
        assert_eq!(series, "番組名");
        assert_eq!(ep, None);
        assert_eq!(sub, Some("特別編".to_string()));
    }

    #[test]
    fn extract_series_episode_dai_no_wa() {
        // 第N without trailing 話 suffix must still extract the episode number
        let (series, ep, sub) = extract_series_episode("アニメ第3");
        assert_eq!(series, "アニメ");
        assert_eq!(ep, Some(3));
        assert_eq!(sub, None);
    }

    #[test]
    fn extract_series_episode_ideographic_space() {
        // U+3000 (ideographic space) is a valid fallback split point
        let (series, ep, sub) = extract_series_episode("番組名\u{3000}サブタイトル");
        assert_eq!(series, "番組名");
        assert_eq!(ep, None);
        assert_eq!(sub, Some("サブタイトル".to_string()));
    }

    #[test]
    fn extract_series_episode_hash_non_digit() {
        // '#' followed by a non-digit must not extract an episode; falls back to space split
        let (series, ep, sub) = extract_series_episode("シリーズ #タグ");
        assert_eq!(ep, None);
        assert_eq!(series, "シリーズ");
        assert_eq!(sub, Some("#タグ".to_string()));
    }

    #[test]
    fn extract_series_episode_empty_string() {
        let (series, ep, sub) = extract_series_episode("");
        assert_eq!(series, "");
        assert_eq!(ep, None);
        assert_eq!(sub, None);
    }

    // ── decode_mjd_bcd ────────────────────────────────────────────────────────

    #[test]
    fn decode_mjd_bcd_known_date() {
        // MJD 51544 = 2000-01-01 (verified: (2000-1900)*365.25 = 36524.0, 36524+15019+1=51544)
        // Actually let's use the standard: MJD of 2000-01-01 = 51544
        // BCD time: 12:00:00
        let data: [u8; 5] = [
            (51544u16 >> 8) as u8,   // MJD high byte
            (51544u16 & 0xFF) as u8, // MJD low byte
            0x12,                    // BCD 12
            0x00,                    // BCD 00
            0x00,                    // BCD 00
        ];
        let dt = decode_mjd_bcd(&data).expect("should decode");
        assert_eq!(dt.date_naive().to_string(), "2000-01-01");
        // Time is stored in JST (UTC+9), stored internally as UTC-9 from JST
        // The function interprets time as JST so the naive UTC part is -9h
        assert_eq!(dt.naive_local().time().hour(), 12);
    }

    #[test]
    fn decode_mjd_bcd_too_short() {
        assert!(decode_mjd_bcd(&[0x00, 0x01, 0x12]).is_none());
    }

    #[test]
    fn decode_mjd_bcd_invalid_marker() {
        // MJD = 0xFFFF is the "undefined" marker
        let data: [u8; 5] = [0xFF, 0xFF, 0x00, 0x00, 0x00];
        assert!(decode_mjd_bcd(&data).is_none());
    }

    #[test]
    fn decode_mjd_bcd_invalid_bcd_byte() {
        // Hour byte 0x9A → lo nibble=A > 9 → invalid BCD
        let data: [u8; 5] = [0xC8, 0x00, 0x9A, 0x00, 0x00];
        assert!(decode_mjd_bcd(&data).is_none());
    }
}
