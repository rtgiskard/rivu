#!/usr/bin/env python3
"""Run reproducible real-worker benchmarks and compare with local reference audio.

No downloads. References are scored at absolute source alignment (never shifted or
level-normalized for scoring). SI-SDR is reported separately from absolute error.
"""
import argparse
import json
from pathlib import Path
import statistics
import subprocess
import sys
import time
if __package__:
    from .runtime.worker import EVENT_VERSION, digest, read_json
    from ..audio_quality.metric_core import db_ratio, projection_metrics, reconstruction_metrics, score_difference
    from ..audio_quality.evaluate import load_identified, require_alignment
else:
    from runtime.worker import EVENT_VERSION, digest, read_json
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
    from audio_quality.metric_core import db_ratio, projection_metrics, reconstruction_metrics, score_difference
    from audio_quality.evaluate import load_identified, require_alignment


TIMING_BOUNDARIES = {
    "worker_subprocess_wall_seconds": "Immediately before subprocess.run through its return; includes process startup, worker work and captured pipe collection only.",
    "evaluation_wall_seconds": "Before each independent run's request creation through completed run record construction, including worker output/event reads, artifact writes, input identity validation and output quality scoring. Excludes one-time fixed reference/input reads, hashes and input quality scoring; excludes appending the completed record, progress printing, aggregate summaries and final report serialization/write.",
}


def timing_summary(runs):
    summary = {}
    for field in TIMING_BOUNDARIES:
        times = [run[field] for run in runs]
        summary[field] = {"median":statistics.median(times), "min":min(times), "max":max(times),
                          "stdev":statistics.stdev(times) if len(times)>1 else 0.0}
    return summary


def metrics(reference, estimate, rate, core_seconds=24):
    import numpy as np
    if reference.shape != estimate.shape:
        raise ValueError("reference/output channel or frame mismatch")
    # Float64 arithmetic is local to bounded blocks, never whole-track copies.
    block_size = max(rate // 50, 65536 // max(1, reference.shape[1]))
    ref_energy = est_energy = error_energy = side_ref = side_est = 0.0
    peak_error = output_peak = 0.0
    for start in range(0, len(reference), block_size):
        ref = reference[start:start+block_size].astype("float64")
        est = estimate[start:start+block_size].astype("float64")
        error = est - ref
        ref_energy += float(np.sum(ref**2))
        est_energy += float(np.sum(est**2))
        error_energy += float(np.sum(error**2))
        peak_error = max(peak_error, float(np.max(np.abs(error))))
        output_peak = max(output_peak, float(np.max(np.abs(est))))
        if reference.shape[1] == 2:
            side_ref += float(np.sum(((ref[:,0]-ref[:,1])/2)**2))
            side_est += float(np.sum(((est[:,0]-est[:,1])/2)**2))
    results = {"absolute_snr_db":db_ratio(ref_energy, error_energy),
               "rms_error":float(np.sqrt(error_energy/reference.size)),
               "peak_error":peak_error, "reference_rms":float(np.sqrt(ref_energy/reference.size)),
               "output_rms":float(np.sqrt(est_energy/reference.size)), "output_peak":output_peak,
               "si_sdr_db":[], "reference_gain":[]}
    for channel in range(reference.shape[1]):
        score, gain = projection_metrics(reference[:,channel], estimate[:,channel])
        results["reference_gain"].append(gain)
        results["si_sdr_db"].append(score)
    if reference.shape[1] == 2:
        def coherence(audio):
            energy_left = energy_right = product = 0.0
            for start in range(0, len(audio), block_size):
                left, right = audio[start:start+block_size].astype("float64").T
                energy_left += float(np.dot(left, left))
                energy_right += float(np.dot(right, right))
                product += float(np.dot(left, right))
            denominator = np.sqrt(energy_left)*np.sqrt(energy_right)
            return float(product/denominator) if denominator else None
        results["reference_lr_coherence"] = coherence(reference)
        results["output_lr_coherence"] = coherence(estimate)
        results["reference_side_rms"] = float(np.sqrt(side_ref/len(reference)))
        results["output_side_rms"] = float(np.sqrt(side_est/len(reference)))
    boundaries = range(core_seconds*rate, len(reference), core_seconds*rate)
    results["outer_boundary_error_jumps"] = [float(np.max(np.abs(
        (estimate[i].astype("float64")-reference[i]) -
        (estimate[i-1].astype("float64")-reference[i-1])))) for i in boundaries]
    # Keep only per-20ms-frame energies; weak-frame sample arrays stay bounded.
    width = max(1, rate//50)
    frames = len(reference)//width
    energies = np.empty(frames, dtype="float64")
    for index in range(frames):
        ref = reference[index*width:(index+1)*width].astype("float64")
        energies[index] = np.mean(ref**2)
    if frames:
        active = energies > max(float(energies.max())*1e-3, 1e-14)
        if active.any():
            weak = active & (energies <= np.quantile(energies[active], 0.2))
            target_energy = residual_energy = product = 0.0
            for index in np.flatnonzero(weak):
                target = reference[index*width:(index+1)*width].astype("float64")
                output = estimate[index*width:(index+1)*width].astype("float64")
                residual = output-target
                target_energy += float(np.sum(target**2))
                residual_energy += float(np.sum(residual**2))
                product += float(np.sum((target+residual)*target))
            results["weak_active_frame_snr_db"] = db_ratio(target_energy, residual_energy)
            results["weak_active_frame_reference_gain"] = product/target_energy
    from scipy.signal import correlate, correlation_lags
    count = min(len(reference), rate*60)
    correlation = correlate(estimate[:count,0].astype("float64"), reference[:count,0].astype("float64"), mode="full", method="fft")
    lags = correlation_lags(count, count)
    keep = np.abs(lags) <= rate//20
    results["estimated_lag_samples"] = int(lags[keep][np.argmax(correlation[keep])])
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--request", type=Path, required=True)
    parser.add_argument("--reference", type=Path, required=True, help="clean speech or clean vocal stem")
    parser.add_argument("--accompaniment-reference", type=Path)
    parser.add_argument("--upstream-reference", type=Path, help="identical-weight upstream output; scored independently")
    parser.add_argument("--corpus-manifest", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    if not 1 <= args.repeats <= 10:
        parser.error("repeats must be 1..10")
    import numpy as np
    request = read_json(args.request)
    base = Path(request["output_dir"])
    report = {"command":sys.argv,"request":request,"corpus_manifest_sha256":digest(args.corpus_manifest),
              "corpus_provenance":read_json(args.corpus_manifest),"runs":[],"timing_boundaries":TIMING_BOUNDARIES}
    reference_pcm = load_identified(args.reference, dtype="float32", max_frames=None)
    input_pcm = reference_pcm if args.reference.resolve() == Path(request["input_path"]).resolve() else load_identified(request["input_path"], dtype="float32", max_frames=None)
    require_alignment(reference_pcm, input_pcm, "benchmark input")
    reference, rate = reference_pcm.data, reference_pcm.rate
    noisy = input_pcm.data
    report["input_sha256"] = input_pcm.identity["sha256"]
    report["reference_sha256"] = reference_pcm.identity["sha256"]
    report["input_quality"] = metrics(reference, noisy, rate)
    upstream = backing = None
    if args.upstream_reference:
        upstream_pcm = load_identified(args.upstream_reference, dtype="float32", max_frames=None)
        require_alignment(reference_pcm, upstream_pcm, "upstream reference")
        upstream = upstream_pcm.data
        upstream_sha256 = upstream_pcm.identity["sha256"]
    if request["operation"] == "separate" and args.accompaniment_reference:
        backing_pcm = load_identified(args.accompaniment_reference, dtype="float32", max_frames=None)
        require_alignment(reference_pcm, backing_pcm, "accompaniment reference")
        backing = backing_pcm.data
        report["accompaniment_reference_sha256"] = backing_pcm.identity["sha256"]
    for index in range(args.repeats):
        evaluation_start = time.perf_counter()
        run_request = {**request,"output_dir":str(base.with_name(base.name+"-run-"+str(index)))}
        request_path = base.parent / (base.name+"-request-"+str(index)+".json")
        with request_path.open("x") as stream:
            json.dump(run_request, stream)
        worker_start = time.perf_counter()
        completed = subprocess.run([sys.executable, "-B", str(Path(__file__).with_name("export.py")), "--request", str(request_path)],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        worker_seconds = time.perf_counter()-worker_start
        events = [json.loads(line) for line in completed.stdout.splitlines()]
        if completed.returncode or not events or events[-1]["type"] != "complete" or events[-1].get("version") != EVENT_VERSION or events[-1].get("task_id") != request["task_id"]:
            raise RuntimeError(json.dumps({"exit":completed.returncode,"events":events,"stderr":completed.stderr}))
        with (Path(run_request["output_dir"]) / "result.json").open() as stream:
            result = json.load(stream)
        if result.get("source_sha256") != report["input_sha256"]:
            raise ValueError("worker input does not match benchmark input")
        stem = "speech" if request["operation"] == "speech" else "vocals"
        output_pcm = load_identified(result["outputs"][stem], dtype="float32", max_frames=None)
        require_alignment(reference_pcm, output_pcm, "output")
        output = output_pcm.data
        result["worker_subprocess_wall_seconds"] = worker_seconds
        result["reference_quality"] = metrics(reference, output, rate, result["processing"]["core_seconds"])
        result["si_sdr_gain_db"] = [score_difference(a,b) for a,b in zip(result["reference_quality"]["si_sdr_db"],report["input_quality"]["si_sdr_db"])]
        if upstream is not None:
            result["upstream_difference"] = metrics(upstream, output, rate, result["processing"]["core_seconds"])
            result["upstream_reference_sha256"] = upstream_sha256
        if request["operation"] == "separate":
            accompaniment_pcm = load_identified(result["outputs"]["accompaniment"], dtype="float32", max_frames=None)
            require_alignment(reference_pcm, accompaniment_pcm, "accompaniment output")
            accompaniment = accompaniment_pcm.data
            result["mixture_reconstruction_peak_residual"] = reconstruction_metrics(noisy, output, accompaniment)[1]
            if backing is not None:
                result["accompaniment_quality"] = metrics(backing, accompaniment, rate, result["processing"]["core_seconds"])
        result["evaluation_wall_seconds"] = time.perf_counter()-evaluation_start
        report["runs"].append(result)
    report["timing_summary"] = timing_summary(report["runs"])
    report["human_listening"] = "not performed; objective scores do not establish listening approval"
    with args.report.open("x") as stream:
        json.dump(report, stream, indent=2, allow_nan=False)
    print(json.dumps({"report":str(args.report),"summary":report["timing_summary"],
                      "si_sdr_gain_db":report["runs"][0]["si_sdr_gain_db"]}, allow_nan=False))

if __name__ == "__main__":
    main()
