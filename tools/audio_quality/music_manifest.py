"""Music corpus identity contract shared by preparation and evaluation.

Directory numbering is presentation only. Diagnostic membership is explicitly
bound to source content hashes, never inferred from an ordinal or a basename.
"""
from pathlib import Path
import re


def music_records(manifest, root):
    """Validate records and directory membership; callers must verify loaded audio."""
    if not isinstance(manifest, dict) or manifest.get("schema") != 2:
        raise ValueError("music manifest schema 2 required; re-prepare with source/hash/diagnostic_only and prepared float identities")
    records = manifest.get("records")
    if not isinstance(records, list) or not records:
        raise ValueError("music manifest requires source identity records")
    by_directory = {}
    by_hash = {}
    for record in records:
        if not isinstance(record, dict):
            raise ValueError("music manifest records must be objects")
        directory = record.get("directory")
        source = record.get("source")
        checksum = record.get("sha256")
        diagnostic = record.get("diagnostic_only")
        if (not isinstance(directory, str) or not re.fullmatch(r"track-\d+", directory)
                or not isinstance(source, str) or not source
                or not isinstance(checksum, str) or not re.fullmatch(r"[a-f0-9]{64}", checksum)
                or type(diagnostic) is not bool):
            raise ValueError("music records require track directory, source, SHA256 and explicit diagnostic_only boolean")
        prepared = record.get("float")
        if not isinstance(prepared, dict) or set(prepared) != {"mixture", "vocals", "accompaniment"}:
            raise ValueError("music records require all three prepared float identities; re-prepare corpus")
        for stem, item in prepared.items():
            if (not isinstance(item, dict) or not isinstance(item.get("sha256"), str)
                    or not re.fullmatch(r"[a-f0-9]{64}", item["sha256"])
                    or type(item.get("bytes")) is not int or item["bytes"] <= 0):
                raise ValueError("music records require prepared float SHA256/bytes: " + stem)
        if directory in by_directory:
            raise ValueError("duplicate music manifest directory: " + directory)
        if checksum in by_hash and by_hash[checksum] != diagnostic:
            raise ValueError("conflicting diagnostic membership for source SHA256: " + checksum)
        by_hash[checksum] = diagnostic
        by_directory[directory] = record
    folders = {folder.name for folder in Path(root).glob("track-*") if folder.is_dir()}
    if folders != set(by_directory):
        raise ValueError("music manifest directories differ from prepared track directories")
    return [(Path(root) / directory, by_directory[directory]) for directory in sorted(by_directory)]
