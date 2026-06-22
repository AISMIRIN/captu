use std::path::{Path, PathBuf};

fn format_ms(ms: i64) -> String {
    let h = ms / 3_600_000;
    let m = (ms % 3_600_000) / 60_000;
    let s = (ms % 60_000) / 1_000;
    let msec = ms % 1_000;  // milliseconds component (0-999), not centiseconds
    format!("{:02}:{:02}:{:02}.{:03}", h, m, s, msec)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: extract <ts_path> [--cache-dir <dir>] [--debug-raw]");
        std::process::exit(1);
    }

    let ts_path = Path::new(&args[1]);
    let cache_dir: PathBuf = args.windows(2)
        .find(|w| w[0] == "--cache-dir")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| PathBuf::from("/tmp/captu_extract"));
    let debug_raw = args.iter().any(|a| a == "--debug-raw");

    // Single PAT/PMT pass — reused by both EPG and caption extraction below.
    let psi = captu::ts::pes::scan_psi(ts_path);

    // EPG
    println!("=== EPG ===");
    match captu::ts::epg::extract_epg(ts_path, &psi.caption_services) {
        Ok(epg) => {
            println!("title:          {}", epg.title);
            println!("series_title:   {}", epg.series_title);
            match epg.episode_number {
                Some(ep) => println!("episode:        {}", ep),
                None     => println!("episode:        (none)"),
            }
            match &epg.sub_title {
                Some(s) => println!("sub_title:      {}", s),
                None    => println!("sub_title:      (none)"),
            }
            match epg.air_datetime {
                Some(dt) => println!("air_date:       {}", dt),
                None      => println!("air_date:       (none)"),
            }
        }
        Err(e) => eprintln!("EPG error: {:#}", e),
    }

    // Raw decoder debug: show every decoded caption event directly
    if debug_raw {
        println!();
        println!("=== Raw decoder output ===");
        let caption_pid = captu::ts::pes::find_caption_pid(ts_path);
        println!("caption PID: {:?}", caption_pid.map(|p| format!("0x{:04X}", p)));

        if let Some(pid) = caption_pid {
            let pes_list = captu::ts::pes::demux_caption_pes(ts_path, pid);
            println!("PES packets: {}", pes_list.len());

            let ctx = aribcaption_sys::Context::new().expect("context");
            let mut decoder = aribcaption_sys::Decoder::new(&ctx).expect("decoder");

            let mut total = 0usize;
            for pes in &pes_list {
                if let Some(cap) = decoder.decode(&pes.payload, pes.pts_ms) {
                    total += 1;
                    let text = cap.text();
                    let flags = cap.inner.flags;
                    let is_cs = cap.is_clear_screen();
                    let dur = cap.duration_ms();
                    if total <= 30 {
                        let preview: String = text.chars().take(40).collect();
                        println!(
                            "  #{:04}  pts={:8}ms  flags=0x{:02X}  clearscreen={}  dur={:?}  text={:?}",
                            total, cap.pts_ms(), flags, is_cs, dur, preview
                        );
                    }
                }
            }
            println!("total decoded: {}", total);
        }
        return;
    }

    // Normal caption extraction (text only, no rendering)
    println!();
    println!("=== Captions ===");
    match captu::ts::subtitle::extract_captions(ts_path, &cache_dir, psi.caption_pid) {
        Ok(captions) => {
            println!("({} 件)", captions.len());
            for cap in &captions {
                println!(
                    "[{} - {}] {}",
                    format_ms(cap.pts_start_ms),
                    format_ms(cap.pts_end_ms),
                    cap.text,
                );
            }
        }
        Err(e) => eprintln!("Caption error: {:#}", e),
    }
}

#[cfg(test)]
mod tests {
    use super::format_ms;

    #[test]
    fn format_ms_zero() {
        assert_eq!(format_ms(0), "00:00:00.000");
    }

    #[test]
    fn format_ms_milliseconds_width() {
        // 50 ms must produce ".050", not ".005" (centisecond bug)
        assert_eq!(format_ms(50), "00:00:00.050");
    }

    #[test]
    fn format_ms_one_second() {
        assert_eq!(format_ms(1_000), "00:00:01.000");
    }

    #[test]
    fn format_ms_mixed() {
        // 1h 2m 3s 456ms
        let ms = 1 * 3_600_000 + 2 * 60_000 + 3 * 1_000 + 456;
        assert_eq!(format_ms(ms), "01:02:03.456");
    }

    #[test]
    fn format_ms_999ms() {
        assert_eq!(format_ms(999), "00:00:00.999");
    }

    #[test]
    fn format_ms_large_hours() {
        // 100h exactly
        assert_eq!(format_ms(100 * 3_600_000), "100:00:00.000");
    }
}
