#include <cstdio>
#include <cstdlib>
#include <string>

#include "qr/greeks.hpp"
#include "qr/implied_vol.hpp"
#include "qr/pcap_reader.hpp"
#include "qr/svi.hpp"
#include "qr/timer.hpp"

// Standalone pipeline runner for development and pure-C++ benchmarking.
// The benchmarked entry point is the nanobind module; this must stay
// behaviorally identical to it.
int main(int argc, char** argv) {
    if (argc < 2) {
        std::fprintf(stderr, "usage: %s <pcap> [reps]\n", argv[0]);
        return 2;
    }
    const std::string path = argv[1];
    const int reps = argc > 2 ? std::atoi(argv[2]) : 1;

    qr::Timer tp;
    qr::QuoteBatch q = qr::read_pcap(path);
    std::printf("parse: %.1f ms, %zu quotes\n", tp.ns() / 1e6, q.size());

    for (int rep = 0; rep < reps; ++rep) {
        qr::IvResult iv = qr::invert_iv(q);
        std::size_t nconv = 0;
        double iv_sum = 0.0;
        for (std::size_t i = 0; i < q.size(); ++i) {
            nconv += iv.converged[i];
            iv_sum += iv.iv[i];
        }
        qr::SviFitResult svi = qr::fit_svi(q, iv.iv, iv.converged);
        double rmse_sum = 0.0;
        int fitted = 0;
        for (const auto& p : svi.params)
            if (p.iters >= 0) { rmse_sum += p.rmse; fitted++; }
        qr::GreeksResult g = qr::compute_greeks(q, svi.params);
        double delta_sum = 0.0;
        for (std::size_t i = 0; i < q.size(); ++i) delta_sum += g.delta[i];

        std::printf(
            "rep %d: iv %.1f ms (%.2f Mq/s, conv %.4f%%, mean_iv %.6f) | "
            "svi %.1f ms (%d slices, mean_rmse %.3e) | greeks %.1f ms (mean_delta %.6f)\n",
            rep, iv.timing.ns / 1e6, q.size() / (iv.timing.ns / 1e3),
            100.0 * nconv / q.size(), iv_sum / q.size(),
            svi.timing.ns / 1e6, fitted, rmse_sum / (fitted ? fitted : 1),
            g.timing.ns / 1e6, delta_sum / q.size());
    }
    return 0;
}
