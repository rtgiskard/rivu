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

Synthetic groups require schema 2 manifests from prepare_speech.py. Re-prepare
older groups; each manifest identifies its generated WAVs by relative path and
SHA256 so the whole group can be moved without changing the selected inputs.

Omit --case to evaluate every case. Repeat --case to select synthetic condition
stems (for example clean or white_+0dB, across speech-* folders), paired utterance
stems (for example p232_001), or music directories (for example track-00).
Every requested selector must exist; unknown selectors reject the run before
output creation or model execution. Paired corpora require
clean/ and noisy/ matching FLOAT WAV files plus manifest.json. Music requires
track-*/{mixture,vocals,accompaniment}.wav plus manifest.json with explicit
source/SHA256/diagnostic_only records; music_manifest.py defines the contract.
Mark the documented Al James reference erratum via prepare_music.py
--diagnostic-source; no track ordinal implies an
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
    from .benchmark import PreparedReference, TIMING_BOUNDARIES, timing_summary
    from .common import digest, read_json_with_digest
    from .export import completed_export_report
    from ..audio_quality.metric_core import reconstruction_metrics, score_difference, score_summary
    from ..audio_quality.music_manifest import music_records
    from ..audio_quality.evaluate import load_audio, load_identified, require_alignment
    from ..audio_quality.synthetic_manifest import synthetic_cases
else:
    from benchmark import PreparedReference, TIMING_BOUNDARIES, timing_summary
    from common import digest, read_json_with_digest
    from export import completed_export_report
    from audio_quality.metric_core import reconstruction_metrics, score_difference, score_summary
    from audio_quality.music_manifest import music_records
    from audio_quality.evaluate import load_audio, load_identified, require_alignment
    from audio_quality.synthetic_manifest import synthetic_cases

def _paired_records(manifest, root):
    if not isinstance(manifest, dict) or manifest.get("schema") != 1:
        raise ValueError("paired manifest schema 1 with prepared float identities required; re-prepare corpus")
    records = manifest.get("records")
    if not isinstance(records, list) or not records:
        raise ValueError("paired manifest requires prepared float identity records; re-prepare corpus")
    by_name = {}
    for record in records:
        if not isinstance(record, dict):
            raise ValueError("paired manifest records must be objects")
        name = record.get("utterance")
        if not isinstance(name, str) or not re.fullmatch(r"[^/\\]+\.wav", name) or name in by_name:
            raise ValueError("paired manifest requires unique WAV utterance names")
        for kind in ("clean", "noisy"):
            pair = record.get(kind)
            item = pair.get("float") if isinstance(pair, dict) else None
            if (not isinstance(item, dict) or not isinstance(item.get("sha256"), str)
                    or not re.fullmatch(r"[a-f0-9]{64}", item["sha256"])
                    or type(item.get("bytes")) is not int or item["bytes"] <= 0):
                raise ValueError("paired records require clean/noisy prepared float SHA256/bytes; re-prepare corpus")
        by_name[name] = record
    for kind in ("clean", "noisy"):
        if {path.name for path in (root / kind).glob("*.wav")} != set(by_name):
            raise ValueError("paired manifest utterances differ from prepared " + kind + " files")
    return [(name, by_name[name]) for name in sorted(by_name)]


def _require_prepared_identity(audio, expected, label):
    if any(audio.identity[key] != expected[key] for key in ("sha256", "bytes")):
        raise ValueError("prepared corpus identity mismatch: " + label)



def cases(kind, root, selected):
    matched = set()
    if kind == "synthetic":
        for folder in sorted(root.glob("speech-*")):
            if not folder.is_dir():
                continue
            manifest, checksum = read_json_with_digest(folder / "manifest.json")
            reference, *conditions = synthetic_cases(folder, manifest)
            for case in (reference, *conditions):
                source = case["path"]
                if selected and source.stem not in selected:
                    continue
                matched.add(source.stem)
                yield (folder.name+"-"+source.stem, source, reference["path"], checksum,
                       None, False, (case["sha256"], reference["sha256"]))
    elif kind == "paired":
        manifest, checksum = read_json_with_digest(root / "manifest.json")
        for name, record in _paired_records(manifest, root):
            source = root / "noisy" / name
            if selected and source.stem not in selected:
                continue
            matched.add(source.stem)
            yield source.stem,source,root/"clean"/name,checksum,None,False,record
    else:
        manifest, checksum = read_json_with_digest(root / "manifest.json")
        for folder, record in music_records(manifest, root):
            if selected and folder.name not in selected:
                continue
            matched.add(folder.name)
            yield folder.name,folder/"mixture.wav",folder/"vocals.wav",checksum,folder/"accompaniment.wav",record["diagnostic_only"],record
    missing = selected - matched
    if missing:
        raise ValueError("unknown " + kind + " case selection: " + ", ".join(sorted(missing)))


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kind",choices=("synthetic","paired","music"),required=True)
    parser.add_argument("--corpus-root",type=Path,required=True)
    parser.add_argument("--model-manifest",type=Path,required=True)
    parser.add_argument("--output-dir",type=Path,required=True)
    parser.add_argument("--case",action="append",default=[],
                        help="repeatable selector: synthetic condition stem, paired utterance stem, or music track directory; every selector must exist")
    parser.add_argument("--threads",type=int,default=4)
    args=parser.parse_args()
    if args.output_dir.exists() or not args.output_dir.parent.is_dir() or not 1<=args.threads<=8:
        parser.error("new output directory with existing parent and threads1..8 required")
    corpus=args.corpus_root.resolve()
    try:
        records=list(cases(args.kind,corpus,set(args.case)))
    except ValueError as error:
        parser.error(str(error))
    if not records:
        parser.error("no matching corpus files")
    model=args.model_manifest.resolve()
    operation="separate" if args.kind=="music" else "speech"
    model_config, model_checksum = read_json_with_digest(model)
    if model_config["operation"]!=operation:
        parser.error("model operation differs from corpus")
    args.output_dir.mkdir(mode=0o700)
    output=args.output_dir.resolve()
    report={"command":sys.argv,"model_manifest_sha256":model_checksum,"runs":[],
            "timing_boundaries":{**TIMING_BOUNDARIES,
                "evaluation_wall_seconds":"Before each case's source/reference/backing reads and identity checks through completed run scoring, including reference metric preparation, request creation, worker subprocess, artifact reads/writes and intelligibility metrics. Excludes corpus/model setup, appending the record, progress printing, aggregate summaries and final report serialization/write."},
            "human_listening":"not performed; objective results do not establish listening approval"}
    if operation=="speech":
        import importlib.metadata
        from pystoi import stoi
        report["metric_packages"]={"pystoi":importlib.metadata.version("pystoi")}
    def score_case(name, source, reference, corpus_checksum, backing, diagnostic, source_identity):
        evaluation_start=time.perf_counter()
        reference_pcm = load_identified(reference, dtype="float32", max_frames=None)
        input_pcm = reference_pcm if source.resolve() == reference.resolve() else load_identified(source, dtype="float32", max_frames=None)
        require_alignment(reference_pcm, input_pcm, "paired source/reference")
        reference_audio, rate = reference_pcm.data, reference_pcm.rate
        input_audio = input_pcm.data
        backing_pcm = load_identified(backing, dtype="float32", max_frames=None) if backing else None
        if backing_pcm is not None:
            require_alignment(reference_pcm, backing_pcm, "backing reference")
        if args.kind == "music":
            for stem, audio in (("mixture", input_pcm), ("vocals", reference_pcm), ("accompaniment", backing_pcm)):
                _require_prepared_identity(audio, source_identity["float"][stem], name + "/" + stem)
        elif args.kind == "paired":
            for kind, audio in (("clean", reference_pcm), ("noisy", input_pcm)):
                _require_prepared_identity(audio, source_identity[kind]["float"], name + "/" + kind)
        elif (input_pcm.identity["sha256"], reference_pcm.identity["sha256"]) != source_identity:
            raise ValueError(f"synthetic corpus input differs from manifest: {name}")
        prepared = PreparedReference(reference_audio, rate)
        request={"version":1,"task_id":name,"operation":operation,"input_path":str(source.resolve()),
                 "output_dir":str(output/name),"model_manifest":str(model),"device":"cpu","threads":args.threads,
                 "parameters":{"overlap":0.75} if operation=="separate" else {"wet_mix":1.0,"attenuation_limit_db":80}}
        request_path=output/(name+"-request.json")
        request_path.write_text(json.dumps(request))
        worker_start=time.perf_counter()
        completed=subprocess.run([sys.executable,"-B",str(Path(__file__).with_name("export.py")),"--request",str(request_path)],
                                 stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        worker_seconds=time.perf_counter()-worker_start
        (output/(name+"-events.jsonl")).write_text(completed.stdout)
        (output/(name+"-stderr.txt")).write_text(completed.stderr)
        result = completed_export_report(completed, request, input_pcm.identity["sha256"])
        estimate_pcm = load_audio(result["outputs"]["vocals" if operation=="separate" else "speech"], dtype="float32", max_frames=None)
        require_alignment(reference_pcm, estimate_pcm, "output")
        estimate = estimate_pcm.data
        core=result["processing"]["core_seconds"]
        record={"name":name,"diagnostic_only":diagnostic,"input_path":str(source),"reference_path":str(reference),
                "input_sha256":input_pcm.identity["sha256"],
                "reference_sha256":reference_pcm.identity["sha256"],"corpus_manifest_sha256":corpus_checksum,"worker":result,
                "worker_subprocess_wall_seconds":worker_seconds,"input_quality":prepared.score(input_audio,core),
                "output_quality":prepared.score(estimate,core)}
        if args.kind == "music":
            record["corpus_source"]={key:source_identity[key] for key in ("source", "sha256")}
        record["si_sdr_gain_db"]=[score_difference(a,b) for a,b in zip(record["output_quality"]["si_sdr_db"],record["input_quality"]["si_sdr_db"])]
        if backing:
            estimated_pcm = load_audio(result["outputs"]["accompaniment"], dtype="float32", max_frames=None)
            require_alignment(reference_pcm, estimated_pcm, "backing output")
            estimated_accompaniment = estimated_pcm.data
            record["accompaniment_reference_sha256"] = backing_pcm.identity["sha256"]
            record["accompaniment_quality"] = PreparedReference(backing_pcm.data,rate).score(estimated_accompaniment,core)
            record["mixture_reconstruction_max_abs"]=reconstruction_metrics(input_audio, estimate, estimated_accompaniment)[1]
        if operation=="speech":
            record["intelligibility"]={}
            for extended in (False,True):
                metric_name="estoi" if extended else "stoi"
                record["intelligibility"][metric_name]={
                    "input":[float(stoi(reference_audio[:,channel],input_audio[:,channel],rate,extended=extended)) for channel in range(reference_audio.shape[1])],
                    "output":[float(stoi(reference_audio[:,channel],estimate[:,channel],rate,extended=extended)) for channel in range(reference_audio.shape[1])]}
        record["evaluation_wall_seconds"]=time.perf_counter()-evaluation_start
        return record

    for index, case in enumerate(records):
        record = score_case(*case)
        report["runs"].append(record)
        print(json.dumps({"case":record["name"],"completed":index+1,"total":len(records),"si_sdr_gain_db":record["si_sdr_gain_db"]},allow_nan=False),flush=True)
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
