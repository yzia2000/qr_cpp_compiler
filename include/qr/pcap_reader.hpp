#pragma once
#include <string>

#include "qr/types.hpp"

namespace qr {

// Parses the synthetic quote-feed pcap (see python/gen_pcap.py for the wire
// format) into a SoA batch. mmap-based; no libpcap dependency.
// Throws std::runtime_error on malformed input.
QuoteBatch read_pcap(const std::string& path);

}  // namespace qr
