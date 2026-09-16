from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path

import numpy as np


SCRIPT = Path(__file__).with_name("fit_moe_distribution.py")
SPEC = importlib.util.spec_from_file_location("fit_moe_distribution", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
fit = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(fit)


class FitMoeDistributionTests(unittest.TestCase):
    def test_beta_rank_fit_recovers_known_law(self) -> None:
        _, target = fit.solve_intercept(128, 4, 0.47, 2.41)
        coefficients, fitted = fit.fit_beta_rank(target)
        self.assertAlmostEqual(float(coefficients[1]), 0.47, places=8)
        self.assertAlmostEqual(float(coefficients[2]), 2.41, places=8)
        self.assertLess(float(np.max(np.abs(fitted - target))), 1e-9)
        self.assertAlmostEqual(float(fitted.sum()), 4.0, places=8)

    def test_generator_is_prefix_stable_and_routes_are_unique(self) -> None:
        _, probabilities = fit.solve_intercept(30, 5, 0.53, 2.2)
        short = fit.generate_routes(
            probabilities, 7, 5, 0x105, "learned", 3
        )
        long = fit.generate_routes(
            probabilities, 32, 5, 0x105, "learned", 3
        )
        np.testing.assert_array_equal(short, long[:7])
        self.assertTrue(np.all(np.apply_along_axis(lambda row: len(set(row)) == 5, 1, long)))
        self.assertTrue(np.all((0 <= long) & (long < 30)))
        other_source = fit.generate_routes(
            probabilities, 7, 5, 0x105, "learned", 3, source_rank=1
        )
        self.assertFalse(np.array_equal(short, other_source))

    def test_iid_control_preserves_token_routes_and_layer_marginals(self) -> None:
        routes = np.arange(4 * 2 * 3 * 2).reshape(4, 2, 3, 2) % 17
        control = fit.shuffled_token_control(routes)
        for layer in range(routes.shape[1]):
            before = sorted(map(tuple, routes[:, layer].reshape(-1, 2)))
            after = sorted(map(tuple, control[:, layer].reshape(-1, 2)))
            self.assertEqual(before, after)

    def test_metric_ratio_handles_zero_overlap(self) -> None:
        self.assertEqual(fit.metric_ratio(0.0, 0.0), 1.0)
        self.assertTrue(np.isnan(fit.metric_ratio(1.0, 0.0)))


if __name__ == "__main__":
    unittest.main()
