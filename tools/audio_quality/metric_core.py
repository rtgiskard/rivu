"""Shared, JSON-safe numerical semantics for paired audio evidence.

Undefined scores/gains are None; exact positive-energy reconstruction is
"infinity". No energy floor changes the underlying SNR or SI-SDR formula.
"""
import math
import statistics


def db_ratio(numerator, denominator):
    if numerator == 0:
        return None
    if denominator == 0:
        return "infinity"
    # Subtract logs rather than dividing: finite energy ratios can overflow.
    return 10 * (math.log10(numerator) - math.log10(denominator))


def projection_metrics(reference, estimate):
    import numpy as np
    ref = reference - reference.mean()
    est = estimate - estimate.mean()
    energy = float(np.dot(ref, ref))
    if not energy:
        return None, None
    gain = float(np.dot(ref, est) / energy)
    target = gain * ref
    residual = est - target
    return db_ratio(float(np.dot(target, target)), float(np.dot(residual, residual))), gain


def score_difference(output, baseline):
    """Subtract extended-real dB scores without inventing infinity - infinity."""
    if output is None or baseline is None or output == baseline == "infinity":
        return None
    if output == "infinity":
        return "infinity"
    if baseline == "infinity":
        return "-infinity"
    return output - baseline


def score_summary(scores):
    """Ignore undefined entries; retain signed infinities in ordering/median.

    A median centered between opposite infinities is undefined, not zero/NaN.
    Counts keep missing channels visible rather than treating them as zero.
    """
    values = [float(score) for score in scores if score is not None]
    def encode(value):
        if math.isnan(value):
            return None
        if math.isinf(value):
            return "infinity" if value > 0 else "-infinity"
        return value
    return {
        "median": encode(statistics.median(values)) if values else None,
        "min": encode(min(values)) if values else None,
        "max": encode(max(values)) if values else None,
        "defined_count": len(values),
        "undefined_count": len(scores) - len(values),
        "positive_infinity_count": values.count(math.inf),
        "negative_infinity_count": values.count(-math.inf),
    }
