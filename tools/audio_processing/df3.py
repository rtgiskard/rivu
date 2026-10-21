"""Explicit DF3 candidate preparation, with factual unknown weight-license status."""
import shutil
from pathlib import Path
import zipfile

REVISION = "978576aa8400552a4ce9730838c635aa30db5e61"
BASE = f"https://raw.githubusercontent.com/Rikorose/DeepFilterNet/{REVISION}/"
WEIGHT_URL = BASE + "models/DeepFilterNet3.zip"
WEIGHT_SIZE = 7986207
WEIGHT_GIT_BLOB = "ea53bac00d064378451d0201d573f44f05f1d746"
WEIGHT_SHA256 = "49c52edc8947ae1f9bf50d81530beaf3a2c3245aeaf34b6f31ff535cd22284d2"


def prepare(args, target, download):
    from worker import digest
    archive_path = target / "upstream-model.zip"
    if args.download:
        download(WEIGHT_URL, archive_path, WEIGHT_SIZE)
    else:
        if args.weights.stat().st_size > 64 * 1024**2:
            raise ValueError("DF3 supplied archive exceeds 64 MiB")
        shutil.copyfile(args.weights, archive_path)
    archive_digest = digest(archive_path)
    if args.download and (archive_path.stat().st_size != WEIGHT_SIZE or archive_digest != WEIGHT_SHA256):
        raise ValueError("DF3 artifact differs from pinned upstream archive SHA256")
    with zipfile.ZipFile(archive_path) as archive:
        if sum(member.file_size for member in archive.infolist()) > 64 * 1024**2:
            raise ValueError("DF3 archive expands beyond 64 MiB")
        for member in archive.infolist():
            name = Path(member.filename)
            if name.is_absolute() or ".." in name.parts or (member.external_attr >> 16) & 0o170000 == 0o120000:
                raise ValueError("unsafe archive entry")
            if not name.parts or name.parts[0] != "DeepFilterNet3":
                raise ValueError("DF3 archive must contain only DeepFilterNet3/ model assets")
        archive.extractall(target)
    if not (target / "DeepFilterNet3/config.ini").is_file():
        raise ValueError("expected DeepFilterNet3/config.ini in archive")
    for name in ("README.md", "LICENSE", "LICENSE-MIT", "LICENSE-APACHE"):
        download(BASE + name, target / ("upstream-" + name.replace(".md", ".txt")), 65536)
    if args.license_evidence:
        shutil.copyfile(args.license_evidence, target / "supplied-weight-license.txt")
    (target / "weight-license.txt").write_text(
        "DeepFilterNet3 pretrained weight license: unresolved; no weight grant inferred from source-code licenses.\n"
        "Upstream source is MIT/Apache-2.0; original notices are retained separately.\n"
        "https://github.com/Rikorose/DeepFilterNet/issues/697\n"
        "This is explicit user-authorized preparation of a supplied/public candidate, not a redistribution grant.\n"
        + ("Additional user-supplied evidence is retained verbatim in supplied-weight-license.txt.\n" if args.license_evidence else ""))
    return {"version":1,"operation":"speech","id":"DeepFilterNet3",
            "packages":{"deepfilternet":"0.5.6","torch":"2.1.2","numpy":"1.26.4","scipy":"1.11.4","soundfile":"0.12.1"},
            "provenance":{"weights":WEIGHT_URL if args.download else "user-supplied local archive",
                          "weights_sha256":archive_digest,"upstream_weight_git_blob":WEIGHT_GIT_BLOB if args.download else None,
                          "code":f"https://github.com/Rikorose/DeepFilterNet/tree/{REVISION}","code_license":"MIT OR Apache-2.0",
                          "weights_license":"unresolved; no grant inferred", "license_source":BASE + "README.md",
                          "public_weight_license_status":"unresolved https://github.com/Rikorose/DeepFilterNet/issues/697",
                          "selection":"explicit user preparation; license metadata is not a candidate gate"}}
