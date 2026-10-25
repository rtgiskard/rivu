//! Stream float WAV through the actual denoiser for paired quality evaluation.
use anyhow::{Context, Result, ensure};
use rivu_dsp::denoise::{DenoiseConfig, DenoiseMode, Denoiser, ProfileCollector};
use serde_json::json;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    fs::{self, OpenOptions},
    io::BufWriter,
    path::Path,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Instant,
};

// Same opt-in allocator accounting as equalizer_benchmark; WAV I/O is excluded.
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

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    ensure!(
        (4..=5).contains(&args.len()),
        "usage: denoise_render INPUT.float.wav OUTPUT.wav CONFIG.json [NOISE_PROFILE.float.wav]"
    );
    let config: DenoiseConfig = serde_json::from_slice(&fs::read(&args[3])?)?;
    config.validate()?;
    let mut reader = hound::WavReader::open(&args[1])?;
    let spec = reader.spec();
    ensure!(
        spec.sample_format == hound::SampleFormat::Float && spec.bits_per_sample == 32,
        "evaluation input must be float32 WAV; no implicit conversion"
    );
    let channels = usize::from(spec.channels);
    let expected_samples = reader.len() as u64;
    let mut never_cancel = || false;
    let profile = if config.mode == DenoiseMode::Profile {
        ensure!(
            args.len() == 5,
            "profile mode requires an explicit noise-only WAV"
        );
        let mut noise = hound::WavReader::open(&args[4])?;
        ensure!(
            noise.spec() == spec,
            "profile WAV format differs from input"
        );
        let max_frames = u64::from(noise.duration());
        let mut collector =
            ProfileCollector::new(spec.sample_rate, channels, config.fft_size, max_frames)?;
        let mut block = Vec::with_capacity(4096 * channels);
        for sample in noise.samples::<f32>() {
            block.push(sample?);
            if block.len() == 4096 * channels {
                collector.push_block(&block, &mut never_cancel)?;
                block.clear();
            }
        }
        if !block.is_empty() {
            collector.push_block(&block, &mut never_cancel)?;
        }
        Some(collector.finish()?)
    } else {
        ensure!(
            args.len() == 4,
            "noise-only WAV is meaningful only in profile mode"
        );
        None
    };
    let preparation = Instant::now();
    COUNTING.store(true, Ordering::Relaxed);
    let mut denoiser = Denoiser::new(spec.sample_rate, channels, config.clone(), profile)?;
    COUNTING.store(false, Ordering::Relaxed);
    let preparation_allocations = ALLOCATIONS.swap(0, Ordering::Relaxed);
    let preparation_allocated_bytes = BYTES.swap(0, Ordering::Relaxed);
    let preparation_ns = preparation.elapsed().as_nanos();
    let destination = Path::new(&args[2]);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .context("creating new evaluation output (never overwrites)")?;
    let rendered = (|| -> Result<_> {
        let mut writer = hound::WavWriter::new(BufWriter::with_capacity(64 * 1024, file), spec)?;
        let mut input = Vec::with_capacity(4096 * channels);
        let mut output = Vec::with_capacity((4096 + 2 * config.fft_size) * channels);
        let mut process_ns = 0;
        let mut output_samples = 0_u64;
        let wall = Instant::now();
        for sample in reader.samples::<f32>() {
            input.push(sample?);
            if input.len() == 4096 * channels {
                let start = Instant::now();
                COUNTING.store(true, Ordering::Relaxed);
                denoiser.process_block(&input, &mut output, &mut never_cancel)?;
                COUNTING.store(false, Ordering::Relaxed);
                process_ns += start.elapsed().as_nanos();
                for &sample in &output {
                    writer.write_sample(sample)?;
                }
                output_samples += output.len() as u64;
                output.clear();
                input.clear();
            }
        }
        if !input.is_empty() {
            let start = Instant::now();
            COUNTING.store(true, Ordering::Relaxed);
            denoiser.process_block(&input, &mut output, &mut never_cancel)?;
            COUNTING.store(false, Ordering::Relaxed);
            process_ns += start.elapsed().as_nanos();
            for &sample in &output {
                writer.write_sample(sample)?;
            }
            output_samples += output.len() as u64;
            output.clear();
        }
        let start = Instant::now();
        COUNTING.store(true, Ordering::Relaxed);
        denoiser.finish(&mut output, &mut never_cancel)?;
        COUNTING.store(false, Ordering::Relaxed);
        process_ns += start.elapsed().as_nanos();
        for &sample in &output {
            writer.write_sample(sample)?;
        }
        output_samples += output.len() as u64;
        ensure!(
            output_samples == expected_samples,
            "denoiser changed exact source frame count"
        );
        writer.finalize()?;
        Ok(
            json!({"schema":1,"input":args[1].to_string_lossy(),"output":args[2].to_string_lossy(),
            "sample_rate":spec.sample_rate,"channels":channels,"frames":output_samples / channels as u64,
            "config":config,"preparation_ns":preparation_ns,"process_ns":process_ns,
            "preparation_allocations":preparation_allocations,
            "preparation_allocated_bytes":preparation_allocated_bytes,
            "process_allocations":ALLOCATIONS.load(Ordering::Relaxed),
            "process_allocated_bytes":BYTES.load(Ordering::Relaxed),
            "render_wall_ns":wall.elapsed().as_nanos(),
            "measurement":"single run; process excludes WAV I/O; profile capture excluded; repeat externally for timing comparisons"}),
        )
    })();
    match rendered {
        Ok(report) => println!("{}", serde_json::to_string_pretty(&report)?),
        Err(error) => {
            let _ = fs::remove_file(destination);
            return Err(error);
        }
    }
    Ok(())
}
