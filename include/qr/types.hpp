#pragma once
#include <cstdint>
#include <vector>

namespace qr {

// Structure-of-arrays quote batch: every hot kernel is a flat loop over
// these, which is what lets the compilers auto-vectorize.
struct QuoteBatch {
    std::vector<double> spot;      // underlying spot
    std::vector<double> strike;
    std::vector<double> ttm;       // time to expiry, years
    std::vector<double> mid;       // (bid+ask)/2
    std::vector<uint8_t> is_call;
    std::vector<uint16_t> underlying;
    std::vector<uint8_t> expiry_idx;
    double rate = 0.0;

    std::size_t size() const { return spot.size(); }
};

struct SviParams {
    double a, b, rho, m, sigma;
    double rmse;         // fit quality, total-variance units
    int iters;
    uint32_t n_quotes;
};

struct StageTiming {
    long long ns;
    std::size_t items;
};

}  // namespace qr
