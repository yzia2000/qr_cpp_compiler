#pragma once
#include <cmath>
#include <vector>

#include "qr/types.hpp"

namespace qr {

inline constexpr int N_UNDERLYINGS = 64;
inline constexpr int N_EXPIRIES = 8;
inline constexpr int N_SLICES = N_UNDERLYINGS * N_EXPIRIES;

struct SviFitResult {
    std::vector<SviParams> params;   // N_SLICES entries, index = uid * N_EXPIRIES + eidx
    StageTiming timing;
};

inline double svi_total_var(const SviParams& p, double k) {
    const double d = k - p.m;
    // sqrt, fma-heavy; fully vectorizable (no transcendental calls)
    return p.a + p.b * (p.rho * d + std::sqrt(d * d + p.sigma * p.sigma));
}

// Levenberg-Marquardt fit of raw-SVI total variance per (underlying, expiry)
// slice, using the IVs produced by invert_iv. Hand-rolled 5x5 Cholesky.
SviFitResult fit_svi(const QuoteBatch& q, const std::vector<double>& iv,
                     const std::vector<uint8_t>& converged);

}  // namespace qr
