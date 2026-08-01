#pragma once
#include <vector>

#include "qr/types.hpp"

namespace qr {

struct IvResult {
    std::vector<double> iv;
    std::vector<uint8_t> converged;
    StageTiming timing;
};

// Newton-Raphson implied vol with analytic vega. Fixed iteration count and
// branchless convergence tracking so every build does identical work.
IvResult invert_iv(const QuoteBatch& q);

}  // namespace qr
