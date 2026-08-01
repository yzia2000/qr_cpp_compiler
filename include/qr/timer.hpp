#pragma once
#include <chrono>

namespace qr {

class Timer {
    std::chrono::steady_clock::time_point start_ = std::chrono::steady_clock::now();
public:
    long long ns() const {
        return std::chrono::duration_cast<std::chrono::nanoseconds>(
                   std::chrono::steady_clock::now() - start_).count();
    }
};

}  // namespace qr
