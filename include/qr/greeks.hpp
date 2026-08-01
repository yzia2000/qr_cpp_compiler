#pragma once
#include <vector>

#include "qr/types.hpp"

namespace qr {

struct GreeksResult {
    std::vector<double> delta, gamma, vega, theta;
    StageTiming timing;
};

// Analytic Black-Scholes greeks for every quote, using its slice's *fitted*
// SVI vol — a second erf/exp/log/sqrt-dense sweep over the full batch.
GreeksResult compute_greeks(const QuoteBatch& q, const std::vector<SviParams>& svi);

}  // namespace qr
