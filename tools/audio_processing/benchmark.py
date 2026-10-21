#!/usr/bin/env python3
"""Run reproducible real-worker benchmarks and compare with local reference audio.

No downloads. References are scored at absolute source alignment (never shifted or
level-normalized for scoring). SI-SDR is reported separately from absolute error.
"""
import argparse
import json
import math
from pathlib import Path
import resource
import statistics
import subprocess
import sys
import time
from worker import digest, read_json


def metrics(reference, estimate, rate, core_seconds=24):
    import numpy as np
    if reference.shape != estimate.shape:
        raise ValueError("reference/output channel or frame mismatch")
    ref = reference.astype("float64")
    est = estimate.astype("float64")
    error = est-ref
    def db(value):
        return 10*math.log10(max(float(value), 1e-30))
    results = {"absolute_snr_db":db(np.sum(ref**2))-db(np.sum(error**2)),
               "rms_error":float(np.sqrt(np.mean(error**2))),
               "peak_error":float(np.max(np.abs(error))),
               "reference_rms":float(np.sqrt(np.mean(ref**2))),
               "output_rms":float(np.sqrt(np.mean(est**2))),
               "output_peak":float(np.max(np.abs(est))), "si_sdr_db":[], "reference_gain":[]}
    for channel in range(ref.shape[1]):
        target, output = ref[:,channel]-ref[:,channel].mean(), est[:,channel]-est[:,channel].mean()
        projection = target * (np.dot(output,target)/max(np.dot(target,target),1e-30))
        results["reference_gain"].append(float(np.dot(output,target)/max(np.dot(target,target),1e-30)))
        results["si_sdr_db"].append(db(np.sum(projection**2))-db(np.sum((output-projection)**2)))
    if ref.shape[1] == 2:
        def coherence(audio):
            left, right = audio[:,0], audio[:,1]
            denominator = np.linalg.norm(left)*np.linalg.norm(right)
            return float(np.dot(left,right)/denominator) if denominator else None
        results["reference_lr_coherence"] = coherence(ref)
        results["output_lr_coherence"] = coherence(est)
        results["reference_side_rms"] = float(np.sqrt(np.mean((ref[:,0]-ref[:,1])**2)))
        results["output_side_rms"] = float(np.sqrt(np.mean((est[:,0]-est[:,1])**2)))
    boundaries = range(core_seconds*rate, len(ref), core_seconds*rate)
    results["outer_boundary_error_jumps"] = [float(np.max(np.abs(error[i]-error[i-1]))) for i in boundaries]
    # The lowest-energy active 20ms reference frames are an objective weak-detail
    # proxy, not a consonant transcript or a substitute for actual listening.
    width = max(1,rate//50)
    frames = len(ref)//width
    if frames:
        ref_blocks = ref[:frames*width].reshape(frames,width,ref.shape[1])
        err_blocks = error[:frames*width].reshape(ref_blocks.shape)
        energies = np.mean(ref_blocks**2,axis=(1,2))
        active = energies > max(float(energies.max())*1e-3,1e-14)
        if active.any():
            weak = active & (energies <= np.quantile(energies[active],0.2))
            target, residual = ref_blocks[weak],err_blocks[weak]
            results["weak_active_frame_snr_db"] = db(np.sum(target**2))-db(np.sum(residual**2))
            results["weak_active_frame_reference_gain"] = float(np.sum((target+residual)*target)/max(np.sum(target**2),1e-30))
    # Absolute alignment check on error cross-correlation, bounded to +/- 50 ms.
    from scipy.signal import correlate, correlation_lags
    count = min(len(ref), rate*60)
    correlation = correlate(est[:count,0], ref[:count,0], mode="full", method="fft")
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
    import soundfile as sf
    request = read_json(args.request)
    base = Path(request["output_dir"])
    report = {"command":sys.argv,"request":request,"corpus_manifest_sha256":digest(args.corpus_manifest),
              "corpus_provenance":read_json(args.corpus_manifest),"reference_sha256":digest(args.reference),"runs":[]}
    reference, rate = sf.read(args.reference, dtype="float32", always_2d=True)
    noisy, source_rate = sf.read(request["input_path"], dtype="float32", always_2d=True)
    if rate != source_rate:
        raise ValueError("reference must be at source rate")
    report["input_quality"] = metrics(reference, noisy, rate)
    for index in range(args.repeats):
        run_request = {**request,"output_dir":str(base.with_name(base.name+"-run-"+str(index)))}
        request_path = base.parent / (base.name+"-request-"+str(index)+".json")
        with request_path.open("x") as stream:
            json.dump(run_request, stream)
        start = time.perf_counter()
        completed = subprocess.run([sys.executable, str(Path(__file__).with_name("worker.py")), "--request", str(request_path)],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        events = [json.loads(line) for line in completed.stdout.splitlines()]
        if completed.returncode or not events or events[-1]["type"] != "complete":
            raise RuntimeError(json.dumps({"exit":completed.returncode,"events":events,"stderr":completed.stderr}))
        result = events[-1]
        stem = "speech" if request["operation"] == "speech" else "vocals"
        output, output_rate = sf.read(result["outputs"][stem], dtype="float32", always_2d=True)
        if output_rate != rate:
            raise ValueError("output rate changed")
        result["end_to_end_wall_seconds"] = time.perf_counter()-start
        result["reference_quality"] = metrics(reference, output, rate, result["processing"]["core_seconds"])
        result["si_sdr_gain_db"] = [a-b for a,b in zip(result["reference_quality"]["si_sdr_db"],report["input_quality"]["si_sdr_db"])]
        if args.upstream_reference:
            upstream, upstream_rate = sf.read(args.upstream_reference, dtype="float32", always_2d=True)
            if upstream_rate != rate:
                raise ValueError("upstream reference rate changed")
            result["upstream_difference"] = metrics(upstream, output, rate, result["processing"]["core_seconds"])
            result["upstream_reference_sha256"] = digest(args.upstream_reference)
        if request["operation"] == "separate":
            accompaniment, _ = sf.read(result["outputs"]["accompaniment"], dtype="float32", always_2d=True)
            result["mixture_reconstruction_peak_residual"] = float(np.max(np.abs(noisy.astype("float64")-output-accompaniment)))
            if args.accompaniment_reference:
                backing, backing_rate = sf.read(args.accompaniment_reference, dtype="float32", always_2d=True)
                if backing_rate != rate:
                    raise ValueError("accompaniment reference rate changed")
                result["accompaniment_quality"] = metrics(backing, accompaniment, rate, result["processing"]["core_seconds"])
        report["runs"].append(result)
    times = [run["end_to_end_wall_seconds"] for run in report["runs"]]
    report["wall_seconds_summary"] = {"median":statistics.median(times),"min":min(times),"max":max(times),
                                      "stdev":statistics.stdev(times) if len(times)>1 else 0.0}
    report["human_listening"] = "not performed; objective scores do not establish listening approval"
    with args.report.open("x") as stream:
        json.dump(report, stream, indent=2, allow_nan=False)
    print(json.dumps({"report":str(args.report),"summary":report["wall_seconds_summary"],
                      "si_sdr_gain_db":report["runs"][0]["si_sdr_gain_db"]}))

if __name__ == "__main__":
    main()
