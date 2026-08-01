#include "qr/svi.hpp"

#include <algorithm>
#include <cmath>
#include <cstring>

#include "qr/timer.hpp"

namespace qr {
namespace {

constexpr int MAX_LM_ITERS = 50;
constexpr double LM_TOL = 1e-12;       // relative SSE improvement stop

// Cholesky solve of the 5x5 normal equations, in-place, no pivoting.
// Returns false if the matrix is not positive definite.
bool solve5(double A[5][5], double b[5], double x[5]) {
    double L[5][5] = {};
    for (int i = 0; i < 5; ++i) {
        for (int j = 0; j <= i; ++j) {
            double s = A[i][j];
            for (int p = 0; p < j; ++p) s -= L[i][p] * L[j][p];
            if (i == j) {
                if (s <= 0.0) return false;
                L[i][i] = std::sqrt(s);
            } else {
                L[i][j] = s / L[j][j];
            }
        }
    }
    double y[5];
    for (int i = 0; i < 5; ++i) {
        double s = b[i];
        for (int p = 0; p < i; ++p) s -= L[i][p] * y[p];
        y[i] = s / L[i][i];
    }
    for (int i = 4; i >= 0; --i) {
        double s = y[i];
        for (int p = i + 1; p < 5; ++p) s -= L[p][i] * x[p];
        x[i] = s / L[i][i];
    }
    return true;
}

struct Slice {
    std::vector<double> k;   // log-moneyness
    std::vector<double> w;   // observed total variance
};

void clamp_params(double p[5]) {
    p[1] = std::max(p[1], 1e-6);                      // b > 0
    p[2] = std::clamp(p[2], -0.999, 0.999);           // |rho| < 1
    p[4] = std::max(p[4], 1e-4);                      // sigma > 0
    // keep min total variance positive: a + b*sigma*sqrt(1-rho^2) > 0
    const double min_w = p[0] + p[1] * p[4] * std::sqrt(1.0 - p[2] * p[2]);
    if (min_w < 1e-8) p[0] += 1e-8 - min_w;
}

// SSE plus (optionally) J^T J and J^T r accumulation — the vectorizable hot
// loop of this stage: pure arithmetic + sqrt, no transcendental calls.
double sse_and_normal_eqs(const Slice& sl, const double p[5],
                          double JtJ[5][5], double Jtr[5], bool accumulate) {
    const double a = p[0], b = p[1], rho = p[2], m = p[3], sg = p[4];
    const std::size_t n = sl.k.size();
    const double* __restrict kk = sl.k.data();
    const double* __restrict ww = sl.w.data();
    double sse = 0.0;
    if (accumulate) {
        std::memset(JtJ, 0, 25 * sizeof(double));
        std::memset(Jtr, 0, 5 * sizeof(double));
    }
    for (std::size_t i = 0; i < n; ++i) {
        const double d = kk[i] - m;
        const double s = std::sqrt(d * d + sg * sg);
        const double res = a + b * (rho * d + s) - ww[i];
        sse += res * res;
        if (accumulate) {
            const double J0 = 1.0;
            const double J1 = rho * d + s;
            const double J2 = b * d;
            const double J3 = -b * (rho + d / s);
            const double J4 = b * sg / s;
            const double J[5] = {J0, J1, J2, J3, J4};
            for (int r_ = 0; r_ < 5; ++r_) {
                Jtr[r_] += J[r_] * res;
                for (int c = 0; c <= r_; ++c) JtJ[r_][c] += J[r_] * J[c];
            }
        }
    }
    return sse;
}

SviParams fit_slice(const Slice& sl) {
    SviParams out{};
    out.n_quotes = (uint32_t)sl.k.size();
    if (sl.k.size() < 8) { out.iters = -1; return out; }

    // Moment-based init.
    double wmin = 1e300, wsum = 0.0, kabs = 0.0;
    for (std::size_t i = 0; i < sl.k.size(); ++i) {
        wmin = std::min(wmin, sl.w[i]);
        wsum += sl.w[i];
        kabs += std::fabs(sl.k[i]);
    }
    const double wmean = wsum / sl.k.size();
    kabs /= sl.k.size();
    double p[5] = {0.9 * wmin, std::max((wmean - wmin) / std::max(kabs, 0.05), 1e-4),
                   -0.5, 0.0, 0.15};
    clamp_params(p);

    double JtJ[5][5], Jtr[5];
    double sse = sse_and_normal_eqs(sl, p, JtJ, Jtr, true);
    double lambda = 1e-3;
    int it = 0;
    for (; it < MAX_LM_ITERS; ++it) {
        double A[5][5];
        for (int r_ = 0; r_ < 5; ++r_) {
            for (int c = 0; c <= r_; ++c) A[r_][c] = A[c][r_] = JtJ[r_][c];
            A[r_][r_] *= 1.0 + lambda;
        }
        double neg_g[5], dp[5];
        for (int r_ = 0; r_ < 5; ++r_) neg_g[r_] = -Jtr[r_];
        if (!solve5(A, neg_g, dp)) { lambda *= 3.0; continue; }
        double cand[5];
        for (int r_ = 0; r_ < 5; ++r_) cand[r_] = p[r_] + dp[r_];
        clamp_params(cand);
        const double cand_sse = sse_and_normal_eqs(sl, cand, JtJ, Jtr, false);
        if (cand_sse < sse) {
            const double rel = (sse - cand_sse) / (sse + 1e-300);
            std::memcpy(p, cand, sizeof(cand));
            sse = cand_sse;
            lambda = std::max(lambda / 3.0, 1e-9);
            sse_and_normal_eqs(sl, p, JtJ, Jtr, true);   // refresh at new point
            if (rel < LM_TOL) break;
        } else {
            lambda *= 3.0;
            if (lambda > 1e8) break;
        }
    }
    out.a = p[0]; out.b = p[1]; out.rho = p[2]; out.m = p[3]; out.sigma = p[4];
    out.rmse = std::sqrt(sse / sl.k.size());
    out.iters = it;
    return out;
}

}  // namespace

SviFitResult fit_svi(const QuoteBatch& q, const std::vector<double>& iv,
                     const std::vector<uint8_t>& converged) {
    Timer t;
    // Bucket quotes into slices (uid, expiry).
    std::vector<Slice> slices(N_SLICES);
    std::vector<uint32_t> counts(N_SLICES, 0);
    const std::size_t n = q.size();
    for (std::size_t i = 0; i < n; ++i)
        counts[q.underlying[i] * N_EXPIRIES + q.expiry_idx[i]]++;
    for (int s = 0; s < N_SLICES; ++s) {
        slices[s].k.reserve(counts[s]);
        slices[s].w.reserve(counts[s]);
    }
    for (std::size_t i = 0; i < n; ++i) {
        if (!converged[i]) continue;
        const int s = q.underlying[i] * N_EXPIRIES + q.expiry_idx[i];
        const double T = q.ttm[i];
        const double fwd = q.spot[i] * std::exp(q.rate * T);
        slices[s].k.push_back(std::log(q.strike[i] / fwd));
        slices[s].w.push_back(iv[i] * iv[i] * T);
    }

    SviFitResult out;
    out.params.resize(N_SLICES);
    for (int s = 0; s < N_SLICES; ++s) out.params[s] = fit_slice(slices[s]);
    out.timing = {t.ns(), n};
    return out;
}

}  // namespace qr
