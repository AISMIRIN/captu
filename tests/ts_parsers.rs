// Integration tests for TS parser functions: scan_psi, find_caption_pid, demux_caption_pes.
//
// All tests build synthetic 188-byte TS packet streams written to tempfiles and
// passed to the parser functions.  No real ISDB-T recording is required.
//
// TS packet structure (ISO 13818-1):
//   [0]     sync byte      0x47
//   [1]     PUSI(1) + PID high(5 bits) in b6:b0
//   [2]     PID low (8 bits)
//   [3]     adaptation field control(2) in b5:b4 + CC(4)
//   [4+]    payload (or adaptation field then payload)
//
// Section accumulation (PAT/PMT):
//   With PUSI=1: [4] pointer_field, then section starts at [5+pointer].
//   Section format: table_id(1) | section_length(12) | section_body | CRC(4).
//
// PES structure: packet_start_code_prefix(3) | stream_id(1) | packet_length(2)
//                | flags(2) | optional_header_length(1) | optional_hdr | payload.

use std::io::Write;

use captu::ts::pes::{demux_caption_pes, find_caption_pid, scan_psi, CaptionPes};

// ── TS packet builders ────────────────────────────────────────────────────────

/// Build a single 188-byte TS packet carrying a section (PAT or PMT).
/// `section` is the full section bytes (table_id through CRC).
/// Fits entirely in one packet; panics if section > 182 bytes.
fn section_packet(pid: u16, section: &[u8]) -> [u8; 188] {
    assert!(section.len() <= 182, "section too large for single packet");
    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47;
    pkt[1] = 0x40 | ((pid >> 8) as u8 & 0x1F); // PUSI=1
    pkt[2] = (pid & 0xFF) as u8;
    pkt[3] = 0x10; // no adaptation field, CC=0
    pkt[4] = 0x00; // pointer_field = 0
    pkt[5..5 + section.len()].copy_from_slice(section);
    pkt
}

/// Build a 188-byte TS packet carrying PES data.
/// With PUSI=1: `data` is the start of a new PES packet.
/// With PUSI=0: `data` is continuation payload.
fn pes_packet(pid: u16, pusi: bool, data: &[u8]) -> [u8; 188] {
    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47;
    pkt[1] = (if pusi { 0x40 } else { 0x00 }) | ((pid >> 8) as u8 & 0x1F);
    pkt[2] = (pid & 0xFF) as u8;
    pkt[3] = 0x10; // no adaptation field, CC=0
    let take = data.len().min(184);
    pkt[4..4 + take].copy_from_slice(&data[..take]);
    pkt
}

/// Encode a 33-bit PTS value into the 5-byte PES PTS field.
fn encode_pts(pts: u64) -> [u8; 5] {
    [
        (0x20u8 | ((pts >> 29) & 0x0E) as u8 | 0x01), // tag bits 0010 | pts[32:30] | marker
        ((pts >> 22) & 0xFF) as u8,
        (((pts >> 14) & 0xFE) as u8 | 0x01),
        ((pts >> 7) & 0xFF) as u8,
        (((pts & 0x7F) << 1) as u8 | 0x01),
    ]
}

/// Build a PAT section (table_id=0x00) with one program entry.
fn pat_section(svc_id: u16, pmt_pid: u16) -> Vec<u8> {
    // Fixed header (5 bytes) + 1 program (4 bytes) + CRC (4 bytes) = 13 bytes after prefix.
    // section_length = 13.
    let slen: u16 = 13;
    let mut s = vec![
        0x00,                              // table_id
        0xB0 | ((slen >> 8) as u8 & 0x0F), // syntax indicator + length high
        (slen & 0xFF) as u8,               // length low
        0x00,                              // ts_id high
        0x01,                              // ts_id low
        0xC1,                              // version=0, current_next=1
        0x00,                              // section_number
        0x00,                              // last_section_number
        (svc_id >> 8) as u8,
        (svc_id & 0xFF) as u8,
        0xE0 | ((pmt_pid >> 8) as u8 & 0x1F),
        (pmt_pid & 0xFF) as u8,
    ];
    s.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // CRC (unchecked)
    s
}

/// Build a PMT section (table_id=0x02) with one ES entry carrying the ARIB
/// data_component_descriptor (tag=0xFD, data_component_id=0x0008).
fn pmt_section_with_arib(svc_id: u16, caption_pid: u16) -> Vec<u8> {
    // descriptor: tag(1) + len(1) + dc_id(2) = 4 bytes
    // ES entry: stream_type(1) + pid_high(1) + pid_low(1) + info_len(2) + descriptor(4) = 9 bytes
    // PMT fixed after length prefix: program_number(2)+version(1)+section_n(1)+last_n(1)+pcr_pid(2)+prog_info_len(2) = 9 bytes
    // Total after prefix: 9 + 9 + 4 (CRC) = 22 = section_length
    let slen: u16 = 22;
    let mut s = vec![
        0x02, // table_id
        0xB0 | ((slen >> 8) as u8 & 0x0F),
        (slen & 0xFF) as u8,
        (svc_id >> 8) as u8,
        (svc_id & 0xFF) as u8,
        0xC1, // version+current
        0x00, // section_number
        0x00, // last_section_number
        0xE0, // PCR_PID high (reserved)
        0x00, // PCR_PID low
        0xF0, // prog_info_len high (0)
        0x00, // prog_info_len low (0)
        // ES entry
        0x06, // stream_type = private data
        0xE0 | ((caption_pid >> 8) as u8 & 0x1F),
        (caption_pid & 0xFF) as u8,
        0xF0, // ES_info_len high (0)
        0x04, // ES_info_len low (4)
        // data_component_descriptor
        0xFD, // tag
        0x02, // length = 2
        0x00, // data_component_id high
        0x08, // data_component_id low = 0x0008 (ARIB STD-B24 caption)
    ];
    // CRC (unchecked)
    s.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    s
}

/// Build a PMT section with stream_type=0x06 but WITHOUT an ARIB descriptor (fallback).
fn pmt_section_no_arib(svc_id: u16, caption_pid: u16) -> Vec<u8> {
    // ES_info_len = 0 (no descriptors)
    let slen: u16 = 18; // 9 fixed + 5 ES entry + 4 CRC
    let mut s = vec![
        0x02,
        0xB0 | ((slen >> 8) as u8 & 0x0F),
        (slen & 0xFF) as u8,
        (svc_id >> 8) as u8,
        (svc_id & 0xFF) as u8,
        0xC1,
        0x00,
        0x00,
        0xE0,
        0x00,
        0xF0,
        0x00,
        // ES entry (no descriptors)
        0x06,
        0xE0 | ((caption_pid >> 8) as u8 & 0x1F),
        (caption_pid & 0xFF) as u8,
        0xF0, // ES_info_len high
        0x00, // ES_info_len low = 0
    ];
    // CRC
    s.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    s
}

/// Build a minimal PES packet header with PTS for the ARIB caption stream.
/// The payload starts with data_identifier=0x80, as expected by extract_pes_payload.
fn pes_header_with_pts(pts_90k: u64, payload: &[u8]) -> Vec<u8> {
    // Fixed PES header:
    //   [0-2]: start_code_prefix 00 00 01
    //   [3]:   stream_id (0xBD for private stream 1, typical for ARIB)
    //   [4-5]: packet_length (0 = unbounded; use actual for short test packets)
    //   [6]:   flags1 = 0x80
    //   [7]:   flags2 = 0x80 (PTS_DTS_flags=10 = PTS only)
    //   [8]:   optional_header_length = 5 (PTS only)
    //   [9-13]: PTS (5 bytes)
    //   [14+]:  payload
    let mut h = Vec::new();
    h.extend_from_slice(&[0x00, 0x00, 0x01, 0xBD]); // start code + stream_id
    let total_after_length = 3 + 5 + payload.len(); // flags(2) + opt_hdr_len(1) + PTS(5) + payload
    h.push((total_after_length >> 8) as u8);
    h.push((total_after_length & 0xFF) as u8);
    h.push(0x80); // flags1: data_alignment_indicator etc.
    h.push(0x80); // flags2: PTS_DTS_flags = 10 (PTS only)
    h.push(0x05); // optional_header_length = 5
    h.extend_from_slice(&encode_pts(pts_90k));
    h.extend_from_slice(payload);
    h
}

/// Write a sequence of 188-byte TS packets to a tempfile and return the path.
fn write_ts_file(packets: &[[u8; 188]]) -> (tempfile::NamedTempFile, std::path::PathBuf) {
    let mut f = tempfile::NamedTempFile::new().expect("tempfile");
    for pkt in packets {
        f.write_all(pkt).unwrap();
    }
    f.flush().unwrap();
    let path = f.path().to_path_buf();
    (f, path)
}

// ── scan_psi tests ────────────────────────────────────────────────────────────

#[test]
fn scan_psi_nonexistent_file_returns_empty() {
    let psi = scan_psi(std::path::Path::new("/nonexistent/file.ts"));
    assert!(psi.caption_services.is_empty());
    assert!(psi.caption_pid.is_none());
}

#[test]
fn scan_psi_empty_file_returns_empty() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let psi = scan_psi(f.path());
    assert!(psi.caption_services.is_empty());
    assert!(psi.caption_pid.is_none());
}

#[test]
fn scan_psi_garbage_bytes_returns_empty() {
    // Random bytes without 0x47 sync → no valid packets.
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(&vec![0xAAu8; 188 * 10]).unwrap();
    f.flush().unwrap();
    let psi = scan_psi(f.path());
    assert!(psi.caption_pid.is_none());
}

#[test]
fn scan_psi_pat_only_no_pmt_returns_no_pid() {
    let svc_id: u16 = 1;
    let pmt_pid: u16 = 0x0100;
    let caption_pid: u16 = 0x0200;

    let pat = section_packet(0x0000, &pat_section(svc_id, pmt_pid));

    // No PMT packet → caption_pid stays None.
    // Add a packet on a different (non-PMT) PID to fill the stream.
    let filler = pes_packet(0x01FF, false, &[0xFFu8; 184]);

    let (_f, path) = write_ts_file(&[pat, filler]);
    let psi = scan_psi(&path);

    // PAT was read, PMT PID registered, but no PMT arrived.
    assert!(psi.caption_pid.is_none());
    // caption_services may or may not include the service depending on fallback.
    let _ = caption_pid; // suppress unused warning
}

#[test]
fn scan_psi_finds_caption_pid_with_arib_descriptor() {
    let svc_id: u16 = 0x0001;
    let pmt_pid: u16 = 0x0100;
    let caption_pid: u16 = 0x0200;

    let pat = section_packet(0x0000, &pat_section(svc_id, pmt_pid));
    let pmt = section_packet(pmt_pid, &pmt_section_with_arib(svc_id, caption_pid));

    let (_f, path) = write_ts_file(&[pat, pmt]);
    let psi = scan_psi(&path);

    assert_eq!(
        psi.caption_pid,
        Some(caption_pid),
        "should identify caption PID from ARIB descriptor"
    );
    assert!(
        psi.caption_services.contains(&svc_id),
        "service should be listed as a caption service"
    );
}

#[test]
fn scan_psi_fallback_to_stream_type_06_without_arib_descriptor() {
    let svc_id: u16 = 0x0001;
    let pmt_pid: u16 = 0x0100;
    let caption_pid: u16 = 0x0200;

    let pat = section_packet(0x0000, &pat_section(svc_id, pmt_pid));
    let pmt = section_packet(pmt_pid, &pmt_section_no_arib(svc_id, caption_pid));

    let (_f, path) = write_ts_file(&[pat, pmt]);
    let psi = scan_psi(&path);

    // Fallback: any 0x06 stream is used when no ARIB descriptor is found.
    assert_eq!(
        psi.caption_pid,
        Some(caption_pid),
        "fallback should pick up the stream_type=0x06 PID"
    );
}

// ── find_caption_pid tests ────────────────────────────────────────────────────

#[test]
fn find_caption_pid_nonexistent_file_returns_none() {
    assert!(find_caption_pid(std::path::Path::new("/nonexistent/x.ts")).is_none());
}

#[test]
fn find_caption_pid_returns_pid_from_pmt() {
    let svc_id: u16 = 0x0001;
    let pmt_pid: u16 = 0x0100;
    let caption_pid: u16 = 0x0300;

    let pat = section_packet(0x0000, &pat_section(svc_id, pmt_pid));
    let pmt = section_packet(pmt_pid, &pmt_section_with_arib(svc_id, caption_pid));

    let (_f, path) = write_ts_file(&[pat, pmt]);
    assert_eq!(find_caption_pid(&path), Some(caption_pid));
}

#[test]
fn find_caption_pid_empty_stream_returns_none() {
    let f = tempfile::NamedTempFile::new().unwrap();
    assert!(find_caption_pid(f.path()).is_none());
}

// ── demux_caption_pes tests ───────────────────────────────────────────────────

#[test]
fn demux_caption_pes_nonexistent_file_returns_empty() {
    let result = demux_caption_pes(std::path::Path::new("/nonexistent/x.ts"), 0x0200);
    assert!(result.is_empty());
}

#[test]
fn demux_caption_pes_empty_file_returns_empty() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let result = demux_caption_pes(f.path(), 0x0200);
    assert!(result.is_empty());
}

#[test]
fn demux_caption_pes_wrong_pid_returns_empty() {
    let caption_pid: u16 = 0x0200;
    let wrong_pid: u16 = 0x0201;

    let pts = 90_000u64; // 1 second at 90 kHz
    let payload = [0x80u8, 0x01, 0x02, 0x03]; // data_identifier 0x80 = ARIB
    let pes_data = pes_header_with_pts(pts, &payload);
    let pkt = pes_packet(caption_pid, true, &pes_data);

    let (_f, path) = write_ts_file(&[pkt]);
    let result = demux_caption_pes(&path, wrong_pid);
    assert!(result.is_empty(), "wrong PID should yield no results");
}

#[test]
fn demux_caption_pes_single_packet_within_one_ts_packet() {
    let caption_pid: u16 = 0x0200;
    let pts_90k: u64 = 90_000; // 1 second
    let payload = [0x80u8, 0xAA, 0xBB]; // minimal ARIB payload

    let pes_data = pes_header_with_pts(pts_90k, &payload);
    // One PES packet with PUSI=1, then one more PUSI to flush it.
    let pkt1 = pes_packet(caption_pid, true, &pes_data);

    // Second PES packet (different PTS) triggers flush of first.
    let pts2 = 180_000u64;
    let payload2 = [0x80u8, 0xCC];
    let pes_data2 = pes_header_with_pts(pts2, &payload2);
    let pkt2 = pes_packet(caption_pid, true, &pes_data2);

    let (_f, path) = write_ts_file(&[pkt1, pkt2]);
    let result = demux_caption_pes(&path, caption_pid);

    // First PES should be flushed by the second PUSI.
    assert!(!result.is_empty(), "should get at least one PES unit");
    // First PTS was the epoch (0 ms), second = (180000-90000)/90 = 1000 ms.
    assert_eq!(result[0].pts_ms, 0, "first PTS should be normalized to 0");
    assert_eq!(
        result[0].payload,
        payload.to_vec(),
        "payload should match what was sent"
    );
}

#[test]
fn demux_caption_pes_pts_normalized_to_first_seen() {
    let caption_pid: u16 = 0x0200;

    // First PES: PTS = 2 seconds → epoch
    let pts_epoch = 90_000u64 * 2;
    let p1 = [0x80u8, 0x01];
    let pes1 = pes_header_with_pts(pts_epoch, &p1);
    let pkt1 = pes_packet(caption_pid, true, &pes1);

    // Second PES: PTS = epoch + 1 second
    let pts_second = pts_epoch + 90_000;
    let p2 = [0x80u8, 0x02];
    let pes2 = pes_header_with_pts(pts_second, &p2);
    let pkt2 = pes_packet(caption_pid, true, &pes2);

    // Third PES: PTS = epoch + 3 seconds (triggers flush of second)
    let pts_third = pts_epoch + 270_000;
    let p3 = [0x80u8, 0x03];
    let pes3 = pes_header_with_pts(pts_third, &p3);
    let pkt3 = pes_packet(caption_pid, true, &pes3);

    let (_f, path) = write_ts_file(&[pkt1, pkt2, pkt3]);
    let result = demux_caption_pes(&path, caption_pid);

    assert!(result.len() >= 2, "should have at least 2 flushed units");
    assert_eq!(result[0].pts_ms, 0, "epoch PTS normalizes to 0 ms");
    // Second PTS: (epoch + 90000 - epoch) / 90 = 1000 ms.
    assert_eq!(result[1].pts_ms, 1000, "second PTS should be 1000 ms");
}

#[test]
fn demux_caption_pes_skips_packet_without_pts() {
    // A PES packet with pts_dts_flags=00 (no PTS) should be silently skipped.
    let caption_pid: u16 = 0x0200;

    // Build a PES with no PTS (flags2 = 0x00).
    let payload = [0x80u8, 0x01, 0x02];
    let mut pes = vec![0x00u8, 0x00, 0x01, 0xBD]; // start code
    let pes_len = 3 + payload.len(); // flags(2) + opt_len(1) + payload
    pes.push((pes_len >> 8) as u8);
    pes.push((pes_len & 0xFF) as u8);
    pes.push(0x80); // flags1
    pes.push(0x00); // flags2: no PTS
    pes.push(0x00); // optional_header_length = 0
    pes.extend_from_slice(&payload);

    let pkt = pes_packet(caption_pid, true, &pes);
    let (_f, path) = write_ts_file(&[pkt]);
    let result = demux_caption_pes(&path, caption_pid);
    // No second PUSI arrives to flush, and no PTS → should be empty.
    assert!(
        result.is_empty(),
        "packet without PTS should not produce output"
    );
}

#[test]
fn demux_caption_pes_adaptation_field_only_skipped() {
    // A TS packet with adaptation_field_control=10 (adaptation only, no payload)
    // must be silently skipped.
    let caption_pid: u16 = 0x0200;

    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47;
    pkt[1] = 0x40 | ((caption_pid >> 8) as u8 & 0x1F); // PUSI=1
    pkt[2] = (caption_pid & 0xFF) as u8;
    pkt[3] = 0x20; // adaptation_field_control=10 (no payload)

    let (_f, path) = write_ts_file(&[pkt]);
    let result = demux_caption_pes(&path, caption_pid);
    assert!(result.is_empty());
}

#[test]
fn demux_caption_pes_continuation_packet_extends_pes() {
    // A PES that spans two TS packets should be reassembled.
    let caption_pid: u16 = 0x0200;
    let pts_90k: u64 = 90_000;

    // Build a PES that is longer than 184 bytes (needs continuation).
    let long_payload = vec![0x80u8; 180]; // 180 bytes of ARIB payload
    let pes_data = pes_header_with_pts(pts_90k, &long_payload);
    assert!(
        pes_data.len() > 184,
        "PES must span multiple TS packets for this test"
    );

    // First packet: PUSI=1, first 184 bytes of PES.
    let pkt1 = pes_packet(caption_pid, true, &pes_data[..184.min(pes_data.len())]);
    // Continuation packet: PUSI=0, rest of PES.
    let pkt2 = pes_packet(caption_pid, false, &pes_data[184.min(pes_data.len())..]);
    // Flush packet: start of a new PES (no actual content needed).
    let flush_data = pes_header_with_pts(pts_90k + 90_000, &[0x80u8]);
    let pkt3 = pes_packet(caption_pid, true, &flush_data);

    let (_f, path) = write_ts_file(&[pkt1, pkt2, pkt3]);
    let result = demux_caption_pes(&path, caption_pid);

    assert!(!result.is_empty(), "continuation PES should be reassembled");
    // The reconstructed payload should match the original.
    assert_eq!(
        result[0].payload, long_payload,
        "multi-packet PES payload should be fully reassembled"
    );
}

// ── CaptionPes blob round-trip (coverage of write_pes_blob/read_pes_blob via
//    the existing unit tests in src/ts/pes.rs — these just confirm the
//    module-level pub API is callable from integration tests too)

#[test]
fn pes_blob_write_read_integration() {
    use captu::ts::pes::{read_pes_blob, write_pes_blob};

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("captions.pes");

    let items = vec![
        CaptionPes {
            pts_ms: 0,
            payload: vec![0x80, 0xAA],
        },
        CaptionPes {
            pts_ms: 2000,
            payload: vec![0x80, 0xBB, 0xCC],
        },
    ];

    write_pes_blob(&path, &items).expect("write");
    let loaded = read_pes_blob(&path).expect("read");

    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].pts_ms, 0);
    assert_eq!(loaded[1].pts_ms, 2000);
    assert_eq!(loaded[1].payload, vec![0x80u8, 0xBB, 0xCC]);
}
