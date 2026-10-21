"""Pinned Hybrid Transformer Demucs separation preparation.

This is the original four-stem HTDemucs architecture, not Kim weights under an
alias. The worker exposes its vocals plus an exact mixture-minus-vocals
complement, deliberately not an independently estimated four-stem mix.
"""
import json
import shutil
import subprocess

REVISION = "64fd0216b9f0309c9d055457002d6910dc245950"
METADATA_URL = f"https://raw.githubusercontent.com/Ryan5453/unblend/{REVISION}/unblend/metadata.yaml"
WEIGHT_URL = "https://huggingface.co/Ryan5453/unblend/resolve/9ba75778acf1b64fc40a9ea05137a2c6af9cd1d5/htdemucs.safetensors"
WEIGHT_SHA256 = "d3cb2f212d0652945fd033c26e1fbfdf9b6fbb9087aa6104def87f08b1016a81"
WEIGHT_SIZE = 84023856


def prepare(args, target, download):
    from worker import digest
    checkpoint = target / "model.safetensors"
    if args.download:
        download(WEIGHT_URL.replace("huggingface.co", args.hf_host) + "?download=true", checkpoint, WEIGHT_SIZE)
    else:
        shutil.copyfile(args.weights, checkpoint)
    if checkpoint.stat().st_size != WEIGHT_SIZE or digest(checkpoint) != WEIGHT_SHA256:
        raise ValueError("HTDemucs artifact differs from pinned unblend conversion")
    metadata = download(METADATA_URL, target / "upstream-metadata.yaml", 65536)
    # Use the explicitly installed interpreter's PyYAML, not a system dependency.
    entry = json.loads(subprocess.check_output([
        str(args.interpreter), "-c",
        "import json,sys,yaml; print(json.dumps(yaml.safe_load(open(sys.argv[1]))['models']['htdemucs']))",
        str(metadata),
    ], text=True))
    expected = entry["checkpoint"]
    if (expected["url"], expected["sha256"], expected["size_bytes"]) != (WEIGHT_URL, WEIGHT_SHA256, WEIGHT_SIZE):
        raise ValueError("pinned architecture metadata/checkpoint identity mismatch")
    entry["checkpoint"] = {"format":"safetensors", "path":str(checkpoint),
                           "sha256":WEIGHT_SHA256, "size_bytes":WEIGHT_SIZE}
    (target / "registry.json").write_text(json.dumps({"version":1,"models":{"htdemucs":entry}}, indent=2) + "\n")
    # Preserve primary author card verbatim. Absence of a grant is metadata, not a
    # candidate gate. This local explicit preparation grants no redistribution.
    card_url = "https://huggingface.co/adefossez/HTDemucs/raw/cbc8a9b1a87023b7fd74e7b3412e6321c0eab003/README.md"
    download(card_url, target / "upstream-author-card.txt", 65536)
    (target / "weight-license.txt").write_text(
        "HTDemucs pretrained weights: no affirmative weight-license grant established here.\n"
        "The engine source is MIT; that does not establish a weight redistribution grant.\n"
        "Prepared only by explicit user request for a public or supplied candidate.\n"
        "Primary author model card is retained verbatim in upstream-author-card.txt.\n"
        "Converter's pinned architecture/provenance/license notes are retained in upstream-metadata.yaml.\n"
        + card_url + "\n")
    return {"version":1,"operation":"separate","id":"htdemucs",
            "packages":{"unblend":"1.0.0","torch":"2.13.0","numpy":"2.2.6","scipy":"1.15.3","soundfile":"0.13.1"},
            "provenance":{"weights":WEIGHT_URL,"weights_sha256":WEIGHT_SHA256,"official_signature":"955717e8",
                          "architecture":"hybrid waveform/spectrogram cross-domain Transformer Demucs",
                          "code":f"https://github.com/Ryan5453/unblend/tree/{REVISION}","code_license":"MIT",
                          "primary_author_card":card_url,"weights_license":"not established; no grant inferred",
                          "transport_host":args.hf_host,"selection":"explicit user preparation; license metadata is not a candidate gate"}}
