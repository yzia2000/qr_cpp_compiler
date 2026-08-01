| variant | compiler | IV med (ms) | IV Mq/s | SVI med (ms) | greeks med (ms) | total (ms) | vs gcc-strict |
|---|---|---|---|---|---|---|---|
| gcc-strict | GNU 13.3.0 | 16316 | 1.83 | 10928 | 2983 | 30226 | 1.00x |
| gcc-fast | GNU 13.3.0 | 11842 | 2.52 | 10921 | 2837 | 25600 | 1.18x |
| clang-strict | Clang 18.1.8 | 17061 | 1.75 | 6559 | 3021 | 26640 | 1.13x |
| clang-fast | Clang 18.1.8 | 15574 | 1.92 | 6278 | 2843 | 24695 | 1.22x |
| icpx-strict | IntelLLVM 2025.3.3 | 15830 | 1.89 | 5107 | 2982 | 23919 | 1.26x |
| icpx-fast | IntelLLVM 2025.3.3 | 12672 | 2.36 | 2802 | 850 | 16323 | 1.85x |
