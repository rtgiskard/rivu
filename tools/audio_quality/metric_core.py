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


class PreparedProjection:
    """Centered reference statistics owned by one scoring caller.

    The caller keeps the referenced samples unchanged for this object's lifetime.
    """

    def __init__(self, reference):
        import numpy as np
        self.reference = reference
        self.mean = float(np.mean(reference, dtype="float64"))
        self.energy = 0.0
        for start in range(0, len(reference), 65536):
            ref = reference[start:start+65536].astype("float64") - self.mean
            self.energy += float(np.dot(ref, ref))

    def score(self, estimate):
        import numpy as np
        if not self.energy:
            return None, None
        reference = self.reference
        est_mean = float(np.mean(estimate, dtype="float64"))
        product = 0.0
        for start in range(0, len(reference), 65536):
            ref = reference[start:start+65536].astype("float64") - self.mean
            est = estimate[start:start+65536].astype("float64") - est_mean
            product += float(np.dot(ref, est))
        gain = product/self.energy
        target_energy = residual_energy = 0.0
        for start in range(0, len(reference), 65536):
            ref = reference[start:start+65536].astype("float64") - self.mean
            est = estimate[start:start+65536].astype("float64") - est_mean
            target = gain*ref
            residual = est-target
            target_energy += float(np.dot(target, target))
            residual_energy += float(np.dot(residual, residual))
        return db_ratio(target_energy, residual_energy), gain


def projection_metrics(reference, estimate):
    return PreparedProjection(reference).score(estimate)

def reconstruction_metrics(mixture, target, complement):
    """Accumulate exact mixture-minus-stems residuals with bounded scratch."""
    import numpy as np
    energy = peak = 0.0
    for start in range(0, len(mixture), 65536):
        residual = (mixture[start:start+65536].astype("float64")
                    - target[start:start+65536] - complement[start:start+65536])
        energy += float(np.sum(residual**2))
        peak = max(peak, float(np.max(np.abs(residual))))
    return float(np.sqrt(energy/mixture.size)), peak



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
