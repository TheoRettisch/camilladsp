"""Reference arithmetic only; this does not execute CamillaDSP/Rust."""
from itertools import permutations, product
from math import isclose, sqrt


def aggregate(samples, mode):
    if not samples:
        raise ValueError("At least one resolved monitor channel is required")
    if mode == "Sum":
        return abs(sum(samples))
    if mode == "Max":
        return max(abs(x) for x in samples)
    if mode == "Rms":
        return sqrt(sum(x * x for x in samples) / len(samples))
    raise ValueError(mode)


cases = [
    ([0.5], (0.5, 0.5, 0.5)),
    ([-0.5], (0.5, 0.5, 0.5)),
    ([0.5, 0.0], (0.5, 0.5, 0.5 / sqrt(2))),
    ([0.0, 0.5], (0.5, 0.5, 0.5 / sqrt(2))),
    ([0.5, 0.5], (1.0, 0.5, 0.5)),
    ([0.5, -0.5], (0.0, 0.5, 0.5)),
    ([0.0, 0.0], (0.0, 0.0, 0.0)),
    ([0.5] * 4, (2.0, 0.5, 0.5)),
    ([0.5, -0.25], (0.25, 0.5, sqrt(0.15625))),
]
checks = 0
for samples, expected in cases:
    for mode, value in zip(("Sum", "Max", "Rms"), expected):
        assert isclose(aggregate(samples, mode), value, rel_tol=1e-12, abs_tol=1e-12)
        checks += 1

samples = [0.5, 0.1, 0.3, 0.25]
for mode in ("Max", "Rms"):
    expected = aggregate(samples, mode)
    for signs in product((-1, 1), repeat=len(samples)):
        actual = aggregate([x * sign for x, sign in zip(samples, signs)], mode)
        assert isclose(actual, expected, rel_tol=1e-12)
        checks += 1
    for ordered in permutations(samples):
        assert isclose(aggregate(ordered, mode), expected, rel_tol=1e-12)
        checks += 1
print(f"{checks} reference arithmetic checks passed (not CamillaDSP integration tests).")
