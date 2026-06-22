// ARIB STD-B24 caption PES demultiplexer.
//
// Reads a TS file sequentially from the beginning (no seeking), reconstructs
// PES packets for the ARIB caption elementary stream, and returns the raw
// PES payload bytes together with the presentation timestamp in milliseconds.
//
// PTS normalisation: the first observed caption PTS (90 kHz absolute) is used
// as the reference epoch, matching the convention used by libaribcaption
// (i.e. the same origin as captions.pts_start).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

// One reassembled PES unit from the caption elementary stream.
#[derive(Debug, Serialize, Deserialize)]
pub struct CaptionPes {
    /// PTS in milliseconds, normalised so that the first packet in the stream = 0.
    pub pts_ms: i64,
    /// Raw PES payload bytes (data_identifier = 0x80, … onward).
    pub payload: Vec<u8>,
}

// ── PES blob serialization ─────────────────────────────────────────────────

/// Serialize a list of PES units to a binary blob file (bincode format).
///
/// Stored at `cache/{stem}/captions.pes` during ingest.  The blob is small
/// (a few hundred KiB) and allows the subtitle renderer to replay the full
/// PES sequence on-demand at thumbnail generation time without re-reading the TS.
pub fn write_pes_blob(path: &Path, pes: &[CaptionPes]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let encoded = bincode::serialize(pes)?;
    let mut f = File::create(path)?;
    f.write_all(&encoded)?;
    Ok(())
}

/// Deserialize a PES blob previously written by [`write_pes_blob`].
pub fn read_pes_blob(path: &Path) -> Result<Vec<CaptionPes>> {
    let data = std::fs::read(path)?;
    Ok(bincode::deserialize(&data)?)
}

// ── PAT/PMT scanner ───────────────────────────────────────────────────────

/// Results from a single PAT+PMT pass over the TS header.
pub struct PsiInfo {
    /// Service IDs (program numbers) that carry an ARIB caption stream.
    /// Falls back to all services with any stream_type=0x06 if no ARIB
    /// descriptor is found.  Used by the EIT scanner to filter the right
    /// service's present/following table.
    pub caption_services: Vec<u16>,
    /// Elementary PID of the ARIB caption stream (stream_type=0x06,
    /// data_component_id=0x0008 or 0x0012).  Falls back to the first
    /// stream_type=0x06 PID.  Used by the PES demultiplexer.
    pub caption_pid: Option<u16>,
}

/// Scan the first ~50 000 TS packets, parse PAT + all PMTs in one pass, and
/// return both the caption service IDs and the caption elementary PID.
///
/// Replaces the former `find_caption_pid` (pes.rs) and `find_caption_services`
/// (epg.rs) which each performed the same PAT/PMT walk independently.
pub fn scan_psi(ts_path: &Path) -> PsiInfo {
    let mut file = match File::open(ts_path) {
        Ok(f) => f,
        Err(_) => return PsiInfo { caption_services: vec![], caption_pid: None },
    };
    let mut packet = [0u8; 188];

    // pid → service_id (program_number) for PMT PIDs found in PAT
    let mut pmt_pids: HashMap<u16, u16> = HashMap::new();
    // section accumulation buffers: pid → (bytes_so_far, total_expected)
    let mut bufs: HashMap<u16, (Vec<u8>, usize)> = HashMap::new();
    let mut checked_pmts = std::collections::HashSet::<u16>::new();

    // Services with confirmed ARIB caption descriptor (preferred).
    let mut arib_services: Vec<u16> = Vec::new();
    // Services with any 0x06 stream but no ARIB descriptor (fallback).
    let mut fallback_services: Vec<u16> = Vec::new();
    // Caption ES PIDs — same split.
    let mut arib_pid: Option<u16> = None;
    let mut fallback_pid: Option<u16> = None;

    for _ in 0..50_000u32 {
        if file.read_exact(&mut packet).is_err() {
            break;
        }
        if packet[0] != 0x47 {
            continue;
        }

        let pid = ((packet[1] as u16 & 0x1F) << 8) | packet[2] as u16;

        // Only care about PAT and known PMT PIDs.
        if pid != 0x0000 && !pmt_pids.contains_key(&pid) {
            continue;
        }

        let pusi = (packet[1] & 0x40) != 0;
        let afc = (packet[3] & 0x30) >> 4;
        if afc == 2 {
            continue; // adaptation field only, no payload
        }

        let mut ps = 4usize;
        if afc == 3 {
            let af_len = packet[4] as usize;
            ps = 5 + af_len;
        }
        if ps >= 188 {
            continue;
        }

        if pusi {
            let ptr = packet[ps] as usize;
            ps += 1 + ptr;
            if ps + 3 > 188 {
                continue;
            }
            let slen =
                (((packet[ps + 1] as usize) & 0x0F) << 8) | packet[ps + 2] as usize;
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

        let complete = bufs
            .get(&pid)
            .map(|e| e.0.len() >= e.1 && e.1 > 0)
            .unwrap_or(false);
        if !complete {
            continue;
        }

        let data = bufs[&pid].0.clone();

        if pid == 0x0000 {
            // PAT: extract service_id → PMT PID mapping.
            if data.len() < 8 {
                continue;
            }
            let slen =
                (((data[1] as usize) & 0x0F) << 8) | data[2] as usize;
            let end = (3 + slen).min(data.len()).saturating_sub(4);
            let mut p = 8usize;
            while p + 4 <= end {
                let svc  = ((data[p]     as u16) << 8) | data[p + 1] as u16;
                let pmt  = ((data[p + 2] as u16 & 0x1F) << 8) | data[p + 3] as u16;
                if svc != 0 {
                    pmt_pids.insert(pmt, svc);
                }
                p += 4;
            }
        } else if let Some(&svc) = pmt_pids.get(&pid) {
            // PMT: walk ES entries to find caption streams.
            if checked_pmts.contains(&pid) {
                continue;
            }
            checked_pmts.insert(pid);

            if data.len() < 12 {
                continue;
            }
            let slen =
                (((data[1] as usize) & 0x0F) << 8) | data[2] as usize;
            let prog_info_len =
                (((data[10] as usize) & 0x0F) << 8) | data[11] as usize;
            let end = (3 + slen).min(data.len()).saturating_sub(4);
            let mut p = 12 + prog_info_len;

            let mut has_arib = false;
            let mut has_private = false;

            while p + 5 <= end {
                let stream_type = data[p];
                let es_pid = ((data[p + 1] as u16 & 0x1F) << 8) | data[p + 2] as u16;
                let es_info_len =
                    (((data[p + 3] as usize) & 0x0F) << 8) | data[p + 4] as usize;

                if stream_type == 0x06 {
                    has_private = true;
                    // Scan ES descriptors for data_component_descriptor (tag=0xFD).
                    let desc_start = p + 5;
                    let desc_end = (desc_start + es_info_len).min(end);
                    let mut d = desc_start;
                    let mut is_arib_es = false;
                    while d + 2 <= desc_end {
                        let tag  = data[d];
                        let dlen = data[d + 1] as usize;
                        if tag == 0xFD && dlen >= 2 && d + 2 + dlen <= desc_end {
                            let dc_id = ((data[d + 2] as u16) << 8) | data[d + 3] as u16;
                            if dc_id == 0x0008 || dc_id == 0x0012 {
                                is_arib_es = true;
                            }
                        }
                        d += 2 + dlen;
                    }
                    if is_arib_es {
                        has_arib = true;
                        if arib_pid.is_none() {
                            arib_pid = Some(es_pid);
                        }
                    } else if fallback_pid.is_none() {
                        fallback_pid = Some(es_pid);
                    }
                }
                p += 5 + es_info_len;
            }

            if has_arib && !arib_services.contains(&svc) {
                arib_services.push(svc);
            } else if has_private && !fallback_services.contains(&svc) {
                fallback_services.push(svc);
            }

            // Stop once all PMTs in the PAT have been processed.
            if !pmt_pids.is_empty() && checked_pmts.len() >= pmt_pids.len() {
                break;
            }
        }
    }

    let caption_services = if !arib_services.is_empty() {
        arib_services
    } else {
        fallback_services
    };
    let caption_pid = arib_pid.or(fallback_pid);

    PsiInfo { caption_services, caption_pid }
}

/// Thin wrapper kept for backward compatibility.
/// Callers that only need the caption PID can use this directly;
/// callers that also need service IDs should use `scan_psi` instead.
pub fn find_caption_pid(ts_path: &Path) -> Option<u16> {
    scan_psi(ts_path).caption_pid
}

// ── PES demultiplexer ─────────────────────────────────────────────────────

/// Read the entire TS file and return all caption PES units for `caption_pid`.
///
/// Packets are accumulated into PES frames using the PUSI flag.  PTS values
/// are extracted from the PES header (33-bit, 90 kHz) and converted to ms.
/// The first observed PTS becomes the reference epoch (pts = 0).
pub fn demux_caption_pes(ts_path: &Path, caption_pid: u16) -> Vec<CaptionPes> {
    let mut file = match File::open(ts_path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    let mut packet = [0u8; 188];

    let mut result: Vec<CaptionPes> = Vec::new();

    // Accumulator for the current PES in progress.
    let mut pes_buf: Vec<u8> = Vec::new();
    let mut pes_total: usize = 0;  // declared PES packet length (0 = unbounded)
    let mut pes_pts_90k: Option<u64> = None;

    // Reference epoch: abs 90kHz PTS of the first packet seen.
    let mut epoch: Option<u64> = None;

    loop {
        if file.read_exact(&mut packet).is_err() {
            break;
        }
        if packet[0] != 0x47 {
            continue;
        }

        let pid = ((packet[1] as u16 & 0x1F) << 8) | packet[2] as u16;
        if pid != caption_pid {
            continue;
        }

        let pusi = (packet[1] & 0x40) != 0;
        let afc = (packet[3] & 0x30) >> 4;
        if afc == 2 {
            continue;
        }

        let mut ps = 4usize;
        if afc == 3 {
            let af_len = packet[4] as usize;
            ps = 5 + af_len;
        }
        if ps >= 188 {
            continue;
        }

        if pusi {
            // Flush the previously accumulated PES (if any) before starting new.
            if !pes_buf.is_empty() {
                if let Some(pts90) = pes_pts_90k {
                    let ep = *epoch.get_or_insert(pts90);
                    let pts_ms = (pts90.wrapping_sub(ep) / 90) as i64;
                    let payload = extract_pes_payload(&pes_buf);
                    if !payload.is_empty() {
                        result.push(CaptionPes { pts_ms, payload });
                    }
                }
            }

            // Start accumulating a new PES.
            pes_buf.clear();
            pes_total = 0;
            pes_pts_90k = None;

            let avail = 188 - ps;
            // PES header needs at least 9 bytes to hold the pts_dts_flags field.
            if avail < 9 {
                continue;
            }

            // PES packet length field (bytes 4-5 of the PES header, 0 = unbounded).
            let pes_len =
                ((packet[ps + 4] as usize) << 8) | packet[ps + 5] as usize;
            // total = 6 (fixed header) + pes_len bytes;  0 = unbounded stream.
            pes_total = if pes_len == 0 { 0 } else { 6 + pes_len };

            // PTS is present when pts_dts_flags[7:6] != 0b00
            let pts_dts_flags = (packet[ps + 7] >> 6) & 0x03;
            if pts_dts_flags != 0 && avail >= 14 {
                let p = &packet[ps + 9..];  // PES optional header start
                if p.len() >= 5 {
                    let pts = parse_pts(p);
                    pes_pts_90k = Some(pts);
                    epoch.get_or_insert(pts);
                }
            }

            let take = if pes_total > 0 {
                avail.min(pes_total)
            } else {
                avail
            };
            pes_buf.extend_from_slice(&packet[ps..ps + take]);
        } else {
            // Continue accumulating current PES.
            if pes_buf.is_empty() {
                continue;
            }
            let avail = 188 - ps;
            let take = if pes_total > 0 {
                let rem = pes_total.saturating_sub(pes_buf.len());
                avail.min(rem)
            } else {
                avail
            };
            if take == 0 {
                continue;
            }
            pes_buf.extend_from_slice(&packet[ps..ps + take]);
        }
    }

    // Flush any trailing PES.
    if !pes_buf.is_empty() {
        if let Some(pts90) = pes_pts_90k {
            let ep = *epoch.get_or_insert(pts90);
            let pts_ms = (pts90.wrapping_sub(ep) / 90) as i64;
            let payload = extract_pes_payload(&pes_buf);
            if !payload.is_empty() {
                result.push(CaptionPes { pts_ms, payload });
            }
        }
    }

    result
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Decode a 5-byte PTS field from a PES optional header.
/// The 33-bit value is packed as: [4 bits marker] [15 bits] [marker] [15 bits] [marker].
pub(crate) fn parse_pts(p: &[u8]) -> u64 {
    let b0 = p[0] as u64;
    let b1 = p[1] as u64;
    let b2 = p[2] as u64;
    let b3 = p[3] as u64;
    let b4 = p[4] as u64;
    ((b0 & 0x0E) << 29) | (b1 << 22) | ((b2 & 0xFE) << 14) | (b3 << 7) | (b4 >> 1)
}

/// Skip the fixed (6-byte) + optional (variable-length) PES header and return
/// the payload slice, which starts with data_identifier (0x80 for ARIB).
pub(crate) fn extract_pes_payload(pes: &[u8]) -> Vec<u8> {
    // Fixed PES header: 3 (start_code+stream_id) + 2 (packet_length) + 1 (flags1) = 6 bytes minimum.
    // The optional header length is in byte 8 (index 8 from start, 0-based).
    // Total header = 9 + optional_header_length.
    if pes.len() < 9 {
        return vec![];
    }
    let opt_len = pes[8] as usize;
    let header_total = 9 + opt_len;
    if header_total >= pes.len() {
        return vec![];
    }
    pes[header_total..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::{extract_pes_payload, parse_pts};

    // ── parse_pts ──────────────────────────────────────────────────────────────
    //
    // PTS encoding (MPEG-2 13818-1):
    //   byte[0]: [4-bit tag] [pts[32:30]] [marker=1]
    //   byte[1]: [pts[29:22]]
    //   byte[2]: [pts[21:15]] [marker=1]
    //   byte[3]: [pts[14: 7]]
    //   byte[4]: [pts[ 6: 0]<<1] [marker=1]
    //
    // Encoding a known PTS value of 0 (all PTS bits = 0):
    //   byte[0] = 0x01 (tag=0, pts[32:30]=000, marker=1)
    //   byte[1] = 0x00
    //   byte[2] = 0x01 (pts[21:15]=0000000, marker=1)
    //   byte[3] = 0x00
    //   byte[4] = 0x01 (pts[6:0]=0000000, marker=1)

    fn encode_pts(pts: u64) -> [u8; 5] {
        [
            (((pts >> 29) & 0x0E) | 0x01) as u8, // [bits32:30 in b3:b1] | marker
            ((pts >> 22) & 0xFF) as u8,
            ((((pts >> 14) & 0xFE) | 0x01)) as u8,
            ((pts >> 7) & 0xFF) as u8,
            (((pts & 0x7F) << 1) | 0x01) as u8,
        ]
    }

    #[test]
    fn parse_pts_zero() {
        let bytes = encode_pts(0);
        assert_eq!(parse_pts(&bytes), 0);
    }

    #[test]
    fn parse_pts_known_value() {
        // PTS = 90_000 (1 second at 90 kHz)
        let pts: u64 = 90_000;
        let bytes = encode_pts(pts);
        assert_eq!(parse_pts(&bytes), pts);
    }

    #[test]
    fn parse_pts_max_33bit() {
        // Max 33-bit value = 2^33 - 1 = 8_589_934_591
        let pts: u64 = (1 << 33) - 1;
        let bytes = encode_pts(pts);
        assert_eq!(parse_pts(&bytes), pts);
    }

    #[test]
    fn parse_pts_typical_ts_value() {
        // Typical first PTS around 1 hour into a 90kHz clock
        let pts: u64 = 90_000 * 3600; // 324_000_000
        let bytes = encode_pts(pts);
        assert_eq!(parse_pts(&bytes), pts);
    }

    // ── extract_pes_payload ────────────────────────────────────────────────────

    fn make_pes(opt_header_len: u8, payload: &[u8]) -> Vec<u8> {
        // Minimal valid PES: 6 fixed bytes + 3 optional-header-prefix bytes + opt payload + payload
        // Byte layout:
        //   [0..3]  = start_code + stream_id  (3 bytes)
        //   [3..5]  = packet_length (2 bytes, 0=unbounded)
        //   [5]     = flags1
        //   [6]     = flags2
        //   [7]     = flags3 (unused here)
        //   [8]     = optional_header_length
        //   [9 .. 9+opt_header_len] = optional header padding
        //   [9+opt_header_len ..]   = payload
        let header_total = 9 + opt_header_len as usize;
        let mut pes = vec![0u8; header_total];
        pes[8] = opt_header_len;
        pes.extend_from_slice(payload);
        pes
    }

    #[test]
    fn extract_pes_payload_basic() {
        let payload = b"\x80\x01\x02\x03";
        let pes = make_pes(0, payload);
        assert_eq!(extract_pes_payload(&pes), payload);
    }

    #[test]
    fn extract_pes_payload_with_optional_header() {
        let payload = b"\xAA\xBB";
        let pes = make_pes(5, payload);  // 5-byte optional header
        assert_eq!(extract_pes_payload(&pes), payload);
    }

    #[test]
    fn extract_pes_payload_too_short_returns_empty() {
        // PES shorter than 9 bytes → empty
        assert_eq!(extract_pes_payload(&[0u8; 8]), Vec::<u8>::new());
    }

    #[test]
    fn extract_pes_payload_header_fills_whole_pes_returns_empty() {
        // opt_len causes header_total == pes.len() → empty payload
        let pes = vec![0u8; 9]; // opt_len=0 → header_total=9 == len → empty
        assert_eq!(extract_pes_payload(&pes), Vec::<u8>::new());
    }
}
