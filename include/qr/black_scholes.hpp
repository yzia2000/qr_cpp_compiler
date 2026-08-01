#pragma once
#include <cmath>

// Header-only Black-Scholes helpers, inlined into every hot loop.
// Deliberately expressed with std::erf/std::exp/std::log/std::sqrt: the whole
// benchmark hinges on how each compiler treats these transcendentals
// (SVML vectorization vs scalar libm calls).

namespace qr {

inline constexpr double INV_SQRT2 = 0.7071067811865475244;
inline constexpr double INV_SQRT2PI = 0.3989422804014326779;

inline double norm_cdf(double x) { return 0.5 * (1.0 + std::erf(x * INV_SQRT2)); }
inline double norm_pdf(double x) { return INV_SQRT2PI * std::exp(-0.5 * x * x); }

struct D12 { double d1, d2; };

inline D12 d12(double s, double k, double t, double r, double vol) {
    const double sqt = std::sqrt(t);
    const double d1 = (std::log(s / k) + (r + 0.5 * vol * vol) * t) / (vol * sqt);
    return {d1, d1 - vol * sqt};
}

// sign = +1 for calls, -1 for puts
inline double bs_price(double sign, double s, double k, double t, double r, double vol) {
    const D12 d = d12(s, k, t, r, vol);
    return sign * (s * norm_cdf(sign * d.d1) - k * std::exp(-r * t) * norm_cdf(sign * d.d2));
}

inline double bs_vega(double s, double k, double t, double r, double vol) {
    const D12 d = d12(s, k, t, r, vol);
    return s * norm_pdf(d.d1) * std::sqrt(t);
}

}  // namespace qr
