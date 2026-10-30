use umac_h264::{Error, Parser, access_units};

fn check_stream(data: &[u8], frames: usize, idrs: usize, slices: usize, size: (u32, u32)) {
    let aus = access_units(data);
    assert_eq!(aus.len(), frames);
    let mut p = Parser::new();
    let mut seen_idr = 0;
    let mut last_poc = i32::MIN;
    for (i, au) in aus.iter().enumerate() {
        let job = p.push(au).unwrap_or_else(|e| panic!("frame {i}: {e}"));
        assert_eq!(job.slice_offsets.len(), slices, "frame {i}");
        assert_eq!(job.sps.display_size(), size);
        if job.idr {
            seen_idr += 1;
            assert!(job.references.is_empty() && job.intra);
            last_poc = i32::MIN;
        }
        assert!(job.poc[0] > last_poc, "frame {i}: POC must increase without B frames");
        last_poc = job.poc[0];
        assert!(job.references.len() <= job.sps.max_num_ref_frames as usize);
        assert!(!job.references.iter().any(|r| r.slot == job.slot), "frame {i}: setup slot is in use");
        assert!(job.slot < job.slots);
        assert_eq!(&job.bitstream[job.slice_offsets[0] as usize..][..3], &[0, 0, 1]);
    }
    assert_eq!(seen_idr, idrs);
}

#[test]
fn x264_high_profile_two_slices() {
    let data = include_bytes!("../testdata/x264_320x180_high.h264");
    check_stream(data, 30, 2, 2, (320, 180));
}

#[test]
fn videotoolbox_low_latency() {
    let Ok(data) = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/videotoolbox_1080p.h264")) else {
        eprintln!("no VideoToolbox sample, skipping");
        return;
    };
    let aus = access_units(&data);
    let mut p = Parser::new();
    for (i, au) in aus.iter().enumerate() {
        let job = p.push(au).unwrap_or_else(|e| panic!("frame {i}: {e}"));
        assert_eq!(job.sps.display_size(), (1920, 1080));
        if i == 0 {
            assert!(job.idr);
        }
        // VideoToolbox keeps its first frames as long-term references for loss recovery.
        if i == 3 {
            assert_eq!(job.references.iter().filter(|r| r.long_term).count(), 3);
        }
        assert!(!job.references.iter().any(|r| r.slot == job.slot));
    }
}

#[test]
fn needs_an_idr_first_and_after_errors() {
    let aus = access_units(include_bytes!("../testdata/x264_320x180_high.h264"));
    let mut p = Parser::new();
    p.push(&aus[0]).unwrap();
    // Dropping a reference picture is detected through the frame_num gap.
    assert!(matches!(p.push(&aus[2]), Err(Error::Invalid(_))));
    // After an error the parser waits for the next IDR, even with intact P frames.
    assert_eq!(p.push(&aus[3]).unwrap_err(), Error::NeedKeyframe);
    assert!(p.push(&aus[15]).unwrap().idr);
    assert!(!p.push(&aus[16]).unwrap().idr);
}

#[test]
fn random_and_mutated_input_never_panics() {
    let aus = access_units(include_bytes!("../testdata/x264_320x180_high.h264"));
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..5000 {
        let mut p = Parser::new();
        let mut au = aus[(next() as usize) % aus.len()].clone();
        if round % 3 == 0 {
            au = (0..(next() % 400)).map(|_| next() as u8).collect();
            au.splice(0..0, [0, 0, 1, if round % 2 == 0 { 0x67 } else { 0x65 }]);
        } else {
            for _ in 0..1 + next() % 8 {
                let i = (next() as usize) % au.len();
                au[i] ^= 1 << (next() % 8);
            }
        }
        let _ = p.push(&aus[0]);
        let _ = p.push(&au);
    }
}
