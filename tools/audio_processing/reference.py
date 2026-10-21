#!/usr/bin/env python3
"""Create an independent upstream inference reference without downloading anything.

Use the same interpreter/model manifest as worker.py. Example:
  python tools/audio_processing/reference.py --model-manifest /abs/manifest.json \
    --input /abs/input.wav --output /abs/reference.wav --threads 4

Kim calls public upstream unblend Separator on the complete input (max40s), not
worker chunking. Moss executes the pinned original decode and helper functions,
not our decoder. Inputs are resampled in explicit training units, then returned
to source rate/length. No peak normalization or output clipping is applied.
"""
import argparse
import ast
from contextlib import redirect_stdout
import json
import math
import os
from pathlib import Path
import sys
from types import SimpleNamespace

from worker import validate_manifest, check_packages, deny_network, digest


def upstream_moss(base, audio, device):
    import numpy as np
    import torch
    import torchaudio
    from mossformer import load_model
    namespace = {"np":np,"torch":torch,"torchaudio":torchaudio,"MAX_WAV_VALUE":32768.0}
    helpers = ast.parse((base/"reference_misc.py").read_text())
    decoder = ast.parse((base/"reference_decode.py").read_text())
    for tree,names in ((helpers,{"compute_fbank","stft","istft"}),
                       (decoder,{"decode_one_audio_mossformer2_se_48k"})):
        functions = [node for node in tree.body if isinstance(node,ast.FunctionDef) and node.name in names]
        if len(functions) != len(names):
            raise ValueError("pinned upstream reference function set is incomplete")
        exec(compile(ast.Module(body=functions,type_ignores=[]),"pinned-upstream-reference","exec"),namespace)
    model = load_model(base,device)
    args = SimpleNamespace(sampling_rate=48000,win_len=1920,win_inc=384,fft_len=1920,
                           num_mels=60,win_type="hamming",one_time_decode_length=100,decode_window=4)
    outputs=[]
    for channel in range(audio.shape[1]):
        torch.manual_seed(0)
        count = len(audio)
        padded_length = max(1920,math.ceil(max(0,count-1920)/384)*384+1920)
        padded = np.pad(audio[:,channel],(0,padded_length-count))
        with torch.inference_mode():
            result=namespace["decode_one_audio_mossformer2_se_48k"](model,device,padded[None,:],args)
        outputs.append(np.asarray(result).reshape(-1)[:count])
    return np.stack(outputs,axis=1)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model-manifest",type=Path,required=True)
    parser.add_argument("--input",type=Path,required=True)
    parser.add_argument("--output",type=Path,required=True)
    parser.add_argument("--threads",type=int,default=4)
    parser.add_argument("--device",choices=("cpu","cuda"),default="cpu")
    parser.add_argument("--overlap",type=float,default=0.75)
    args=parser.parse_args()
    if args.output.exists() or not 1<=args.threads<=8 or not 0<=args.overlap<=0.875:
        parser.error("output must not exist; threads1..8; overlap0..0.875")
    raw=json.loads(args.model_manifest.read_text())
    manifest=validate_manifest(args.model_manifest,raw["operation"])
    check_packages(manifest)
    for key in ("OMP_NUM_THREADS","MKL_NUM_THREADS","OPENBLAS_NUM_THREADS"):
        os.environ[key]=str(args.threads)
    deny_network()
    import numpy as np
    import soundfile as sf
    import torch
    from scipy.signal import resample_poly
    info=sf.info(args.input)
    if info.subtype!="FLOAT" or info.channels not in (1,2) or not 0<info.frames<=40*info.samplerate:
        parser.error("reference requires nonempty float32 mono/stereo WAV <=40seconds")
    audio,rate=sf.read(args.input,dtype="float32",always_2d=True)
    model_rate=44100 if manifest["operation"]=="separate" else 48000
    divisor=math.gcd(rate,model_rate)
    model_audio=resample_poly(audio,model_rate//divisor,rate//divisor,axis=0).astype("float32") if rate!=model_rate else audio
    torch.set_num_threads(args.threads)
    torch.set_num_interop_threads(1)
    torch.manual_seed(0)
    torch.backends.cuda.matmul.allow_tf32=False
    torch.backends.cudnn.allow_tf32=False
    base=args.model_manifest.resolve().parent
    with redirect_stdout(sys.stderr):
        if manifest["id"]=="MossFormer2_SE_48K":
            result=upstream_moss(base,model_audio,args.device)
        elif manifest["id"]=="melband_roformer_kim":
            from unblend import Separator
            from unblend.repo import ModelRepository
            model=ModelRepository(metadata_path=base/"registry.json",extra_models=[]).get_model("melband_roformer_kim")
            separator=Separator(model,device=args.device,dtype=torch.float32,compile=False,chunk_batch_size=1,custom_kernels=False)
            with torch.inference_mode():
                result=separator.separate((torch.from_numpy(np.ascontiguousarray(model_audio.T)),44100),shifts=0,
                    split_overlap=args.overlap,seed=0).sources["vocals"].detach().cpu().numpy().T
        else:
            parser.error("independent reference is implemented for MossFormer2 and Kim, not other models")
    if rate!=model_rate:
        result=resample_poly(result,rate//divisor,model_rate//divisor,axis=0).astype("float32")
    result=result[:len(audio)]
    if result.shape!=audio.shape or not np.isfinite(result).all():
        raise ValueError("reference changed shape or produced nonfinite audio")
    sf.write(args.output,result,rate,format="WAV",subtype="FLOAT")
    print(json.dumps({"output":str(args.output.resolve()),"sha256":digest(args.output),"source_sha256":digest(args.input),
                      "manifest_sha256":digest(args.model_manifest),"command":sys.argv,"frames":len(result),"rate":rate}))

if __name__=="__main__":
    main()
