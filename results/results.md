| variant | compiler | IV med (ms) | IV Mq/s | SVI med (ms) | greeks med (ms) | total (ms) | vs gcc-strict |
|---|---|---|---|---|---|---|---|
| gcc-strict | GNU 13.3.0 | 13087 | 2.28 | 8879 | 2571 | 24536 | 1.00x |
| gcc-fast | GNU 13.3.0 | 9719 | 3.07 | 9494 | 2341 | 21554 | 1.14x |
| gcc-fast-zmm | GNU 13.3.0 | 9813 | 3.04 | 9094 | 2499 | 21406 | 1.15x |
| clang-strict | Clang 18.1.8 | 13971 | 2.14 | 5852 | 2652 | 22475 | 1.09x |
| clang-fast | Clang 18.1.8 | 12877 | 2.32 | 5411 | 2454 | 20742 | 1.18x |
| clang-fast-zmm | Clang 18.1.8 | 12501 | 2.39 | 5461 | 2491 | 20453 | 1.20x |
| icpx-strict | IntelLLVM 2025.3.3 | 13067 | 2.29 | 4333 | 2520 | 19920 | 1.23x |
| icpx-fast | IntelLLVM 2025.3.3 | 10545 | 2.83 | 2385 | 770 | 13700 | 1.79x |
| icpx-fast-zmm | IntelLLVM 2025.3.3 | 12056 | 2.48 | 3039 | 633 | 15728 | 1.56x |
