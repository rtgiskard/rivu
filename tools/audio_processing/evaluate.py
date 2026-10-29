#!/usr/bin/env python3
"""Evaluate actual local models against a provenance-bound paired corpus.

No downloads, clipping, alignment search or level normalization. Run one CPU
model job at a time. Use the corresponding prepared interpreter, for example:
  python tools/audio_processing/evaluate.py --kind synthetic --corpus-root /abs/corpus \
    --model-manifest /abs/moss/manifest.json --output-dir /abs/new-evaluation \
    --case clean --case white_+0dB --case fan_+0dB --case varying_+0dB --case impulsive_+0dB
  python tools/audio_processing/evaluate.py --kind paired --corpus-root /abs/voicebank \
    --model-manifest /abs/moss/manifest.json --output-dir /abs/new-evaluation
  python tools/audio_processing/evaluate.py --kind music --corpus-root /abs/music-float \
    --model-manifest /abs/kim/manifest.json --output-dir /abs/new-evaluation

Omit --case to evaluate every canonical synthetic case. Paired corpora require
clean/ and noisy/ matching FLOAT WAV files plus manifest.json. Music requires
track-*/{mixture,vocals,accompaniment}.wav plus schema-2 manifest.json with explicit
source/SHA256/diagnostic_only records. Mark the documented Al James reference
erratum via prepare_music.py --diagnostic-source; no track ordinal implies an
erratum. All AAC MUSDB excerpts are bandlimited, not fullband quality evidence.
"""
import argparse
import json
from pathlib import Path
import re
import statistics
import subprocess
import sys
import time
if __package__:
    from .benchmark import metrics, TIMING_BOUNDARIES, timing_summary
    from .runtime.worker import digest, read_json
    from ..audio_quality.metric_core import score_difference, score_summary
    from ..audio_quality.music_manifest import music_records
else:
    from benchmark import metrics, TIMING_BOUNDARIES, timing_summary
    from runtime.worker import digest, read_json
    from audio_quality.metric_core import score_difference, score_summary
    from audio_quality.music_manifest import music_records


def cases(kind, root, selected):
    if kind == "synthetic":
        for folder in sorted(root.glob("speech-*")):
            if not folder.is_dir():
                continue
            for source in sorted(folder.glob("*.wav")):
                if not re.fullmatch(r"clean|(?:white|fan|varying|impulsive)_[+-]\d+dB",source.stem):
                    continue
                if selected and source.stem not in selected:
                    continue
                yield folder.name+"-"+source.stem,source,folder/"clean.wav",folder/"manifest.json",None,False,None
    elif kind == "paired":
        for source in sorted((root/"noisy").glob("*.wav")):
            yield source.stem,source,root/"clean"/source.name,root/"manifest.json",None,False,None
    else:
        for folder, record in music_records(read_json(root/"manifest.json"), root):
            identity = {key:record[key] for key in ("source", "sha256")}
            yield folder.name,folder/"mixture.wav",folder/"vocals.wav",root/"manifest.json",folder/"accompaniment.wav",record["diagnostic_only"],identity


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kind",choices=("synthetic","paired","music"),required=True)
    parser.add_argument("--corpus-root",type=Path,required=True)
    parser.add_argument("--model-manifest",type=Path,required=True)
    parser.add_argument("--output-dir",type=Path,required=True)
    parser.add_argument("--case",action="append",default=[])
    parser.add_argument("--threads",type=int,default=4)
    args=parser.parse_args()
    if args.output_dir.exists() or not args.output_dir.parent.is_dir() or not 1<=args.threads<=8:
        parser.error("new output directory with existing parent and threads1..8 required")
    corpus=args.corpus_root.resolve()
    records=list(cases(args.kind,corpus,set(args.case)))
    if not records:
        parser.error("no matching paired corpus files")
    import numpy as np
    import soundfile as sf
    model=args.model_manifest.resolve()
    operation="separate" if args.kind=="music" else "speech"
    if read_json(model)["operation"]!=operation:
        parser.error("model operation differs from corpus")
    args.output_dir.mkdir(mode=0o700)
    output=args.output_dir.resolve()
    report={"command":sys.argv,"model_manifest_sha256":digest(model),"runs":[],
            "timing_boundaries":TIMING_BOUNDARIES,
            "human_listening":"not performed; objective results do not establish listening approval"}
    if operation=="speech":
        import importlib.metadata
        from pystoi import stoi
        report["metric_packages"]={"pystoi":importlib.metadata.version("pystoi")}
    for index,(name,source,reference,provenance,backing,diagnostic,source_identity) in enumerate(records):
        evaluation_start=time.perf_counter()
        reference_audio,rate=sf.read(reference,dtype="float32",always_2d=True)
        input_audio,input_rate=sf.read(source,dtype="float32",always_2d=True)
        if rate!=input_rate:
            raise ValueError("paired source/reference rates differ: "+name)
        request={"version":1,"task_id":name,"operation":operation,"input_path":str(source.resolve()),
                 "output_dir":str(output/name),"model_manifest":str(model),"device":"cpu","threads":args.threads,
                 "parameters":{"overlap":0.75} if operation=="separate" else {"wet_mix":1.0,"attenuation_limit_db":80}}
        request_path=output/(name+"-request.json")
        request_path.write_text(json.dumps(request))
        worker_start=time.perf_counter()
        completed=subprocess.run([sys.executable,"-B",str(Path(__file__).with_name("export.py")),"--request",str(request_path)],
                                 stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        worker_seconds=time.perf_counter()-worker_start
        events=[json.loads(line) for line in completed.stdout.splitlines()]
        (output/(name+"-events.jsonl")).write_text(completed.stdout)
        (output/(name+"-stderr.txt")).write_text(completed.stderr)
        if completed.returncode or not events or events[-1]["type"]!="complete" or events[-1].get("version")!=3 or events[-1].get("task_id")!=request["task_id"]:
            raise RuntimeError(json.dumps({"case":name,"exit":completed.returncode,"events":events,"stderr":completed.stderr}))
        with (Path(request["output_dir"])/"result.json").open() as stream:
            result=json.load(stream)
        estimate,output_rate=sf.read(result["outputs"]["vocals" if operation=="separate" else "speech"],dtype="float32",always_2d=True)
        if output_rate!=rate:
            raise ValueError("output rate changed: "+name)
        core=result["processing"]["core_seconds"]
        record={"name":name,"diagnostic_only":diagnostic,"input_path":str(source),"reference_path":str(reference),
                "reference_sha256":digest(reference),"corpus_manifest_sha256":digest(provenance),"worker":result,
                "worker_subprocess_wall_seconds":worker_seconds,"input_quality":metrics(reference_audio,input_audio,rate,core),
                "output_quality":metrics(reference_audio,estimate,rate,core)}
        if source_identity is not None:
            record["corpus_source"]=source_identity
        record["si_sdr_gain_db"]=[score_difference(a,b) for a,b in zip(record["output_quality"]["si_sdr_db"],record["input_quality"]["si_sdr_db"])]
        if backing:
            accompaniment,accompaniment_rate=sf.read(backing,dtype="float32",always_2d=True)
            estimated_accompaniment,estimated_rate=sf.read(result["outputs"]["accompaniment"],dtype="float32",always_2d=True)
            if accompaniment_rate!=rate or estimated_rate!=rate:
                raise ValueError("backing rates changed")
            record["accompaniment_reference_sha256"]=digest(backing)
            record["accompaniment_quality"]=metrics(accompaniment,estimated_accompaniment,rate,core)
            record["mixture_reconstruction_max_abs"]=float(np.max(np.abs(input_audio.astype("float64")-estimate-estimated_accompaniment)))
        if operation=="speech":
            record["intelligibility"]={}
            for extended in (False,True):
                metric_name="estoi" if extended else "stoi"
                record["intelligibility"][metric_name]={
                    "input":[float(stoi(reference_audio[:,channel],input_audio[:,channel],rate,extended=extended)) for channel in range(reference_audio.shape[1])],
                    "output":[float(stoi(reference_audio[:,channel],estimate[:,channel],rate,extended=extended)) for channel in range(reference_audio.shape[1])]}
        record["evaluation_wall_seconds"]=time.perf_counter()-evaluation_start
        report["runs"].append(record)
        print(json.dumps({"case":name,"completed":index+1,"total":len(records),"si_sdr_gain_db":record["si_sdr_gain_db"]},allow_nan=False),flush=True)
    primary=[run for run in report["runs"] if not run["diagnostic_only"] and not run["name"].endswith("-clean")]
    gains=[gain for run in primary for gain in run["si_sdr_gain_db"]]
    gain_summary=score_summary(gains)
    report["summary"]={"cases":len(records),"primary_cases":len(primary),"median_si_sdr_gain_db":gain_summary["median"],
                       "min_si_sdr_gain_db":gain_summary["min"],"max_si_sdr_gain_db":gain_summary["max"],
                       "si_sdr_gain_counts":{key:value for key,value in gain_summary.items() if key.endswith("_count")},
                       "median_warm_rtf":statistics.median(run["worker"]["metrics"]["real_time_factor"] for run in report["runs"])}
    report["timing_summary"]=timing_summary(report["runs"])
    with (output/"report.json").open("x") as stream:
        json.dump(report,stream,indent=2,allow_nan=False)
    print(json.dumps({"report":str(output/"report.json"),"summary":report["summary"],"timing_summary":report["timing_summary"]},allow_nan=False),flush=True)

if __name__=="__main__":
    main()
