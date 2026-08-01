#include <nanobind/nanobind.h>
#include <nanobind/ndarray.h>
#include <nanobind/stl/string.h>

#include <memory>
#include <stdexcept>

#include "qr/greeks.hpp"
#include "qr/implied_vol.hpp"
#include "qr/pcap_reader.hpp"
#include "qr/svi.hpp"
#include "qr/timer.hpp"

namespace nb = nanobind;

namespace {

using DArray = nb::ndarray<nb::numpy, const double, nb::ndim<1>>;

// Zero-copy view over a vector owned by a Pipeline (the owner capsule keeps
// the Python Pipeline object alive while views exist).
DArray view(const std::vector<double>& v, nb::handle owner) {
    return DArray(v.data(), {v.size()}, owner);
}

struct Pipeline {
    qr::QuoteBatch batch;
    long long parse_ns = 0;
    qr::IvResult iv_res;
    qr::SviFitResult svi_res;
    qr::GreeksResult greeks_res;
    bool have_iv = false, have_svi = false, have_greeks = false;

    explicit Pipeline(const std::string& path) {
        qr::Timer t;
        batch = qr::read_pcap(path);
        parse_ns = t.ns();
    }

    long long run_iv() {
        iv_res = qr::invert_iv(batch);
        have_iv = true;
        return iv_res.timing.ns;
    }
    long long fit_svi() {
        if (!have_iv) throw std::runtime_error("run_iv() first");
        svi_res = qr::fit_svi(batch, iv_res.iv, iv_res.converged);
        have_svi = true;
        return svi_res.timing.ns;
    }
    long long run_greeks() {
        if (!have_svi) throw std::runtime_error("fit_svi() first");
        greeks_res = qr::compute_greeks(batch, svi_res.params);
        have_greeks = true;
        return greeks_res.timing.ns;
    }
};

}  // namespace

NB_MODULE(qr_pipeline, m) {
    m.def("build_info", []() {
        nb::dict d;
        d["compiler"] = QR_COMPILER_STR;
        d["flags"] = QR_FLAGS_STR;
        d["fast_math"] = (bool)QR_FASTMATH;
        return d;
    });

    nb::class_<Pipeline>(m, "Pipeline")
        .def(nb::init<const std::string&>(), nb::arg("pcap_path"))
        .def_ro("parse_ns", &Pipeline::parse_ns)
        .def_prop_ro("num_quotes", [](const Pipeline& p) { return p.batch.size(); })
        .def("run_iv", &Pipeline::run_iv)
        .def("fit_svi", &Pipeline::fit_svi)
        .def("run_greeks", &Pipeline::run_greeks)
        .def("iv", [](nb::handle_t<Pipeline> h) {
            const Pipeline& p = nb::cast<const Pipeline&>(h);
            if (!p.have_iv) throw std::runtime_error("run_iv() first");
            return view(p.iv_res.iv, h);
        })
        .def("converged", [](nb::handle_t<Pipeline> h) {
            const Pipeline& p = nb::cast<const Pipeline&>(h);
            if (!p.have_iv) throw std::runtime_error("run_iv() first");
            return nb::ndarray<nb::numpy, const uint8_t, nb::ndim<1>>(
                p.iv_res.converged.data(), {p.iv_res.converged.size()}, h);
        })
        .def("svi_params", [](nb::handle_t<Pipeline> h) {
            const Pipeline& p = nb::cast<const Pipeline&>(h);
            if (!p.have_svi) throw std::runtime_error("fit_svi() first");
            // copy into a (N_SLICES, 8) array: a,b,rho,m,sigma,rmse,iters,n
            const auto& ps = p.svi_res.params;
            double* buf = new double[ps.size() * 8];
            for (std::size_t i = 0; i < ps.size(); ++i) {
                buf[i * 8 + 0] = ps[i].a;
                buf[i * 8 + 1] = ps[i].b;
                buf[i * 8 + 2] = ps[i].rho;
                buf[i * 8 + 3] = ps[i].m;
                buf[i * 8 + 4] = ps[i].sigma;
                buf[i * 8 + 5] = ps[i].rmse;
                buf[i * 8 + 6] = ps[i].iters;
                buf[i * 8 + 7] = ps[i].n_quotes;
            }
            nb::capsule owner(buf, [](void* d) noexcept { delete[] (double*)d; });
            return nb::ndarray<nb::numpy, double, nb::ndim<2>>(
                buf, {ps.size(), 8}, owner);
        })
        .def("greeks", [](nb::handle_t<Pipeline> h) {
            const Pipeline& p = nb::cast<const Pipeline&>(h);
            if (!p.have_greeks) throw std::runtime_error("run_greeks() first");
            nb::dict d;
            d["delta"] = view(p.greeks_res.delta, h);
            d["gamma"] = view(p.greeks_res.gamma, h);
            d["vega"] = view(p.greeks_res.vega, h);
            d["theta"] = view(p.greeks_res.theta, h);
            return d;
        });
}
