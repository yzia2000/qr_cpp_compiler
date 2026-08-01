#include "qr/greeks.hpp"

#include <cmath>

#include "qr/black_scholes.hpp"
#include "qr/svi.hpp"
#include "qr/timer.hpp"

namespace qr {

GreeksResult compute_greeks(const QuoteBatch& q, const std::vector<SviParams>& svi) {
    const std::size_t n = q.size();
    GreeksResult out;
    out.delta.resize(n);
    out.gamma.resize(n);
    out.vega.resize(n);
    out.theta.resize(n);

    // Flatten fitted params into per-quote arrays first (gather stage), so the
    // math loop below is a pure flat sweep the compiler can vectorize.
    std::vector<double> va(n), vb(n), vrho(n), vm(n), vsg(n);
    for (std::size_t i = 0; i < n; ++i) {
        const SviParams& p = svi[q.underlying[i] * N_EXPIRIES + q.expiry_idx[i]];
        va[i] = p.a; vb[i] = p.b; vrho[i] = p.rho; vm[i] = p.m; vsg[i] = p.sigma;
    }

    const double* __restrict spot = q.spot.data();
    const double* __restrict strike = q.strike.data();
    const double* __restrict ttm = q.ttm.data();
    const uint8_t* __restrict is_call = q.is_call.data();
    const double r = q.rate;
    double* __restrict delta = out.delta.data();
    double* __restrict gamma = out.gamma.data();
    double* __restrict vega = out.vega.data();
    double* __restrict theta = out.theta.data();

    Timer t;
    for (std::size_t i = 0; i < n; ++i) {
        const double s = spot[i], k = strike[i], T = ttm[i];
        const double fwd = s * std::exp(r * T);
        const double km = std::log(k / fwd);
        const double d0 = km - vm[i];
        const double w = va[i] + vb[i] * (vrho[i] * d0 + std::sqrt(d0 * d0 + vsg[i] * vsg[i]));
        const double vol = std::sqrt(w / T);
        const double sqt = std::sqrt(T);
        const double d1 = (-km + (0.5 * vol * vol) * T) / (vol * sqt);
        const double d2 = d1 - vol * sqt;
        const double pdf1 = norm_pdf(d1);
        const double disc = std::exp(-r * T);
        const double sign = is_call[i] ? 1.0 : -1.0;
        delta[i] = is_call[i] ? norm_cdf(d1) : norm_cdf(d1) - 1.0;
        gamma[i] = pdf1 / (s * vol * sqt);
        vega[i] = s * pdf1 * sqt;
        theta[i] = -0.5 * s * pdf1 * vol / sqt - sign * r * k * disc * norm_cdf(sign * d2);
    }
    out.timing = {t.ns(), n};
    return out;
}

}  // namespace qr
