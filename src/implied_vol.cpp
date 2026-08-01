#include "qr/implied_vol.hpp"

#include <cmath>

#include "qr/black_scholes.hpp"
#include "qr/timer.hpp"

namespace qr {

namespace {
constexpr int NEWTON_ITERS = 8;
constexpr double PRICE_TOL = 1e-8;     // absolute, in price units
constexpr double VOL_MIN = 0.005;
constexpr double VOL_MAX = 5.0;
}  // namespace

// The hot loop of the whole benchmark. Per quote per iteration:
// 2x erf, 2x exp, 1x log, sqrt, divides. Fixed iteration count and
// branchless bookkeeping keep the work identical across compilers and the
// loop body free of early exits, so it is auto-vectorizable in principle
// (whether a compiler CAN vectorize it comes down to its vector math
// library: SVML has vector erf, glibc's libmvec does not).
IvResult invert_iv(const QuoteBatch& q) {
    const std::size_t n = q.size();
    IvResult out;
    out.iv.resize(n);
    out.converged.assign(n, 0);

    const double* __restrict spot = q.spot.data();
    const double* __restrict strike = q.strike.data();
    const double* __restrict ttm = q.ttm.data();
    const double* __restrict mid = q.mid.data();
    const uint8_t* __restrict is_call = q.is_call.data();
    double* __restrict iv = out.iv.data();
    uint8_t* __restrict conv = out.converged.data();
    const double r = q.rate;

    Timer t;
    for (std::size_t i = 0; i < n; ++i) {
        const double s = spot[i], k = strike[i], T = ttm[i], px = mid[i];
        const double sign = is_call[i] ? 1.0 : -1.0;
        // Brenner-Subrahmanyam-style seed, clamped to a sane band.
        double v = std::sqrt(2.0 * M_PI / T) * px / s;
        v = v < 0.05 ? 0.05 : (v > 2.0 ? 2.0 : v);
        double done = 0.0;   // 1.0 once |price error| < tol; freezes updates
        for (int it = 0; it < NEWTON_ITERS; ++it) {
            const D12 d = d12(s, k, T, r, v);
            const double price =
                sign * (s * norm_cdf(sign * d.d1) - k * std::exp(-r * T) * norm_cdf(sign * d.d2));
            const double vega = s * norm_pdf(d.d1) * std::sqrt(T);
            const double err = price - px;
            done = done + (1.0 - done) * (std::fabs(err) < PRICE_TOL ? 1.0 : 0.0);
            const double safe_vega = vega > 1e-12 ? vega : 1e-12;
            double step = err / safe_vega;
            step = step > 0.5 ? 0.5 : (step < -0.5 ? -0.5 : step);
            v -= (1.0 - done) * step;
            v = v < VOL_MIN ? VOL_MIN : (v > VOL_MAX ? VOL_MAX : v);
        }
        iv[i] = v;
        // Final residual check (not the frozen flag): counts genuine converg.
        const double final_err = bs_price(sign, s, k, T, r, v) - px;
        conv[i] = std::fabs(final_err) < 1e-6 * (px > 1.0 ? px : 1.0) ? 1 : 0;
    }
    out.timing = {t.ns(), n};
    return out;
}

}  // namespace qr
