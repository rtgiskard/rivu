from pathlib import Path


def synthetic_cases(root, manifest):
    """Resolve prepared inputs against this copy of the corpus."""
    if manifest.get("schema") != 2:
        raise ValueError("synthetic manifest schema 2 required; re-prepare corpus")
    root = root.resolve()
    for case in (manifest["reference"], *manifest["cases"]):
        relative = Path(case["path"])
        path = (root / relative).resolve()
        if relative.is_absolute() or not path.is_relative_to(root):
            raise ValueError(f"input is outside the synthetic corpus: {relative}")
        yield {**case, "path": path}
