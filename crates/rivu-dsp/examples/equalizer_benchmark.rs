//! Reproducible CPU/heap microbenchmark; run with --release, never as a quality score.
use rivu_dsp::equalizer::{EqBand, Equalizer, EqualizerConfig, FilterKind};
use rivu_dsp::fir_eq::FirEqualizer;
use serde_json::json;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Instant,
};

struct Counted;
static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(size as u64, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, old, size) }
    }
}
#[global_allocator]
static ALLOCATOR: Counted = Counted;

fn percentile(sorted: &[u128], fraction: f64) -> u128 {
    sorted[((sorted.len() - 1) as f64 * fraction).ceil() as usize]
}

enum Processor {
    Iir(Equalizer),
    Fir(FirEqualizer),
}

impl Processor {
    fn process(&mut self, samples: &mut [f32]) -> anyhow::Result<()> {
        match self {
            Self::Iir(eq) => Ok(eq.process(samples)?),
            Self::Fir(eq) => eq.process_interleaved(samples),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Iir(eq) => eq.reset(),
            Self::Fir(eq) => eq.reset(),
        }
    }

    fn latency_frames(&self) -> usize {
        match self {
            Self::Iir(_) => 0,
            Self::Fir(eq) => eq.latency_frames(),
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let repetitions = 9;
    let mut results = Vec::new();
    for (rate, channels, bands, stages, enabled, taps) in [
        (48_000, 2, 0, 1, false, 0),
        (48_000, 2, 1, 1, true, 0),
        (48_000, 2, 10, 1, true, 0),
        (48_000, 2, 32, 1, true, 0),
        (48_000, 8, 32, 1, true, 0),
        (192_000, 8, 32, 4, true, 0),
        (768_000, 2, 32, 4, true, 0),
        (192_000, 32, 32, 4, true, 0),
        (768_000, 16, 32, 4, true, 0),
        (48_000, 2, 10, 1, true, 1025),
        (48_000, 2, 10, 1, true, 4097),
        (96_000, 2, 10, 1, true, 4097),
        (48_000, 8, 32, 1, true, 4097),
    ] {
        let block_frames = 256;
        let blocks = (rate / block_frames).max(128) as usize;
        let config = EqualizerConfig {
            enabled,
            preamp_db: -12.0,
            output_gain_db: 0.0,
            bands: (0..bands)
                .map(|index| EqBand {
                    kind: if stages > 1 {
                        FilterKind::Notch
                    } else {
                        FilterKind::Peak
                    },
                    frequency_hz: 30.0
                        * (16_000.0_f64 / 30.0).powf(index as f64 / bands.max(1) as f64),
                    gain_db: if index % 2 == 0 { 1.0 } else { -1.0 },
                    q: 1.0,
                    stages,
                    ..EqBand::default()
                })
                .collect(),
        };
        let prepared = Instant::now();
        let mut eq = if taps == 0 {
            Processor::Iir(Equalizer::new(&config, rate, channels)?)
        } else {
            Processor::Fir(FirEqualizer::new(&config, rate, channels, taps)?)
        };
        let preparation_ns = prepared.elapsed().as_nanos();
        let seed: Vec<f32> = (0..block_frames as usize * channels)
            .map(|index| {
                let frame = index / channels;
                let channel = index % channels;
                (0.02 * (frame as f64 * 0.17 + channel as f64 * 0.31).sin()) as f32
            })
            .collect();
        let mut block = seed.clone();
        for _ in 0..32 {
            block.copy_from_slice(&seed);
            eq.process(&mut block)?;
        }
        let mut elapsed = Vec::with_capacity(repetitions);
        let mut worst_block_ns = 0;
        let mut measured_allocations = 0;
        let mut measured_bytes = 0;
        let mut block_times = Vec::with_capacity(repetitions * blocks);
        for _ in 0..repetitions {
            eq.reset();
            let mut process_ns = 0;
            for _ in 0..blocks {
                block.copy_from_slice(&seed);
                ALLOCATIONS.store(0, Ordering::Relaxed);
                BYTES.store(0, Ordering::Relaxed);
                COUNTING.store(true, Ordering::Relaxed);
                let start = Instant::now();
                let processed = eq.process(black_box(&mut block));
                let ns = start.elapsed().as_nanos();
                COUNTING.store(false, Ordering::Relaxed);
                processed?;
                block_times.push(ns);
                black_box(&block);
                measured_allocations += ALLOCATIONS.load(Ordering::Relaxed);
                measured_bytes += BYTES.load(Ordering::Relaxed);
                process_ns += ns;
                worst_block_ns = worst_block_ns.max(ns);
            }
            elapsed.push(process_ns);
        }
        elapsed.sort_unstable();
        block_times.sort_unstable();
        let audio_seconds = blocks as f64 * block_frames as f64 / f64::from(rate);
        let median_ns = percentile(&elapsed, 0.5);
        results.push(json!({
            "sample_rate":rate,"channels":channels,"bands":bands,"stages":stages,
            "enabled":enabled,"block_frames":block_frames,"blocks_per_repeat":blocks,
            "implementation":if taps == 0 { "iir" } else { "linear_phase_fir" },
            "taps":taps,"latency_frames":eq.latency_frames(),
            "block_process_ns_p50":percentile(&block_times,0.5),
            "block_process_ns_p95":percentile(&block_times,0.95),
            "block_process_ns_p99":percentile(&block_times,0.99),
            "observed_blocks_over_deadline":block_times.iter().filter(|&&ns| ns as f64 > f64::from(block_frames) / f64::from(rate) * 1e9).count(),
            "repetitions":repetitions,"audio_seconds_per_repeat":audio_seconds,
            "preparation_ns_single_sample":preparation_ns,
            "process_ns_min":elapsed[0],"process_ns_median":median_ns,
            "process_ns_p95":percentile(&elapsed,0.95),"process_ns_max":elapsed[elapsed.len()-1],
            "worst_observed_block_ns":worst_block_ns,
            "median_realtime_factor":median_ns as f64 / 1e9 / audio_seconds,
            "process_allocations":measured_allocations,"process_allocated_bytes":measured_bytes,
            "repeat_process_ns":elapsed,
        }));
        if measured_allocations != 0 {
            return Err(
                "EQ processing allocated; inspect results before claiming real-time safety".into(),
            );
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema":1,"suite":"equalizer_cpu_heap","profile":"run with cargo --release",
            "measurement":"warm process calls only; excludes input copy, preparation, file I/O and device output; includes per-call clock overhead; not a callback deadline guarantee",
            "results":results
        }))?
    );
    Ok(())
}
