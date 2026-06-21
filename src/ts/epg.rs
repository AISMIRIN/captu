use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Command;

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
    if data.len() < 5 { return None; }
    let mjd = ((data[0] as u32) << 8) | data[1] as u32;
    if mjd == 0xFFFF { return None; }

    // MJD to calendar date (algorithm from ARIB STD-B10)
    let yp = ((mjd as f64 - 15078.2) / 365.25) as i32;
    let mp = ((mjd as f64 - 14956.1 - (yp as f64 * 365.25).floor()) / 30.6001) as i32;
    let day = mjd as i32 - 14956 - (yp as f64 * 365.25).floor() as i32
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
    Some(DateTime::from_naive_utc_and_offset(naive - chrono::Duration::hours(9), jst))
}

fn bcd_byte(b: u8) -> Option<u8> {
    let hi = b >> 4;
    let lo = b & 0x0F;
    if hi > 9 || lo > 9 { return None; }
    Some(hi * 10 + lo)
}

fn parse_eit_section(data: &[u8]) -> Option<EpgInfo> {
    // Header: table_id(1) + section_syntax(2) + service_id(2) + version(1)
    //         + section/last_section(2) + tsid(2) + onid(2) + seg_last(1) + last_tid(1) = 14 bytes
    if data.len() < 18 { return None; }

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
            if pos + dlen > desc_end { break; }
            let d = &data[pos..pos + dlen];

            match tag {
                0x4D => {
                    // short_event_descriptor: lang(3) + name_len(1) + name + text_len(1) + text
                    if dlen >= 4 {
                        let name_len = d[3] as usize;
                        if 4 + name_len <= dlen {
                            epg.title = decode_arib_b24(&d[4..4 + name_len]);
                        }
                        let text_pos = 4 + name_len;
                        if text_pos + 1 <= dlen {
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
                        if ep > 0 { epg.episode_number = Some(ep); }
                        if last > 0 { epg.last_episode = Some(last); }
                        let name_len = d[8] as usize;
                        if 9 + name_len <= dlen {
                            let s = decode_arib_b24(&d[9..9 + name_len]);
                            if !s.is_empty() { epg.series_name = Some(s); }
                        }
                    }
                }
                0x4E => {
                    // extended_event_descriptor: descriptor_num(4)+last(4)+lang(3)+items_len(1)+items+text_len(1)+text
                    if dlen >= 5 {
                        let items_len = d[4] as usize;
                        let text_pos = 5 + items_len;
                        if text_pos + 1 <= dlen {
                            let text_len = d[text_pos] as usize;
                            if text_pos + 1 + text_len <= dlen && text_len > 0 {
                                let t = decode_arib_b24(&d[text_pos + 1..text_pos + 1 + text_len]);
                                if !t.is_empty() { epg.detail = Some(t); }
                            }
                        }
                    }
                }
                _ => {}
            }
            pos += dlen;
        }
        pos = desc_end;
        if !epg.title.is_empty() { break; }
    }

    if epg.title.is_empty() { None } else { Some(epg) }
}

// Ask ffprobe which program contains the first subtitle stream.
// Parses `-show_programs -show_streams -print_format ini` output:
// program_id lines appear before their program's streams, so the program_id
// immediately preceding a `codec_type=subtitle` line is the one we want.
fn get_subtitle_service_id(ts_path: &Path) -> Option<u16> {
    let out = Command::new("ffprobe")
        .args([
            "-v", "quiet",
            "-show_programs",
            "-show_streams",
            "-print_format", "ini",
            ts_path.to_str()?,
        ])
        .output()
        .ok()?;

    let text = String::from_utf8_lossy(&out.stdout);
    let mut current_prog: Option<u16> = None;
    for line in text.lines() {
        if let Some(val) = line.strip_prefix("program_id=") {
            current_prog = val.trim().parse::<u16>().ok();
        } else if line.starts_with("codec_type=subtitle") {
            if let Some(prog) = current_prog {
                return Some(prog);
            }
        }
    }
    None
}

// Scan PAT + PMTs and return all service_ids that have an ARIB caption stream.
// ARIB captions: stream_type=0x06 with data_component_descriptor (tag=0xFD,
// data_component_id=0x0008 for caption or 0x0012 for superimpose).
// Falls back to any 0x06 stream if no descriptor found.
fn find_caption_services(ts_path: &Path) -> Vec<u16> {
    let mut file = match File::open(ts_path) { Ok(f) => f, Err(_) => return vec![] };
    let mut packet = [0u8; 188];

    let mut pmt_pids: std::collections::HashMap<u16, u16> = std::collections::HashMap::new();
    let mut checked_pmts: std::collections::HashSet<u16> = std::collections::HashSet::new();
    let mut bufs: std::collections::HashMap<u16, (Vec<u8>, usize)> = std::collections::HashMap::new();
    let mut caption_svcs: Vec<u16> = Vec::new();
    let mut fallback_svcs: Vec<u16> = Vec::new(); // services with 0x06 but no ARIB descriptor

    for _ in 0..50_000u32 {
        if file.read_exact(&mut packet).is_err() { break; }
        if packet[0] != 0x47 { continue; }

        let pid = ((packet[1] as u16 & 0x1F) << 8) | packet[2] as u16;
        if pid != 0x0000 && !pmt_pids.contains_key(&pid) { continue; }

        let pusi = (packet[1] & 0x40) != 0;
        let afc = (packet[3] & 0x30) >> 4;
        if afc == 2 { continue; }

        let mut ps = 4usize;
        if afc == 3 { ps = 5 + packet[4] as usize; }
        if ps >= 188 { continue; }

        if pusi {
            let ptr = packet[ps] as usize;
            ps += 1 + ptr;
            if ps + 3 > 188 { continue; }
            let slen = (((packet[ps + 1] as usize) & 0x0F) << 8) | packet[ps + 2] as usize;
            let total = 3 + slen;
            let avail = 188 - ps;
            let entry = bufs.entry(pid).or_default();
            entry.0.clear();
            entry.0.extend_from_slice(&packet[ps..ps + total.min(avail)]);
            entry.1 = total;
        } else if let Some(entry) = bufs.get_mut(&pid) {
            if !entry.0.is_empty() {
                let rem = entry.1.saturating_sub(entry.0.len());
                let avail = 188 - ps;
                let take = rem.min(avail);
                entry.0.extend_from_slice(&packet[ps..ps + take]);
            }
        }

        if let Some(entry) = bufs.get(&pid) {
            if entry.0.len() < entry.1 || entry.1 == 0 { continue; }
        } else { continue; }

        let data = bufs[&pid].0.clone();

        if pid == 0x0000 {
            if data.len() < 8 { continue; }
            let slen = (((data[1] as usize) & 0x0F) << 8) | data[2] as usize;
            let end = (3 + slen).min(data.len()).saturating_sub(4);
            let mut p = 8usize;
            while p + 4 <= end {
                let prog = ((data[p] as u16) << 8) | data[p + 1] as u16;
                let pmt  = ((data[p + 2] as u16 & 0x1F) << 8) | data[p + 3] as u16;
                if prog != 0 { pmt_pids.insert(pmt, prog); }
                p += 4;
            }
        } else if let Some(&prog) = pmt_pids.get(&pid) {
            if checked_pmts.contains(&pid) { continue; }
            checked_pmts.insert(pid);
            if data.len() < 12 { continue; }

            let prog_info_len = (((data[10] as usize) & 0x0F) << 8) | data[11] as usize;
            let slen = (((data[1] as usize) & 0x0F) << 8) | data[2] as usize;
            let end = (3 + slen).min(data.len()).saturating_sub(4);
            let mut p = 12 + prog_info_len;

            let mut has_arib_caption = false;
            let mut has_private_stream = false;

            while p + 5 <= end {
                let stream_type = data[p];
                let es_info_len = (((data[p + 3] as usize) & 0x0F) << 8) | data[p + 4] as usize;
                if stream_type == 0x06 {
                    has_private_stream = true;
                    // Scan ES_info for data_component_descriptor (tag=0xFD)
                    let desc_start = p + 5;
                    let desc_end = (desc_start + es_info_len).min(end);
                    let mut d = desc_start;
                    while d + 2 <= desc_end {
                        let tag = data[d];
                        let dlen = data[d + 1] as usize;
                        if tag == 0xFD && dlen >= 2 && d + 2 + dlen <= desc_end {
                            let dc_id = ((data[d + 2] as u16) << 8) | data[d + 3] as u16;
                            if dc_id == 0x0008 || dc_id == 0x0012 {
                                has_arib_caption = true;
                            }
                        }
                        d += 2 + dlen;
                    }
                }
                p += 5 + es_info_len;
            }

            if has_arib_caption && !caption_svcs.contains(&prog) {
                caption_svcs.push(prog);
            } else if has_private_stream && !fallback_svcs.contains(&prog) {
                fallback_svcs.push(prog);
            }

            if !checked_pmts.is_empty() && checked_pmts.len() >= pmt_pids.len() { break; }
        }
    }

    if !caption_svcs.is_empty() { caption_svcs } else { fallback_svcs }
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
        if file.read_exact(&mut packet).is_err() { return None; }
        if packet[0] != 0x47 { continue; }

        let pid = ((packet[1] as u16 & 0x1F) << 8) | packet[2] as u16;
        if pid != 0x0012 { continue; }

        let pusi = (packet[1] & 0x40) != 0;
        let afc = (packet[3] & 0x30) >> 4;
        if afc == 2 { continue; }

        let mut ps = 4usize;
        if afc == 3 { ps = 5 + packet[4] as usize; }
        if ps >= 188 { continue; }

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
            if ps + 8 > 188 { continue; }

            let table_id = packet[ps];
            // Only EIT[p/f] present/following (0x4E). Schedule tables (0x50-0x6F)
            // span the full day and would return the previous programme.
            if table_id != 0x4E { continue; }
            if packet[ps + 1] & 0x80 == 0 { continue; }

            // service_id filter (bytes 3-4 of the section)
            let sec_svc = ((packet[ps + 3] as u16) << 8) | packet[ps + 4] as u16;
            if !service_ids.is_empty() && !service_ids.contains(&sec_svc) { continue; }

            // section_number=0: current event (present). section_number=1 is "following".
            if packet[ps + 6] != 0 { continue; }

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
        if val > 9999 { break; }
        byte_len += c.len_utf8();
        found = true;
    }
    if found && val <= u16::MAX as u32 { Some((val as u16, byte_len)) } else { None }
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
                    if sub.is_empty() { None } else { Some(sub.to_string()) },
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
                    if sub.is_empty() { None } else { Some(sub.to_string()) },
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
        (series, None, if sub.is_empty() { None } else { Some(sub.to_string()) })
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

pub fn extract_epg(ts_path: &Path) -> Result<EpgInfo> {
    let file_size = std::fs::metadata(ts_path)?.len();
    // Prefer ffprobe result (most reliable); fall back to PMT-based scan
    let caption_svcs = if let Some(svc) = get_subtitle_service_id(ts_path) {
        vec![svc]
    } else {
        find_caption_services(ts_path)
    };

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
    let air_datetime = std::fs::metadata(ts_path).ok()
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
