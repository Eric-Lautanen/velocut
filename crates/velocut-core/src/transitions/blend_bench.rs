// Measurement benches for the CPU transition blend — not part of the suite.
//
// Everything here is `#[ignore]`d, so it never runs in CI and cannot fail on a
// slow machine; run one explicitly with:
//
//   cargo test -p velocut-core --release -- --ignored --nocapture blend_cost
//
// Kept out of `transitions/mod.rs` because it is measurement scaffolding, not
// registry code, and it is long enough to drown the module it measures.

use super::helpers::{blend_byte_fixed, fixed_inverse, fixed_weight};
use super::*;
use std::time::Instant;

fn timed(kind: TransitionKind, w: u32, h: u32, frames: usize) -> f64 {
    let registry = registry();
    let t = registry.get(&kind).expect("registered transition");
    let len = (w * h * 4) as usize;
    // Non-uniform content so the blend maths does real work, not zeros.
    let a: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    let b: Vec<u8> = (0..len).map(|i| ((i * 7) % 253) as u8).collect();

    // One warm-up pass so allocator + rayon pool setup is not measured.
    let _ = t.apply_rgba(&a, &b, w, h, 0.5);

    let start = Instant::now();
    for i in 0..frames {
        let alpha = (i as f32 + 1.0) / (frames as f32 + 1.0);
        let out = t.apply_rgba(&a, &b, w, h, alpha);
        std::hint::black_box(&out);
    }
    start.elapsed().as_secs_f64() * 1000.0 / frames as f64
}

/// The dissolve's RGBA blend as it was written before the fixed-point
/// rewrite: one rayon task per pixel and a float blend per channel. Kept
/// here, in the measurement module, only so the speed-up can be measured
/// against the real thing instead of against a memory of it.
fn crossfade_reference_rgba(frame_a: &[u8], frame_b: &[u8], alpha: f32) -> Vec<u8> {
    use rayon::prelude::*;
    let eased = crate::transitions::helpers::ease_in_out(alpha);
    let mut out = vec![0u8; frame_a.len()];
    out.par_chunks_mut(4)
        .zip(frame_a.par_chunks(4))
        .zip(frame_b.par_chunks(4))
        .for_each(|((o, a), b)| {
            for c in 0..4 {
                o[c] = crate::transitions::helpers::blend_byte(a[c], b[c], eased);
            }
        });
    out
}

#[test]
#[ignore = "measurement only — run explicitly with --ignored --nocapture"]
fn blend_speedup_is_real_and_measured_in_process() {
    let w = 1504u32;
    let h = 832u32;
    let len = (w * h * 4) as usize;
    let a: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    let b: Vec<u8> = (0..len).map(|i| ((i * 7) % 253) as u8).collect();
    let t = registry()
        .get(&TransitionKind::Crossfade)
        .expect("crossfade registered");

    let run = |shipped: bool| -> f64 {
        // Warm-up, then 15 frames at walking alphas, as playback produces them.
        let _ = if shipped {
            t.apply_rgba(&a, &b, w, h, 0.5)
        } else {
            crossfade_reference_rgba(&a, &b, 0.5)
        };
        let start = Instant::now();
        for i in 0..15 {
            let alpha = (i as f32 + 1.0) / 16.0;
            let out = if shipped {
                t.apply_rgba(&a, &b, w, h, alpha)
            } else {
                crossfade_reference_rgba(&a, &b, alpha)
            };
            std::hint::black_box(&out);
        }
        start.elapsed().as_secs_f64() * 1000.0 / 15.0
    };

    // Interleave the two implementations round by round so a load spike hits both
    // equally, then take the median of the per-round RATIOS — a median of the
    // absolute times can pair a lucky old sample with an unlucky new one. The
    // min/max spread is printed too: it is the honest error bar on any claim
    // taken from this bench.
    let mut ratios = Vec::new();
    let mut new_ms = Vec::new();
    let mut old_ms = Vec::new();
    for _ in 0..7 {
        let o = run(false);
        let n = run(true);
        old_ms.push(o);
        new_ms.push(n);
        ratios.push(o / n);
    }
    ratios.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let med = |xs: &mut Vec<f64>| {
        xs.sort_by(|x, y| x.partial_cmp(y).unwrap());
        xs[xs.len() / 2]
    };
    let (old, new) = (med(&mut old_ms), med(&mut new_ms));

    println!("\nCrossfade apply_rgba at 1504x832, ms/frame (median of 7 interleaved rounds):");
    println!("  float, one task per pixel   {old:.2} ms");
    println!("  fixed-point, 1024-px blocks {new:.2} ms");
    println!(
        "  speed-up {:.2}x (per-round range {:.2}x-{:.2}x)\n",
        ratios[ratios.len() / 2],
        ratios[0],
        ratios[ratios.len() - 1]
    );

    // A dissolve at preview size inside a 60 fps budget is the point of the
    // rewrite, so guard the measurement itself against measuring nothing.
    assert!(old > 0.0 && new > 0.0, "both paths must be timed");
}

/// Where the remaining time goes: a pure `memcpy` of one frame, the same
/// fixed-point blend on one thread, and the shipped rayon version — all into
/// a preallocated buffer, so allocation is excluded from the first two.
///
/// This exists to answer "is the blend compute-bound or bandwidth-bound?"
/// before anyone reaches for SIMD or a GPU: if the single-thread blend already
/// sits near the copy cost, more threads cannot buy much.
#[test]
#[ignore = "measurement only — run explicitly with --ignored --nocapture"]
fn blend_is_bandwidth_or_compute_bound() {
    use rayon::prelude::*;
    let (w, h) = (1504u32, 832u32);
    let len = (w * h * 4) as usize;
    let a: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    let b: Vec<u8> = (0..len).map(|i| ((i * 7) % 253) as u8).collect();
    let mut out = vec![0u8; len];
    let (wgt, inv) = (fixed_weight(0.5), fixed_inverse(fixed_weight(0.5)));

    // Best-of, not median: other agents compile on this machine, so the
    // typical sample is inflated by whatever else is running. The fastest of
    // several rounds is the closest thing to a controlled measurement, and it
    // still favours neither candidate.
    fn best(mut f: impl FnMut()) -> f64 {
        f();
        (0..7)
            .map(|_| {
                let t = Instant::now();
                f();
                t.elapsed().as_secs_f64() * 1000.0
            })
            .fold(f64::INFINITY, f64::min)
    }

    let alloc_ms = best(|| {
        // What `apply_rgba` pays before it blends a single byte.
        let v = vec![0u8; len];
        std::hint::black_box(&v);
    });
    let copy_ms = best(|| {
        out.copy_from_slice(&a);
        std::hint::black_box(&out);
    });
    let one_thread = best(|| {
        for ((o, x), y) in out.iter_mut().zip(&a).zip(&b) {
            *o = blend_byte_fixed(*x, *y, inv, wgt);
        }
        std::hint::black_box(&out);
    });
    let rayon_ms = best(|| {
        out.par_chunks_mut(4096)
            .zip(a.par_chunks(4096))
            .zip(b.par_chunks(4096))
            .for_each(|((o, x), y)| {
                for ((oc, xc), yc) in o.iter_mut().zip(x).zip(y) {
                    *oc = blend_byte_fixed(*xc, *yc, inv, wgt);
                }
            });
        std::hint::black_box(&out);
    });

    let fresh_ms = best(|| {
        // A fresh output buffer per frame, as every `apply_rgba` does: the
        // allocation itself is cheap (calloc hands back lazy zero pages), but
        // writing 5 MB into brand-new pages faults them in one 4 KiB page at a
        // time. This line is the difference between the two.
        let mut v = vec![0u8; len];
        for ((o, x), y) in v.iter_mut().zip(&a).zip(&b) {
            *o = blend_byte_fixed(*x, *y, inv, wgt);
        }
        std::hint::black_box(&v);
    });

    println!("\nBlend floor at 1504x832 ({len} bytes), ms/frame:");
    println!("  zeroed Vec allocation  {alloc_ms:.2} ms");
    println!("  memcpy one frame       {copy_ms:.2} ms");
    println!("  1 thread, no alloc     {one_thread:.2} ms");
    println!("  1 thread, fresh buffer {fresh_ms:.2} ms");
    println!("  rayon, no alloc        {rayon_ms:.2} ms");
    println!(
        "  rayon threads          {}\n",
        rayon::current_num_threads()
    );
}

#[test]
#[ignore = "measurement only — run explicitly with --ignored --nocapture"]
fn blend_cost_across_resolutions() {
    let cases = [
        ("preview 1504x832", 1504, 832),
        ("1080p", 1920, 1080),
        ("1440p", 2560, 1440),
        ("4K", 3840, 2160),
    ];
    let kinds = [
        TransitionKind::Crossfade,
        TransitionKind::Iris,
        TransitionKind::ClockWipe,
    ];

    println!("\nCPU blend cost, ms/frame (rayon over rows, release build):");
    for (label, w, h) in cases {
        let mut line = format!("  {label:<18}");
        for kind in kinds {
            let ms = timed(kind, w, h, 12);
            line.push_str(&format!(" {kind:?}={ms:.1}ms"));
        }
        println!("{line}");
    }
    println!("  budget: 33.3 ms/frame at 30 fps, 16.7 ms/frame at 60 fps\n");
}

// ── Page-fault cost of the per-frame allocation ─────────────────────────────��

/// Minor page faults this process has taken so far, or `None` where the platform
/// makes it awkward. On Windows the counter comes from `K32GetProcessMemoryInfo`
/// in kernel32 (Vista+), which needs no crate; on Linux it is the `minflt` field
/// of `/proc/self/stat`.
fn page_faults() -> Option<u64> {
    #[cfg(windows)]
    {
        /// `PROCESS_MEMORY_COUNTERS`. Field names are snake_case rather than the
        /// SDK's CamelCase, but the DECLARATION ORDER is the ABI: `repr(C)` lays
        /// the fields out in this order, so it must stay identical to winnt.h.
        #[repr(C)]
        struct ProcessMemoryCounters {
            cb: u32,
            page_fault_count: u32,
            peak_working_set_size: usize,
            working_set_size: usize,
            quota_peak_paged_pool_usage: usize,
            quota_paged_pool_usage: usize,
            quota_peak_non_paged_pool_usage: usize,
            quota_non_paged_pool_usage: usize,
            pagefile_usage: usize,
            peak_pagefile_usage: usize,
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentProcess() -> *mut core::ffi::c_void;
            fn K32GetProcessMemoryInfo(
                process: *mut core::ffi::c_void,
                counters: *mut ProcessMemoryCounters,
                cb: u32,
            ) -> i32;
        }

        // SAFETY: the struct is POD, `cb` is set to its own size as the API
        // requires, and the handle is the documented pseudo-handle for this
        // process. A zero return means the call failed and the count is not read.
        unsafe {
            let mut pmc: ProcessMemoryCounters = core::mem::zeroed();
            pmc.cb = core::mem::size_of::<ProcessMemoryCounters>() as u32;
            let ok = K32GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb);
            if ok == 0 {
                return None;
            }
            Some(pmc.page_fault_count as u64)
        }
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        // Field 10 (1-based) is minflt; the comm field can contain spaces, so
        // split after the closing paren rather than on the whole line.
        let tail = &stat[stat.rfind(')')? + 1..];
        let minflt = tail.split_whitespace().nth(7)?;
        minflt.parse().ok()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        None
    }
}

/// What the per-frame allocation actually costs, in the currency the fix claims
/// to save: page faults.
///
/// `apply_rgba` hands back a fresh `Vec` for every frame; at 1504×832 that is a
/// 5 MB allocation, which the allocator satisfies with `mmap` and returns to the
/// OS on every drop. Writing the blend therefore faults in ~1200 zero pages each
/// frame, and the playback loop paid that twice — once for the blend result and
/// once for the copy it handed to the channel. Blending into a buffer the caller
/// keeps removes the first pass. Run this alone (`--test-threads=1`): the counter
/// is process-wide, so a concurrent test allocating memory would be attributed to
/// whichever path is running.
///
/// Timings are best-of-rounds because this machine is shared; fault counts are
/// totals and are stable run to run.
#[test]
#[ignore = "measurement only — run explicitly with --ignored --nocapture --test-threads=1"]
fn reuse_removes_the_per_frame_page_faults() {
    if page_faults().is_none() {
        println!(
            "
page faults unavailable on this platform — nothing measured
"
        );
        return;
    }
    const FRAMES: usize = 30;
    const ROUNDS: usize = 3;

    /// Best per-frame time and total faults across `rounds` passes of `frames`.
    fn measure(mut f: impl FnMut(f32), frames: usize, rounds: usize) -> (f64, u64) {
        let faults_before = page_faults().expect("checked by caller");
        let mut best = f64::INFINITY;
        for _ in 0..rounds {
            let start = Instant::now();
            for i in 0..frames {
                f((i as f32 + 1.0) / (frames as f32 + 1.0));
            }
            best = best.min(start.elapsed().as_secs_f64() * 1000.0 / frames as f64);
        }
        let faults = page_faults().expect("checked by caller") - faults_before;
        (best, faults)
    }

    let (w, h) = (1504u32, 832u32);
    let len = (w * h * 4) as usize;
    let a: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    let b: Vec<u8> = (0..len).map(|i| ((i * 7) % 253) as u8).collect();
    let t = registry()
        .get(&TransitionKind::Crossfade)
        .expect("crossfade registered");
    let mut scratch: Vec<u8> = Vec::new();

    // Warm-ups: allocator, rayon pool, and the scratch's first allocation.
    let _ = t.apply_rgba(&a, &b, w, h, 0.5);
    t.apply_rgba_into(&a, &b, w, h, 0.5, &mut scratch);

    let (fresh_ms, fresh_faults) = measure(
        |alpha| {
            let out = t.apply_rgba(&a, &b, w, h, alpha);
            std::hint::black_box(&out);
        },
        FRAMES,
        ROUNDS,
    );
    let (reuse_ms, reuse_faults) = measure(
        |alpha| {
            t.apply_rgba_into(&a, &b, w, h, alpha, &mut scratch);
            std::hint::black_box(&scratch);
        },
        FRAMES,
        ROUNDS,
    );
    let (after_ms, after_faults) = measure(
        |alpha| {
            t.apply_rgba_into(&a, &b, w, h, alpha, &mut scratch);
            let sent = scratch.clone();
            std::hint::black_box(&sent);
        },
        FRAMES,
        ROUNDS,
    );
    let (before_ms, before_faults) = measure(
        |alpha| {
            let blended = t.apply_rgba(&a, &b, w, h, alpha);
            let sent = blended.clone();
            std::hint::black_box(&sent);
        },
        FRAMES,
        ROUNDS,
    );

    let passes = (FRAMES * ROUNDS) as f64;
    let per_frame = |n: u64| n as f64 / passes;
    println!(
        "
{FRAMES} dissolve frames × {ROUNDS} rounds at 1504×832 (5.0 MB per frame):"
    );
    println!(
        "  blend, fresh Vec per frame      {:>7.0} faults/frame  {:>6.2} ms/frame",
        per_frame(fresh_faults),
        fresh_ms
    );
    println!(
        "  blend, reused buffer            {:>7.0} faults/frame  {:>6.2} ms/frame",
        per_frame(reuse_faults),
        reuse_ms
    );
    println!(
        "  playback before (blend + clone) {:>7.0} faults/frame  {:>6.2} ms/frame",
        per_frame(before_faults),
        before_ms
    );
    println!(
        "  playback after  (blend + clone) {:>7.0} faults/frame  {:>6.2} ms/frame",
        per_frame(after_faults),
        after_ms
    );
    println!(
        "  saved by the reuse              {:>7.0} faults/frame
",
        per_frame(before_faults) - per_frame(after_faults)
    );

    // Structural, not a speed threshold: the reuse must halve the fault traffic of
    // the playback loop. A frame is ~1200 pages, so harness noise cannot flip this.
    assert!(
        after_faults * 2 < before_faults,
        "expected the reused buffer to halve the page faults of the playback loop          (before {before_faults}, after {after_faults}) — is the scratch being reallocated?"
    );
}

/// The full round trip: playback copies the frame into a buffer, the UI thread
/// receives it, does its upload work and drops it, and the buffer comes back.
///
/// The consumer here sleeps ~2 ms per frame to stand in for the texture upload, so
/// the producer runs a few frames ahead and sometimes finds the pool empty — which
/// is what actually happens. That makes the number a range rather than a best case:
/// with returns keeping up, the copy costs nothing; when they lag, it allocates.
#[test]
#[ignore = "measurement only — run explicitly with --ignored --nocapture --test-threads=1"]
fn recycling_the_channel_copy_stops_it_faulting() {
    use std::sync::mpsc::sync_channel;
    use std::thread;

    if page_faults().is_none() {
        println!("\npage faults unavailable on this platform — nothing measured\n");
        return;
    }
    const FRAMES: usize = 60;
    const POOL_MAX: usize = 6;
    let (w, h) = (1504u32, 832u32);
    let len = (w * h * 4) as usize;
    let held: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();

    // Consumer: stands in for the UI thread. Receives a frame, "uploads" it, and
    // hands the buffer back the way `FrameBuf::drop` does.
    fn run_consumer(
        frame_rx: std::sync::mpsc::Receiver<Vec<u8>>,
        return_tx: std::sync::mpsc::SyncSender<Vec<u8>>,
        work: std::time::Duration,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            while let Ok(buf) = frame_rx.recv() {
                std::hint::black_box(&buf);
                thread::sleep(work);
                if return_tx.try_send(buf).is_err() {
                    // Nothing collecting: drop it, exactly like a full pool.
                }
            }
        })
    }

    // ── Before: fresh allocation per sent frame, consumer drops it ────────────
    let (frame_tx, frame_rx) = sync_channel::<Vec<u8>>(6);
    // No collector in the "before" run: the returned buffer is simply dropped.
    let (return_tx, _return_rx) = sync_channel::<Vec<u8>>(8);
    let consumer = run_consumer(frame_rx, return_tx, std::time::Duration::from_millis(2));
    let faults_before = page_faults().expect("checked above");
    let t0 = Instant::now();
    for _ in 0..FRAMES {
        let sent = held.clone();
        frame_tx.send(sent).expect("consumer alive");
    }
    let before_ms = t0.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;
    let before_faults = page_faults().expect("checked above") - faults_before;
    drop(frame_tx);
    consumer.join().expect("consumer");

    // ── After: pool fed by the returned buffers ──────────────────────────────
    let (frame_tx, frame_rx) = sync_channel::<Vec<u8>>(6);
    let (return_tx, return_rx) = sync_channel::<Vec<u8>>(8);
    let consumer = run_consumer(frame_rx, return_tx, std::time::Duration::from_millis(2));
    let mut pool: Vec<Vec<u8>> = Vec::new();
    let faults_after = page_faults().expect("checked above");
    let t1 = Instant::now();
    for _ in 0..FRAMES {
        while pool.len() < POOL_MAX {
            match return_rx.try_recv() {
                Ok(buf) => pool.push(buf),
                Err(_) => break,
            }
        }
        let mut buf = pool.pop().unwrap_or_default();
        buf.clear();
        buf.extend_from_slice(&held);
        frame_tx.send(buf).expect("consumer alive");
    }
    let after_ms = t1.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;
    let after_faults = page_faults().expect("checked above") - faults_after;
    drop(frame_tx);
    consumer.join().expect("consumer");

    let per_frame = |n: u64| n as f64 / FRAMES as f64;
    println!("\n{FRAMES} sent frames at 1504×832, UI stand-in returns buffers after 2 ms of work:");
    println!(
        "  fresh Vec per frame   {:>7.0} faults/frame  {:>6.2} ms/frame",
        per_frame(before_faults),
        before_ms
    );
    println!(
        "  recycled buffers      {:>7.0} faults/frame  {:>6.2} ms/frame",
        per_frame(after_faults),
        after_ms
    );
    println!(
        "  remaining             {:.0} faults/frame are copies made while the pool was empty\n",
        per_frame(after_faults)
    );

    assert!(
        after_faults < before_faults,
        "recycling must fault less than allocating (before {before_faults}, after {after_faults})"
    );
}
